//! Stage E — annotation injection (design §3.5).
//!
//! Wraps content in `<external_content …>…</external_content>` boundary
//! markers when it is pulled from Archive into an agent's context pack.
//! Modern LLMs are trained to respect this delimiter convention as a hint
//! that the enclosed text is data, not instructions.
//!
//! Stage E runs at **context-assembly time** (not ingest) so the wrap can
//! include context-specific fields — `consuming_agent`, `ingested_at`,
//! `firewall_verdict` — that aren't known at ingest. The annotation is a
//! pure string operation; no I/O, no allocation beyond the assembled
//! markup.

use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;

use crate::types::{ContentSource, FirewallVerdict, TrustLevel, Verdict};

/// Configuration for Stage E (only the consuming-agent identifier is
/// context-specific; everything else is derived from the entry).
#[derive(Debug, Clone)]
pub struct StageEConfig<'a> {
    /// Stable id of the consuming agent. Surfaced as `consuming_agent`
    /// on the annotation so audit (T120) can attribute who saw what.
    pub consuming_agent: &'a str,
}

/// Wrap `content` with the `<external_content>` annotation.
///
/// Required attributes per design §3.5:
/// - `source` — `ContentSource.kind`
/// - `trust` — wire form of [`TrustLevel`]
/// - `content_hash` — `sha256:<hex>` over `content`
/// - `firewall_verdict` — wire form of [`Verdict`]
/// - `firewall_version` — `verdict.verdict_version`
/// - `ingested_at` — RFC3339 of `verdict.scan_timestamp`
/// - `consuming_agent` — id of the agent receiving the context
pub fn wrap(
    content: &str,
    source: &ContentSource,
    trust: TrustLevel,
    verdict: &FirewallVerdict,
    config: &StageEConfig<'_>,
) -> String {
    let trust_str = trust_to_str(trust);
    let verdict_str = verdict_to_str(verdict.verdict);
    let scan_ts = verdict
        .scan_timestamp
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string());
    let hash = content_hash(content);

    let mut out = String::with_capacity(content.len() + 256);
    out.push_str("<external_content");
    out.push_str(&format!("\n  source=\"{}\"", escape(&source.kind)));
    out.push_str(&format!("\n  trust=\"{trust_str}\""));
    out.push_str(&format!("\n  content_hash=\"sha256:{hash}\""));
    out.push_str(&format!("\n  firewall_verdict=\"{verdict_str}\""));
    out.push_str(&format!(
        "\n  firewall_version=\"{}\"",
        escape(&verdict.verdict_version)
    ));
    out.push_str(&format!("\n  ingested_at=\"{}\"", escape(&scan_ts)));
    out.push_str(&format!(
        "\n  consuming_agent=\"{}\">\n",
        escape(config.consuming_agent)
    ));
    out.push_str(content);
    out.push_str("\n</external_content>");
    out
}

fn trust_to_str(level: TrustLevel) -> &'static str {
    match level {
        TrustLevel::Trusted => "trusted",
        TrustLevel::Medium => "medium",
        TrustLevel::Low => "low",
        TrustLevel::VeryLow => "very_low",
    }
}

fn verdict_to_str(v: Verdict) -> &'static str {
    match v {
        Verdict::Passed => "passed",
        Verdict::Flagged => "flagged",
        Verdict::Quarantined => "quarantined",
    }
}

fn content_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

