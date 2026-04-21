//! Wikilink parser for extracting `[[links]]` from Markdown note bodies.
//!
//! Supports two formats:
//! - `[[target-title]]` — plain link
//! - `[[target-title|display text]]` — link with display text
//!
//! The parser extracts each link's target, optional display text, and a
//! surrounding context snippet (~50 chars on each side).

use regex::Regex;
use std::sync::LazyLock;

use crate::types::WikilinkRef;

/// Compiled regex for wikilink extraction.
///
/// Matches `[[target]]` and `[[target|display]]` while rejecting
/// empty targets and nested brackets.
static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]\[|]+)(?:\|([^\]\[]+))?\]\]").unwrap());

/// The number of characters to capture on each side of a wikilink for context.
const CONTEXT_RADIUS: usize = 50;

/// Extract all wikilink references from the given text.
///
/// Returns a `Vec<WikilinkRef>` with the raw target, normalized target
/// (lowercase kebab-case), optional display text, and a context snippet.
pub fn extract_wikilinks(text: &str) -> Vec<WikilinkRef> {
    WIKILINK_RE
        .captures_iter(text)
        .map(|cap| {
            let full_match = cap.get(0).unwrap();
            let target_raw = cap[1].trim().to_string();
            let display_text = cap.get(2).map(|m| m.as_str().trim().to_string());

            let context = extract_context(text, full_match.start(), full_match.end());

            WikilinkRef {
                target_normalized: normalize_to_kebab(&target_raw),
                target: target_raw,
                display_text,
                context,
            }
        })
        .collect()
}

/// Normalize a string to lowercase kebab-case for matching.
///
/// Replaces whitespace and underscores with hyphens, strips non-alphanumeric
/// characters (except hyphens), collapses consecutive hyphens, and trims.
fn normalize_to_kebab(s: &str) -> String {
    let lowered = s.to_lowercase();
    let mut result = String::with_capacity(lowered.len());
    let mut prev_was_hyphen = false;

    for c in lowered.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c);
            prev_was_hyphen = false;
        } else if (c == ' ' || c == '_' || c == '-') && !prev_was_hyphen && !result.is_empty() {
            result.push('-');
            prev_was_hyphen = true;
        }
        // Other characters (including separators when prev was already a hyphen
        // or result is empty) are silently dropped.
    }

    result.trim_end_matches('-').to_string()
}

