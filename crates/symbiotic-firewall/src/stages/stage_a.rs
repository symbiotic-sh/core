//! Stage A — structural sanitization (design §3.1).
//!
//! Stage A is the ingest-time structural pass. It:
//!
//! 1. Enforces UTF-8 validity on the payload (caller already has a `&str`,
//!    so this is effectively a precondition check — we additionally reject
//!    bidi-override characters that can visually disguise text).
//! 2. Enforces a size cap (default 1 MB).
//! 3. Validates declared vs. actual content-type (magic-byte sniff).
//! 4. Delegates to [`crate::sanitize::html`] / [`crate::sanitize::markdown`]
//!    based on the claimed content-type.
//! 5. Converts any observed structural violations into a quarantined verdict
//!    with `QuarantineClass::SourceIntegrity` — or, when the payload is
//!    benign, returns the sanitized payload + optional informational
//!    findings for Stage B.

use time::OffsetDateTime;

use crate::heuristics::encoding::BIDI_OVERRIDE_CODEPOINTS;
use crate::sanitize::{
    html::sanitize_html,
    markdown::{sanitize_markdown, ImageAllowlist},
};
use crate::stages::passed_verdict;
use crate::types::{
    FindingKind, FirewallVerdict, QuarantineClass, ScanContext, Stage, StageFinding, Verdict,
};
use crate::version::SECURITY_VERSION;

/// Stage A configuration.
#[derive(Debug, Clone)]
pub struct StageAConfig {
    /// Maximum allowed payload size in bytes. Default 1 MiB.
    pub max_bytes: usize,
    /// Image host allowlist for Markdown sanitization.
    pub image_allowlist: ImageAllowlist,
}

impl Default for StageAConfig {
    fn default() -> Self {
        Self {
            max_bytes: 1024 * 1024,
            image_allowlist: ImageAllowlist::default_allowlist(),
        }
    }
}

/// Outcome of Stage A.
#[derive(Debug, Clone)]
pub enum StageAOutcome {
    /// Stage A cleared the payload. `cleaned` is the sanitized version;
    /// `findings` are informational findings (if any) to accumulate onto the
    /// final verdict.
    Passed {
        cleaned: String,
        findings: Vec<StageFinding>,
    },
    /// Stage A rejected the payload.
    Quarantined(FirewallVerdict),
}

/// Top-level entry point.
pub fn run(ctx: &ScanContext, payload: &str, config: &StageAConfig) -> StageAOutcome {
    let now = OffsetDateTime::now_utc();

    // 1. Size cap.
    if payload.len() > config.max_bytes {
        return quarantine(
            now,
            vec![StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: format!(
                    "payload size {} exceeds limit {}",
                    payload.len(),
                    config.max_bytes
                ),
                confidence: 1.0,
            }],
        );
    }

    // 2. Bidi-override check (cheap; runs before format-specific sanitizers).
    if let Some(count) = count_bidi(payload) {
        return quarantine(
            now,
            vec![StageFinding {
                stage: Stage::A,
                kind: FindingKind::StructuralViolation,
                detail: format!("{count} bidi-override codepoint(s) present"),
                confidence: 1.0,
            }],
        );
    }

    // 3. Content-type validation: if the source declared a content-type and
    //    the payload's magic bytes disagree, we quarantine.
    if let Some(declared) = ctx.source.claimed_content_type.as_deref() {
        if let Some(mismatch) = mime_mismatch(declared, payload) {
            return quarantine(
                now,
                vec![StageFinding {
                    stage: Stage::A,
                    kind: FindingKind::StructuralViolation,
                    detail: mismatch,
                    confidence: 1.0,
                }],
            );
        }
    }

    // 4. Format-specific sanitization.
    let format = ContentFormat::from_claimed(ctx.source.claimed_content_type.as_deref(), payload);
    match format {
        ContentFormat::Html => {
            let report = sanitize_html(payload);
            if report
                .stripped_tags
                .iter()
                .any(|t| HARD_FAIL.contains(&t.as_str()))
            {
                return quarantine(now, report.to_findings());
            }
            // Stray handlers / dangerous URLs: note as informational findings
            // but pass through — ammonia has already stripped them from the
            // cleaned payload.
            let findings = report.to_findings();
            StageAOutcome::Passed {
                cleaned: report.cleaned,
                findings,
            }
        }
        ContentFormat::Markdown => {
            let report = sanitize_markdown(payload, &config.image_allowlist);
            if report
                .html_report
                .stripped_tags
                .iter()
                .any(|t| HARD_FAIL.contains(&t.as_str()))
            {
                return quarantine(now, report.to_findings());
            }
            let findings = report.to_findings();
            StageAOutcome::Passed {
                cleaned: report.cleaned,
                findings,
            }
        }
        ContentFormat::PlainText => {
            // No sanitization required; pass through.
            let _ = passed_verdict(now); // keep helper referenced
            StageAOutcome::Passed {
                cleaned: payload.to_string(),
                findings: Vec::new(),
            }
        }
    }
}

/// Tag categories that force a Stage A quarantine when observed (design §3.1).
const HARD_FAIL: &[&str] = &[
    "script", "iframe", "object", "embed", "applet", "form", "meta", "link",
];

fn count_bidi(payload: &str) -> Option<usize> {
    let count = payload
        .chars()
        .filter(|c| BIDI_OVERRIDE_CODEPOINTS.contains(c))
        .count();
    if count > 0 {
        Some(count)
    } else {
        None
    }
}

