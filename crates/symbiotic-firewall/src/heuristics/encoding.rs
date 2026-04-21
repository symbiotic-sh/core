//! Encoding-smuggling detection.
//!
//! Covers three surfaces:
//!
//! 1. **Large base64 blobs** (>= 1 KB) that could hide injection payloads
//!    behind an innocuous-looking "encoded attachment" framing.
//! 2. **Hex / escape-sequence blobs** of comparable size.
//! 3. **Bidi-override characters** (U+202A–U+202E, U+2066–U+2069) that can
//!    visually disguise text — a classic supply-chain attack surface
//!    (CVE-2021-42574 "Trojan Source").
//!
//! Note that the Stage A structural pass *also* rejects bidi overrides with
//! `SourceIntegrity`; Stage B flags them as a heuristic *in case* the text
//! slipped through an earlier stage-A bypass (e.g. content coming from a
//! medium-trust peer where Stage A strictness was reduced).

use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;

use super::{HeuristicHit, HitSeverity};

/// Minimum length of a base64 run to flag as suspicious. 1 KB matches the
/// design-doc threshold.
const BASE64_FLAG_MIN: usize = 1024;
/// Minimum length of a hex run to flag as suspicious.
const HEX_FLAG_MIN: usize = 1024;

/// Bidi-override characters that can visually reorder text at render time
/// (U+202A LRE, U+202B RLE, U+202C PDF, U+202D LRO, U+202E RLO) plus the
/// isolate set added in Unicode 6.3 (U+2066 LRI, U+2067 RLI, U+2068 FSI,
/// U+2069 PDI).
pub const BIDI_OVERRIDE_CODEPOINTS: &[char] = &[
    '\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}',
    '\u{2069}',
];

// A contiguous run of base64-compatible characters. We accept the URL-safe
// alphabet too since attackers use it interchangeably.
static BASE64_RUN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[A-Za-z0-9+/=_-]{256,}").expect("static regex"));

static HEX_RUN: Lazy<Regex> = Lazy::new(|| Regex::new(r"[0-9a-fA-F]{512,}").expect("static regex"));

pub fn scan(input: &str) -> Vec<HeuristicHit> {
    let mut hits = Vec::new();

    if let Some(len) = find_bidi_override(input) {
        hits.push(HeuristicHit {
            rule: "bidi_override_char",
            detail: format!("{len} bidi-override codepoint(s) present"),
            severity: HitSeverity::Strong,
        });
    }

    if let Some(len) = find_large_base64(input) {
        hits.push(HeuristicHit {
            rule: "large_base64_blob",
            detail: format!("base64-like run of {len} bytes"),
            severity: HitSeverity::Moderate,
        });
    }

    if let Some(len) = find_large_hex(input) {
        hits.push(HeuristicHit {
            rule: "large_hex_blob",
            detail: format!("hex-like run of {len} bytes"),
            severity: HitSeverity::Weak,
        });
    }

    hits
}

fn find_bidi_override(input: &str) -> Option<usize> {
    let count = input
        .chars()
        .filter(|c| BIDI_OVERRIDE_CODEPOINTS.contains(c))
        .count();
    if count > 0 {
        Some(count)
    } else {
        None
    }
}

fn find_large_base64(input: &str) -> Option<usize> {
    for m in BASE64_RUN.find_iter(input) {
        let candidate = m.as_str();
        if candidate.len() < BASE64_FLAG_MIN {
            continue;
        }
        // Require valid base64 decode to drop false positives on long ids /
        // content hashes that happen to match the alphabet.
        let engine = base64::engine::general_purpose::STANDARD_NO_PAD;
        let trimmed: String = candidate.trim_end_matches('=').to_string();
        if engine.decode(trimmed.as_bytes()).is_ok() {
            return Some(candidate.len());
        }
        // Try URL-safe too.
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        if engine.decode(trimmed.as_bytes()).is_ok() {
            return Some(candidate.len());
        }
    }
    None
}

fn find_large_hex(input: &str) -> Option<usize> {
    HEX_RUN
        .find_iter(input)
        .map(|m| m.as_str().len())
        .find(|len| *len >= HEX_FLAG_MIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn benign_short_content_no_hits() {
        assert!(scan("The quick brown fox jumps over the lazy dog.").is_empty());
    }

    #[test]
    fn bidi_override_detected() {
        let s = format!("hello{}world", '\u{202E}');
        let hits = scan(&s);
        assert!(hits.iter().any(|h| h.rule == "bidi_override_char"));
    }

    #[test]
    fn large_base64_detected() {
        // 1200 "A" chars decodes as valid base64.
        let blob = "A".repeat(1200);
        let text = format!("before {blob} after");
        let hits = scan(&text);
        assert!(hits.iter().any(|h| h.rule == "large_base64_blob"));
    }

    #[test]
    fn large_hex_detected() {
        let blob = "ab".repeat(300); // 600 chars
        let text = format!("xx {} yy", blob.repeat(2)); // 1200 chars
        let hits = scan(&text);
        assert!(hits.iter().any(|h| h.rule == "large_hex_blob"));
    }

    #[test]
    fn short_base64_not_flagged() {
        let text = "token=abcdefghijklmnopqrstuvwx==";
        let hits = scan(text);
        assert!(!hits.iter().any(|h| h.rule == "large_base64_blob"));
    }
}