/// Extract a context snippet around the match at `[start..end]`.
fn extract_context(text: &str, start: usize, end: usize) -> String {
    let ctx_start = start.saturating_sub(CONTEXT_RADIUS);
    let ctx_end = (end + CONTEXT_RADIUS).min(text.len());

    // Ensure we slice on char boundaries.
    let safe_start = text
        .char_indices()
        .rev()
        .find(|&(i, _)| i <= ctx_start)
        .map(|(i, _)| i)
        .unwrap_or(0);

    let safe_end = text
        .char_indices()
        .find(|&(i, _)| i >= ctx_end)
        .map(|(i, _)| i)
        .unwrap_or(text.len());

    let snippet = &text[safe_start..safe_end];

    let mut result = String::new();
    if safe_start > 0 {
        result.push_str("...");
    }
    result.push_str(snippet.trim());
    if safe_end < text.len() {
        result.push_str("...");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_link() {
        let refs = extract_wikilinks("Check out [[simple link]] for details.");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "simple link");
        assert_eq!(refs[0].target_normalized, "simple-link");
        assert_eq!(refs[0].display_text, None);
        assert!(refs[0].context.contains("simple link"));
    }

    #[test]
    fn parse_link_with_display_text() {
        let refs = extract_wikilinks("See [[link target|display text]] here.");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "link target");
        assert_eq!(refs[0].target_normalized, "link-target");
        assert_eq!(refs[0].display_text, Some("display text".to_string()));
    }

    #[test]
    fn parse_multiple_wikilinks() {
        let text = "First [[alpha]] then [[beta|Beta Display]] and finally [[gamma]]";
        let refs = extract_wikilinks(text);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].target, "alpha");
        assert_eq!(refs[1].target, "beta");
        assert_eq!(refs[1].display_text, Some("Beta Display".to_string()));
        assert_eq!(refs[2].target, "gamma");
    }

    #[test]
    fn parse_no_wikilinks() {
        let refs = extract_wikilinks("No links here, just [regular] brackets.");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_empty_target_ignored() {
        // [[]] has nothing between the brackets; regex requires at least one char
        let refs = extract_wikilinks("Empty [[]] link.");
        assert!(refs.is_empty());
    }

    #[test]
    fn parse_nested_brackets_rejected() {
        // Nested brackets like [[ [[inner]] ]] should not match as one link
        let refs = extract_wikilinks("Nested [[ [[inner]] ]] stuff.");
        // The regex should match [[inner]] only, not the outer brackets
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "inner");
    }

    #[test]
    fn normalize_simple() {
        assert_eq!(normalize_to_kebab("Simple Link"), "simple-link");
    }

    #[test]
    fn normalize_underscores() {
        assert_eq!(normalize_to_kebab("under_score_name"), "under-score-name");
    }

    #[test]
    fn normalize_mixed_case_and_special() {
        assert_eq!(normalize_to_kebab("Hello, World!"), "hello-world");
    }

    #[test]
    fn normalize_already_kebab() {
        assert_eq!(normalize_to_kebab("already-kebab"), "already-kebab");
    }

    #[test]
    fn normalize_leading_trailing_spaces() {
        assert_eq!(normalize_to_kebab("  padded  "), "padded");
    }

    #[test]
    fn normalize_consecutive_separators() {
        assert_eq!(normalize_to_kebab("a---b___c   d"), "a-b-c-d");
    }

    #[test]
    fn context_snippet_short_text() {
        let text = "A [[link]] B";
        let refs = extract_wikilinks(text);
        assert_eq!(refs.len(), 1);
        // Short text: full text appears, no ellipsis
        assert_eq!(refs[0].context, "A [[link]] B");
    }

    #[test]
    fn context_snippet_long_text() {
        let prefix = "x".repeat(100);
        let suffix = "y".repeat(100);
        let text = format!("{prefix}[[target]]{suffix}");
        let refs = extract_wikilinks(&text);
        assert_eq!(refs.len(), 1);
        // Should have ellipsis on both sides
        assert!(refs[0].context.starts_with("..."));
        assert!(refs[0].context.ends_with("..."));
        assert!(refs[0].context.contains("[[target]]"));
    }

    #[test]
    fn parse_wikilink_with_whitespace_in_target() {
        let refs = extract_wikilinks("See [[  spaced target  ]] here.");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "spaced target");
        assert_eq!(refs[0].target_normalized, "spaced-target");
    }

    #[test]
    fn parse_adjacent_wikilinks() {
        let refs = extract_wikilinks("[[alpha]][[beta]]");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].target, "alpha");
        assert_eq!(refs[1].target, "beta");
    }

    #[test]
    fn parse_wikilink_at_start_and_end() {
        let refs = extract_wikilinks("[[start]] middle [[end]]");
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].target, "start");
        assert_eq!(refs[1].target, "end");
    }

    #[test]
    fn parse_pipe_with_no_display_text_is_empty_display() {
        // [[target|]] — empty display text after pipe
        let refs = extract_wikilinks("See [[target|]] here.");
        // Regex requires at least one char after pipe, so this should not match
        // with display_text. It should still match the whole thing because the
        // pipe-display group is optional, but the regex won't match an empty
        // capture after the pipe. Let's verify behavior:
        // The regex `[^\]\[]+` requires one or more chars, so `|]]` fails the
        // second group. But the first group `[^\]\[|]+` would match "target"
        // and then `|]]` doesn't match the optional group, so the whole regex
        // fails because `|]]` is not `]]`. Actually let me trace:
        // `\[\[([^\]\[|]+)(?:\|([^\]\[]+))?\]\]` applied to `[[target|]]`
        // Group 1: `target` matches `[^\]\[|]+`
        // Then `(?:\|([^\]\[]+))?` tries to match `|]]`:
        //   `\|` matches `|`, then `([^\]\[]+)` needs 1+ chars not ] or [.
        //   Next char is `]`, so the inner group fails.
        //   The outer `(?:...)?` is optional, so it backtracks.
        // Then `\]\]` tries to match `|]]` — fails because `|` != `]`.
        // So the whole regex fails to match `[[target|]]`.
        assert!(refs.is_empty());
    }
}