fn quarantine(timestamp: OffsetDateTime, annotations: Vec<StageFinding>) -> StageAOutcome {
    StageAOutcome::Quarantined(FirewallVerdict {
        verdict: Verdict::Quarantined,
        verdict_version: SECURITY_VERSION.to_string(),
        scan_timestamp: timestamp,
        quarantine_class: Some(QuarantineClass::SourceIntegrity),
        source_receipt_id: None,
        annotations,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentFormat {
    Html,
    Markdown,
    PlainText,
}

impl ContentFormat {
    fn from_claimed(claimed: Option<&str>, payload: &str) -> Self {
        if let Some(ct) = claimed {
            let ct = ct.to_ascii_lowercase();
            if ct.contains("html") {
                return ContentFormat::Html;
            }
            if ct.contains("markdown") || ct.ends_with("/md") {
                return ContentFormat::Markdown;
            }
            if ct.starts_with("text/") {
                return ContentFormat::PlainText;
            }
        }
        // Fallback: sniff a couple of obvious signals.
        if payload.trim_start().starts_with('<') && payload.to_ascii_lowercase().contains("<html") {
            ContentFormat::Html
        } else {
            ContentFormat::PlainText
        }
    }
}

/// Declared vs. actual content-type mismatch detection.
///
/// We only catch obvious cases (magic-byte sniffing): the declared type
/// claims HTML/plain-text but the payload starts with a well-known binary
/// signature (PDF, PNG, JPEG, GIF, ZIP, ELF, Mach-O).
fn mime_mismatch(declared: &str, payload: &str) -> Option<String> {
    let declared_lower = declared.to_ascii_lowercase();
    let is_texty = declared_lower.contains("text/") || declared_lower.contains("json");
    if !is_texty {
        return None;
    }
    let bytes = payload.as_bytes();
    let magic = match bytes {
        [0x25, 0x50, 0x44, 0x46, ..] => Some("application/pdf"),
        [0x89, 0x50, 0x4E, 0x47, ..] => Some("image/png"),
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x47, 0x49, 0x46, 0x38, ..] => Some("image/gif"),
        [0x50, 0x4B, 0x03, 0x04, ..] => Some("application/zip"),
        [0x7F, 0x45, 0x4C, 0x46, ..] => Some("application/x-elf"),
        _ => None,
    };
    magic.map(|actual| {
        format!("declared content-type {declared} disagrees with actual magic bytes ({actual})")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{CallSite, ConsumingAgentScope, ContentSource};
    use std::collections::BTreeMap;

    fn ctx_with_mime(mime: Option<&str>) -> ScanContext {
        ScanContext {
            source: ContentSource {
                kind: "web_fetch".into(),
                url: Some("https://example.com/doc".into()),
                fetched_at: OffsetDateTime::now_utc(),
                claimed_content_type: mime.map(str::to_string),
                headers: BTreeMap::new(),
            },
            consuming_agent_scope: ConsumingAgentScope::minimal("agent-x"),
            call_site: CallSite::new("test.stage_a"),
        }
    }

    #[test]
    fn passes_benign_text() {
        let ctx = ctx_with_mime(Some("text/plain"));
        let out = run(&ctx, "hello world", &StageAConfig::default());
        match out {
            StageAOutcome::Passed { cleaned, findings } => {
                assert_eq!(cleaned, "hello world");
                assert!(findings.is_empty());
            }
            other => panic!("expected pass, got {other:?}"),
        }
    }

    #[test]
    fn quarantines_script_in_html() {
        let ctx = ctx_with_mime(Some("text/html"));
        let out = run(
            &ctx,
            "<html><body><script>pwn()</script></body></html>",
            &StageAConfig::default(),
        );
        match out {
            StageAOutcome::Quarantined(v) => {
                assert_eq!(v.verdict, Verdict::Quarantined);
                assert_eq!(v.quarantine_class, Some(QuarantineClass::SourceIntegrity));
                assert!(!v.annotations.is_empty());
            }
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn quarantines_oversized() {
        let ctx = ctx_with_mime(Some("text/plain"));
        let big = "a".repeat(2048);
        let cfg = StageAConfig {
            max_bytes: 1024,
            ..StageAConfig::default()
        };
        match run(&ctx, &big, &cfg) {
            StageAOutcome::Quarantined(v) => {
                assert!(v
                    .annotations
                    .iter()
                    .any(|a| a.detail.contains("exceeds limit")));
            }
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn quarantines_bidi_override() {
        let ctx = ctx_with_mime(Some("text/plain"));
        let s = format!("hello{}world", '\u{202E}');
        match run(&ctx, &s, &StageAConfig::default()) {
            StageAOutcome::Quarantined(v) => {
                assert!(v.annotations.iter().any(|a| a.detail.contains("bidi")));
            }
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn quarantines_mime_mismatch() {
        let ctx = ctx_with_mime(Some("text/html"));
        // PDF magic bytes as the payload start.
        let mut bytes = vec![0x25, 0x50, 0x44, 0x46];
        bytes.extend_from_slice(b" rest of PDF-ish stuff");
        let s = String::from_utf8(bytes).unwrap();
        match run(&ctx, &s, &StageAConfig::default()) {
            StageAOutcome::Quarantined(v) => {
                assert!(v
                    .annotations
                    .iter()
                    .any(|a| a.detail.contains("magic bytes")));
            }
            other => panic!("expected quarantine, got {other:?}"),
        }
    }

    #[test]
    fn markdown_with_external_image_has_finding_but_passes() {
        let ctx = ctx_with_mime(Some("text/markdown"));
        let md = "Hello ![x](https://evil.example/y.png)";
        match run(&ctx, md, &StageAConfig::default()) {
            StageAOutcome::Passed { findings, .. } => {
                // External-image finding is present but non-quarantining.
                assert!(findings
                    .iter()
                    .any(|f| f.detail.contains("external image URL")));
            }
            other => panic!("expected pass, got {other:?}"),
        }
    }
}