/// Minimal escaping for attribute values — replaces `"` and control-ish
/// characters with safe forms so the wrapping stays well-formed even if
/// the source kind contains stray quotes.
fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("&quot;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            ch if ch.is_control() && ch != '\n' && ch != '\t' => {
                // Drop control chars to avoid breaking the annotation.
            }
            ch => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::SECURITY_VERSION;
    use std::collections::BTreeMap;
    use time::OffsetDateTime;

    fn sample_source() -> ContentSource {
        ContentSource {
            kind: "web_fetch".into(),
            url: Some("https://example.com/doc".into()),
            fetched_at: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
            claimed_content_type: Some("text/html".into()),
            headers: BTreeMap::new(),
        }
    }

    fn passed_verdict() -> FirewallVerdict {
        FirewallVerdict {
            verdict: Verdict::Passed,
            verdict_version: SECURITY_VERSION.to_string(),
            scan_timestamp: OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("valid timestamp"),
            quarantine_class: None,
            source_receipt_id: None,
            annotations: Vec::new(),
        }
    }

    #[test]
    fn wrap_includes_all_required_fields() {
        let cfg = StageEConfig {
            consuming_agent: "researcher-v2",
        };
        let wrapped = wrap(
            "hello world",
            &sample_source(),
            TrustLevel::Low,
            &passed_verdict(),
            &cfg,
        );
        assert!(wrapped.contains("<external_content"));
        assert!(wrapped.contains("source=\"web_fetch\""));
        assert!(wrapped.contains("trust=\"low\""));
        assert!(wrapped.contains("content_hash=\"sha256:"));
        assert!(wrapped.contains("firewall_verdict=\"passed\""));
        assert!(wrapped.contains("firewall_version=\"0.1.0\""));
        assert!(wrapped.contains("ingested_at=\"2023-11-14T22:13:20Z\""));
        assert!(wrapped.contains("consuming_agent=\"researcher-v2\""));
        assert!(wrapped.contains("hello world"));
        assert!(wrapped.ends_with("</external_content>"));
    }

    #[test]
    fn wrap_preserves_content_verbatim() {
        let body = "line one\nline two\n  indented";
        let cfg = StageEConfig {
            consuming_agent: "agent-x",
        };
        let wrapped = wrap(
            body,
            &sample_source(),
            TrustLevel::Medium,
            &passed_verdict(),
            &cfg,
        );
        assert!(wrapped.contains(body));
    }

    #[test]
    fn wrap_uses_correct_trust_label_for_each_level() {
        let cfg = StageEConfig {
            consuming_agent: "a",
        };
        for (level, label) in [
            (TrustLevel::Trusted, "trusted"),
            (TrustLevel::Medium, "medium"),
            (TrustLevel::Low, "low"),
            (TrustLevel::VeryLow, "very_low"),
        ] {
            let wrapped = wrap("x", &sample_source(), level, &passed_verdict(), &cfg);
            assert!(
                wrapped.contains(&format!("trust=\"{label}\"")),
                "level {level:?} → {label}"
            );
        }
    }

    #[test]
    fn wrap_emits_flagged_or_quarantined_verdicts_verbatim() {
        let cfg = StageEConfig {
            consuming_agent: "a",
        };
        let mut v = passed_verdict();
        v.verdict = Verdict::Flagged;
        let wrapped = wrap("x", &sample_source(), TrustLevel::Low, &v, &cfg);
        assert!(wrapped.contains("firewall_verdict=\"flagged\""));

        v.verdict = Verdict::Quarantined;
        let wrapped = wrap("x", &sample_source(), TrustLevel::Low, &v, &cfg);
        assert!(wrapped.contains("firewall_verdict=\"quarantined\""));
    }

    #[test]
    fn wrap_hash_is_stable_for_identical_content() {
        let cfg = StageEConfig {
            consuming_agent: "a",
        };
        let a = wrap(
            "hello",
            &sample_source(),
            TrustLevel::Low,
            &passed_verdict(),
            &cfg,
        );
        let b = wrap(
            "hello",
            &sample_source(),
            TrustLevel::Low,
            &passed_verdict(),
            &cfg,
        );
        // Hash + body identical; the only differing fields would be in the
        // verdict if scan_timestamp differed, but our fixture is fixed.
        assert_eq!(a, b);
    }

    #[test]
    fn wrap_escapes_quotes_in_source_kind() {
        let mut src = sample_source();
        src.kind = r#"quote " inside"#.to_string();
        let cfg = StageEConfig {
            consuming_agent: r#"agent " name"#,
        };
        let wrapped = wrap("body", &src, TrustLevel::Low, &passed_verdict(), &cfg);
        // Quotes must be escaped so the surrounding `"…"` stays well-formed.
        assert!(!wrapped.contains(r#"="quote " inside""#));
        assert!(wrapped.contains("&quot;"));
    }
}
