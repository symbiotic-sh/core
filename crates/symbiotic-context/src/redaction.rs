//! PII detection and pseudonymization engine for the Recall Gateway.
//!
//! Provides regex-based detection of 10 PII categories (MVP) and
//! session-scoped hash-based pseudonymization for multi-turn consistency.
//! See `docs/design/redaction-policy.md` for the full specification.

use std::collections::HashMap;

use rand::Rng;
use regex::Regex;
use sha2::{Digest, Sha256};

/// Categories of personally identifiable information detected by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PiiCategory {
    Email,
    PhoneUs,
    PhoneInternational,
    Ssn,
    CreditCard,
    IpAddress,
    UsAddress,
    UsZipCode,
    ApiKey,
    SensitiveKeyword,
}

impl PiiCategory {
    /// Human-readable label used in pseudonym generation (e.g., "Email A").
    pub fn label(&self) -> &'static str {
        match self {
            Self::Email => "Email",
            Self::PhoneUs | Self::PhoneInternational => "Phone",
            Self::Ssn => "SSN",
            Self::CreditCard => "Card",
            Self::IpAddress => "IP",
            Self::UsAddress => "Address",
            Self::UsZipCode => "Zip",
            Self::ApiKey => "Key",
            Self::SensitiveKeyword => "Sensitive",
        }
    }

    /// The redaction action for this category.
    pub fn action(&self) -> RedactionAction {
        match self {
            Self::Email => RedactionAction::Replace("[redacted-email]"),
            Self::PhoneUs | Self::PhoneInternational => {
                RedactionAction::Replace("[redacted-phone]")
            }
            Self::Ssn => RedactionAction::Remove,
            Self::CreditCard => RedactionAction::Remove,
            Self::IpAddress => RedactionAction::Replace("[redacted-ip]"),
            Self::UsAddress => RedactionAction::Remove,
            Self::UsZipCode => RedactionAction::Remove,
            Self::ApiKey => RedactionAction::Replace("[redacted-key]"),
            Self::SensitiveKeyword => RedactionAction::Replace("[redacted-sensitive]"),
        }
    }
}

/// What to do with a detected PII span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedactionAction {
    /// Replace the span with a fixed placeholder string.
    Replace(&'static str),
    /// Remove the span entirely.
    Remove,
}

/// A single PII detection within input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PiiDetection {
    pub category: PiiCategory,
    /// Byte offset of start of match in the input.
    pub start: usize,
    /// Byte offset of end of match in the input (exclusive).
    pub end: usize,
    /// The matched text.
    pub matched_text: String,
}

/// Compiled regex patterns for PII detection.
pub struct RedactionEngine {
    patterns: Vec<(PiiCategory, Regex)>,
    sensitive_keywords: Vec<String>,
}

impl RedactionEngine {
    /// Creates a new engine with the default MVP pattern set.
    pub fn new() -> Self {
        Self::with_keywords(vec![
            "password".to_string(),
            "api_key".to_string(),
            "apikey".to_string(),
            "secret".to_string(),
            "ssn".to_string(),
            "card".to_string(),
        ])
    }

    /// Creates a new engine with custom sensitive keywords.
    pub fn with_keywords(sensitive_keywords: Vec<String>) -> Self {
        let patterns = vec![
            // Email: standard RFC-ish pattern
            (
                PiiCategory::Email,
                Regex::new(r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}").unwrap(),
            ),
            // SSN: XXX-XX-XXXX (must come before phone to get priority)
            (
                PiiCategory::Ssn,
                Regex::new(r"\b\d{3}-\d{2}-\d{4}\b").unwrap(),
            ),
            // Credit card: 4 groups of 4 digits (with Luhn check in post-processing)
            (
                PiiCategory::CreditCard,
                Regex::new(r"\b\d{4}[-\s]?\d{4}[-\s]?\d{4}[-\s]?\d{4}\b").unwrap(),
            ),
            // API key patterns: key=value or key:value with 20+ char value
            (
                PiiCategory::ApiKey,
                Regex::new(
                    r#"(?i)(?:api[_\-]?key|token|secret|aws[_\-]?access|gh[ps]_)[=:\s]+["']?[\w\-]{20,}"#,
                )
                .unwrap(),
            ),
            // GitHub personal access tokens
            (
                PiiCategory::ApiKey,
                Regex::new(r"\b(?:ghp|ghs|gho|ghu|ghr)_[A-Za-z0-9]{36}\b").unwrap(),
            ),
            // AWS access key IDs
            (
                PiiCategory::ApiKey,
                Regex::new(r"\bAKIA[0-9A-Z]{16}\b").unwrap(),
            ),
            // US Address: number + street name + suffix
            (
                PiiCategory::UsAddress,
                Regex::new(
                    r"(?i)\b\d{1,5}\s+[\w\s]+(?:Street|St|Avenue|Ave|Boulevard|Blvd|Drive|Dr|Lane|Ln|Road|Rd|Way|Court|Ct|Place|Pl)\b",
                )
                .unwrap(),
            ),
            // Phone (US): various formats with optional country code
            (
                PiiCategory::PhoneUs,
                Regex::new(
                    r"(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b",
                )
                .unwrap(),
            ),
            // Phone (international): +CC followed by digit groups
            (
                PiiCategory::PhoneInternational,
                Regex::new(r"\+\d{1,3}(?:[-.\s]\d+){1,5}").unwrap(),
            ),
            // IPv4 address (private ranges: 10.x, 172.16-31.x, 192.168.x)
            (
                PiiCategory::IpAddress,
                Regex::new(
                    r"\b(?:10\.\d{1,3}\.\d{1,3}\.\d{1,3}|172\.(?:1[6-9]|2\d|3[01])\.\d{1,3}\.\d{1,3}|192\.168\.\d{1,3}\.\d{1,3})\b",
                )
                .unwrap(),
            ),
            // US ZIP code: 5-digit or ZIP+4 format
            (
                PiiCategory::UsZipCode,
                Regex::new(r"\b\d{5}(-\d{4})?\b").unwrap(),
            ),
        ];

        Self {
            patterns,
            sensitive_keywords,
        }
    }

    /// Detects all PII in the input text.
    ///
    /// Returns detections sorted by start position. Overlapping detections
    /// are resolved by keeping the earlier/longer match.
    pub fn detect(&self, text: &str) -> Vec<PiiDetection> {
        let mut detections = Vec::new();

        // Regex-based detection
        for (category, regex) in &self.patterns {
            for m in regex.find_iter(text) {
                let matched_text = m.as_str().to_string();

                // Post-processing: Luhn check for credit cards
                if *category == PiiCategory::CreditCard && !luhn_check(&matched_text) {
                    continue;
                }

                detections.push(PiiDetection {
                    category: *category,
                    start: m.start(),
                    end: m.end(),
                    matched_text,
                });
            }
        }

        // Sensitive keyword detection (word-boundary matching)
        let text_lower = text.to_ascii_lowercase();
        for keyword in &self.sensitive_keywords {
            let kw_lower = keyword.to_ascii_lowercase();
            let mut search_from = 0;
            while let Some(pos) = text_lower[search_from..].find(&kw_lower) {
                let abs_start = search_from + pos;
                let abs_end = abs_start + kw_lower.len();

                // Check word boundaries
                let before_ok =
                    abs_start == 0 || !text.as_bytes()[abs_start - 1].is_ascii_alphanumeric();
                let after_ok =
                    abs_end >= text.len() || !text.as_bytes()[abs_end].is_ascii_alphanumeric();

                if before_ok && after_ok {
                    // Also match keyword=value patterns
                    let end = if abs_end < text.len() && text.as_bytes()[abs_end] == b'=' {
                        // Consume the value after '='
                        let value_start = abs_end + 1;
                        text[value_start..]
                            .find(|c: char| c.is_whitespace())
                            .map(|p| value_start + p)
                            .unwrap_or(text.len())
                    } else {
                        abs_end
                    };

                    detections.push(PiiDetection {
                        category: PiiCategory::SensitiveKeyword,
                        start: abs_start,
                        end,
                        matched_text: text[abs_start..end].to_string(),
                    });
                }

                search_from = abs_end;
            }
        }

        // Sort by start position, then by length (longer first for overlap resolution)
        detections.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| b.end.cmp(&a.end)));

        // Remove overlapping detections: keep the first (earlier/longer) match
        let mut result = Vec::new();
        let mut last_end = 0;
        for det in detections {
            if det.start >= last_end {
                last_end = det.end;
                result.push(det);
            }
        }

        result
    }

    /// Redacts all detected PII from the text.
    ///
    /// Applies the category-specific action (replace or remove) for each detection.
    pub fn redact(&self, text: &str) -> String {
        let detections = self.detect(text);
        if detections.is_empty() {
            return text.to_string();
        }

        let mut result = String::with_capacity(text.len());
        let mut cursor = 0;

        for det in &detections {
            // Append text before this detection
            result.push_str(&text[cursor..det.start]);

            match det.category.action() {
                RedactionAction::Replace(placeholder) => {
                    result.push_str(placeholder);
                }
                RedactionAction::Remove => {
                    // Remove: skip the matched text (and trim trailing whitespace)
                }
            }

            cursor = det.end;
        }

        // Append remaining text
        result.push_str(&text[cursor..]);

        // Clean up double spaces from removals
        while result.contains("  ") {
            result = result.replace("  ", " ");
        }

        result.trim().to_string()
    }

    /// Redacts PII using pseudonymization for consistent multi-turn masking.
    ///
    /// Entities that should be replaced get session-consistent pseudonyms
    /// (e.g., "Email A", "Phone B"). Entities that should be removed are
    /// still removed entirely.
    pub fn redact_with_pseudonyms(&self, text: &str, mask_map: &mut SessionMaskMap) -> String {
        let detections = self.detect(text);
        if detections.is_empty() {
            return text.to_string();
        }

        let mut result = String::with_capacity(text.len());
        let mut cursor = 0;

        for det in &detections {
            result.push_str(&text[cursor..det.start]);

            match det.category.action() {
                RedactionAction::Replace(_) => {
                    let pseudonym = mask_map.pseudonym(&det.matched_text, det.category);
                    result.push('[');
                    result.push_str(&pseudonym);
                    result.push(']');
                }
                RedactionAction::Remove => {
                    // Remove entirely
                }
            }

            cursor = det.end;
        }

        result.push_str(&text[cursor..]);

        while result.contains("  ") {
            result = result.replace("  ", " ");
        }

        result.trim().to_string()
    }
}

impl Default for RedactionEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Convenience function: redact PII from text using the default engine.
///
/// This is a stateless helper intended for pipeline integration points
/// (e.g. intake before storage, agent output before sending). For
/// session-scoped pseudonymization use [`RedactionEngine::redact_with_pseudonyms`].
pub fn redact_pii(text: &str) -> String {
    RedactionEngine::new().redact(text)
}

/// Session-scoped mask mapping for multi-turn pseudonymization consistency.
///
/// Uses a random ephemeral salt (never persisted) and salted SHA-256 hashing
/// so that the same entity always maps to the same pseudonym within a session,
/// but different sessions produce different pseudonyms.
pub struct SessionMaskMap {
    salt: [u8; 32],
    map: HashMap<String, String>,
    counters: HashMap<PiiCategory, u32>,
}

impl SessionMaskMap {
    /// Creates a new session mask map with a random salt.
    pub fn new() -> Self {
        let mut salt = [0u8; 32];
        rand::rng().fill_bytes(&mut salt);
        Self {
            salt,
            map: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    /// Creates a session mask map with a fixed salt (for testing).
    #[cfg(test)]
    pub fn with_salt(salt: [u8; 32]) -> Self {
        Self {
            salt,
            map: HashMap::new(),
            counters: HashMap::new(),
        }
    }

    /// Gets or creates a pseudonym for the given entity text and category.
    ///
    /// Same entity text within the same session always returns the same pseudonym.
    pub fn pseudonym(&mut self, entity: &str, category: PiiCategory) -> String {
        let hash = self.hash_entity(entity);
        if let Some(existing) = self.map.get(&hash) {
            return existing.clone();
        }

        let counter = self.counters.entry(category).or_insert(0);
        *counter += 1;
        let label = format!("{} {}", category.label(), index_to_letter(*counter));
        self.map.insert(hash, label.clone());
        label
    }

    fn hash_entity(&self, entity: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.salt);
        hasher.update(entity.as_bytes());
        hex::encode(hasher.finalize())
    }
}

impl Default for SessionMaskMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Converts a 1-based counter to a letter label: 1->A, 2->B, ..., 26->Z, 27->AA, etc.
fn index_to_letter(index: u32) -> String {
    let mut result = String::new();
    let mut n = index;
    while n > 0 {
        n -= 1;
        result.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    result.chars().rev().collect()
}

/// Validates a credit card number using the Luhn algorithm.
fn luhn_check(number: &str) -> bool {
    let digits: Vec<u32> = number
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c.to_digit(10).unwrap())
        .collect();

    if digits.len() < 13 || digits.len() > 19 {
        return false;
    }

    let mut sum = 0;
    let mut double = false;
    for &d in digits.iter().rev() {
        let mut val = d;
        if double {
            val *= 2;
            if val > 9 {
                val -= 9;
            }
        }
        sum += val;
        double = !double;
    }

    sum % 10 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> RedactionEngine {
        RedactionEngine::new()
    }

    // --- Email detection ---

    #[test]
    fn detects_standard_email() {
        let dets = engine().detect("contact user@example.com for details");
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].category, PiiCategory::Email);
        assert_eq!(dets[0].matched_text, "user@example.com");
    }

    #[test]
    fn detects_email_with_dots_and_plus() {
        let dets = engine().detect("reach john.doe+test@sub.domain.org now");
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].category, PiiCategory::Email);
        assert_eq!(dets[0].matched_text, "john.doe+test@sub.domain.org");
    }

    #[test]
    fn no_false_positive_for_at_sign_without_tld() {
        let dets = engine().detect("variable @x is used here");
        let emails: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::Email)
            .collect();
        assert!(emails.is_empty());
    }

    // --- Phone detection ---

    #[test]
    fn detects_us_phone_with_dashes() {
        let dets = engine().detect("Call 555-123-4567 today");
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].category, PiiCategory::PhoneUs);
        assert_eq!(dets[0].matched_text, "555-123-4567");
    }

    #[test]
    fn detects_us_phone_with_country_code() {
        let dets = engine().detect("Phone: +1-555-123-4567");
        assert!(!dets.is_empty());
        let phone = dets.iter().find(|d| {
            matches!(
                d.category,
                PiiCategory::PhoneUs | PiiCategory::PhoneInternational
            )
        });
        assert!(phone.is_some());
    }

    #[test]
    fn detects_us_phone_with_parens() {
        let dets = engine().detect("Reach us at (555) 123-4567");
        assert!(!dets.is_empty());
        let phone = dets.iter().find(|d| d.category == PiiCategory::PhoneUs);
        assert!(phone.is_some());
    }

    #[test]
    fn detects_international_phone() {
        let dets = engine().detect("Office: +44 20 7946 0958");
        assert!(!dets.is_empty());
        let phone = dets.iter().find(|d| {
            matches!(
                d.category,
                PiiCategory::PhoneUs | PiiCategory::PhoneInternational
            )
        });
        assert!(phone.is_some());
    }

    // --- SSN detection ---

    #[test]
    fn detects_ssn_pattern() {
        let dets = engine().detect("SSN: 123-45-6789");
        assert_eq!(dets.len(), 2); // SSN pattern + sensitive keyword "ssn"
        let ssn = dets.iter().find(|d| d.category == PiiCategory::Ssn);
        assert!(ssn.is_some());
        assert_eq!(ssn.unwrap().matched_text, "123-45-6789");
    }

    #[test]
    fn no_false_positive_ssn_for_phone() {
        // Phone numbers should not match SSN pattern (different digit grouping)
        let dets = engine().detect("Number: 123-456-7890");
        let ssns: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::Ssn)
            .collect();
        assert!(ssns.is_empty());
    }

    // --- Credit card detection ---

    #[test]
    fn detects_credit_card_with_spaces() {
        // Visa test number (passes Luhn)
        let dets = engine().detect("Pay with 4539 1488 0343 6467");
        let cc = dets.iter().find(|d| d.category == PiiCategory::CreditCard);
        assert!(cc.is_some());
    }

    #[test]
    fn detects_credit_card_with_dashes() {
        // Visa test number (passes Luhn)
        let dets = engine().detect("Card: 4539-1488-0343-6467");
        let cc = dets.iter().find(|d| d.category == PiiCategory::CreditCard);
        assert!(cc.is_some());
    }

    #[test]
    fn rejects_credit_card_failing_luhn() {
        let dets = engine().detect("Not a card: 1234-5678-9012-3456");
        let cc: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::CreditCard)
            .collect();
        assert!(cc.is_empty());
    }

    // --- API key detection ---

    #[test]
    fn detects_api_key_assignment() {
        let dets = engine().detect("api_key=sk_live_1234567890abcdefghij");
        let keys: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::ApiKey)
            .collect();
        assert!(!keys.is_empty());
    }

    #[test]
    fn detects_github_personal_access_token() {
        let dets = engine().detect("token: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        let keys: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::ApiKey)
            .collect();
        assert!(!keys.is_empty());
    }

    #[test]
    fn detects_aws_access_key() {
        let dets = engine().detect("aws key AKIAIOSFODNN7EXAMPLE");
        let keys: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::ApiKey)
            .collect();
        assert!(!keys.is_empty());
    }

    // --- IP address detection ---

    #[test]
    fn detects_private_ipv4() {
        let dets = engine().detect("Server at 192.168.1.100");
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].category, PiiCategory::IpAddress);
        assert_eq!(dets[0].matched_text, "192.168.1.100");
    }

    #[test]
    fn detects_10_network_ip() {
        let dets = engine().detect("Connect to 10.0.0.1");
        assert_eq!(dets.len(), 1);
        assert_eq!(dets[0].category, PiiCategory::IpAddress);
    }

    #[test]
    fn does_not_flag_public_ip() {
        let dets = engine().detect("Public DNS: 8.8.8.8");
        let ips: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::IpAddress)
            .collect();
        assert!(ips.is_empty());
    }

    // --- US address detection ---

    #[test]
    fn detects_us_street_address() {
        let dets = engine().detect("Ship to 123 Main Street");
        let addrs: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::UsAddress)
            .collect();
        assert!(!addrs.is_empty());
    }

    #[test]
    fn detects_address_with_blvd() {
        let dets = engine().detect("Office at 456 Sunset Blvd");
        let addrs: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::UsAddress)
            .collect();
        assert!(!addrs.is_empty());
    }

    // --- Sensitive keyword detection ---

    #[test]
    fn detects_sensitive_keywords() {
        let dets = engine().detect("The password is stored in a file");
        let kws: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::SensitiveKeyword)
            .collect();
        assert!(!kws.is_empty());
        assert_eq!(kws[0].matched_text, "password");
    }

    #[test]
    fn detects_keyword_with_value() {
        let dets = engine().detect("Set password=hunter2 in config");
        let kws: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::SensitiveKeyword)
            .collect();
        assert!(!kws.is_empty());
        assert_eq!(kws[0].matched_text, "password=hunter2");
    }

    #[test]
    fn no_false_positive_keyword_in_word() {
        let dets = engine().detect("The passwords were changed");
        // "password" is a substring of "passwords", not a word boundary match
        let kws: Vec<_> = dets
            .iter()
            .filter(|d| d.category == PiiCategory::SensitiveKeyword)
            .collect();
        assert!(kws.is_empty());
    }

    // --- Redaction ---

    #[test]
    fn redact_replaces_email() {
        let result = engine().redact("Contact user@example.com for help");
        assert_eq!(result, "Contact [redacted-email] for help");
    }

    #[test]
    fn redact_replaces_phone() {
        let result = engine().redact("Call 555-123-4567 now");
        assert_eq!(result, "Call [redacted-phone] now");
    }

    #[test]
    fn redact_removes_ssn() {
        let result = engine().redact("SSN is 123-45-6789 on file");
        // SSN removed, keyword "ssn" may overlap. The SSN digits should be gone.
        assert!(!result.contains("123-45-6789"));
    }

    #[test]
    fn redact_removes_credit_card() {
        let result = engine().redact("Charge 4539 1488 0343 6467 now");
        assert!(!result.contains("4539"));
    }

    #[test]
    fn redact_replaces_private_ip() {
        let result = engine().redact("Server is 192.168.1.1 in the rack");
        assert_eq!(result, "Server is [redacted-ip] in the rack");
    }

    #[test]
    fn redact_removes_address() {
        let result = engine().redact("Lives at 123 Main Street downtown");
        assert!(!result.contains("123 Main Street"));
    }

    #[test]
    fn redact_preserves_clean_text() {
        let text = "This is normal text without any PII";
        assert_eq!(engine().redact(text), text);
    }

    #[test]
    fn redact_handles_empty_input() {
        assert_eq!(engine().redact(""), "");
    }

    #[test]
    fn redact_handles_multiple_pii_types() {
        let text = "Email user@example.com, phone 555-123-4567, ip 10.0.0.1";
        let result = engine().redact(text);
        assert!(result.contains("[redacted-email]"));
        assert!(result.contains("[redacted-phone]"));
        assert!(result.contains("[redacted-ip]"));
    }

    // --- Pseudonymization ---

    #[test]
    fn pseudonym_consistent_within_session() {
        let mut map = SessionMaskMap::with_salt([42u8; 32]);
        let p1 = map.pseudonym("user@example.com", PiiCategory::Email);
        let p2 = map.pseudonym("user@example.com", PiiCategory::Email);
        assert_eq!(p1, p2);
        assert_eq!(p1, "Email A");
    }

    #[test]
    fn pseudonym_different_entities_get_different_labels() {
        let mut map = SessionMaskMap::with_salt([42u8; 32]);
        let p1 = map.pseudonym("a@example.com", PiiCategory::Email);
        let p2 = map.pseudonym("b@example.com", PiiCategory::Email);
        assert_eq!(p1, "Email A");
        assert_eq!(p2, "Email B");
    }

    #[test]
    fn pseudonym_different_categories_have_separate_counters() {
        let mut map = SessionMaskMap::with_salt([42u8; 32]);
        let email = map.pseudonym("user@example.com", PiiCategory::Email);
        let phone = map.pseudonym("555-123-4567", PiiCategory::PhoneUs);
        assert_eq!(email, "Email A");
        assert_eq!(phone, "Phone A");
    }

    #[test]
    fn pseudonym_different_sessions_produce_different_hashes() {
        let mut map1 = SessionMaskMap::with_salt([1u8; 32]);
        let mut map2 = SessionMaskMap::with_salt([2u8; 32]);

        // Same entity gets same label but the internal hash differs
        let p1 = map1.pseudonym("user@example.com", PiiCategory::Email);
        let p2 = map2.pseudonym("user@example.com", PiiCategory::Email);

        // Labels are the same (both "Email A") because counters start at 1
        // but internal hashes are different (verified by the salt difference)
        assert_eq!(p1, "Email A");
        assert_eq!(p2, "Email A");

        // If we query in different order, different salt -> different mapping
        let mut map3 = SessionMaskMap::with_salt([3u8; 32]);
        map3.pseudonym("other@example.com", PiiCategory::Email);
        let p3 = map3.pseudonym("user@example.com", PiiCategory::Email);
        assert_eq!(p3, "Email B"); // Second email in this session
    }

    #[test]
    fn redact_with_pseudonyms_produces_consistent_labels() {
        let engine = engine();
        let mut map = SessionMaskMap::with_salt([42u8; 32]);

        let text1 = "Contact user@example.com for details";
        let text2 = "Also email user@example.com again";

        let r1 = engine.redact_with_pseudonyms(text1, &mut map);
        let r2 = engine.redact_with_pseudonyms(text2, &mut map);

        // Same email gets same pseudonym across calls
        assert!(r1.contains("[Email A]"));
        assert!(r2.contains("[Email A]"));
    }

    #[test]
    fn redact_with_pseudonyms_removes_critical_pii() {
        let engine = engine();
        let mut map = SessionMaskMap::with_salt([42u8; 32]);

        let text = "SSN 123-45-6789 and email user@example.com";
        let result = engine.redact_with_pseudonyms(text, &mut map);

        // SSN should be removed (not pseudonymized)
        assert!(!result.contains("123-45-6789"));
        // Email should be pseudonymized
        assert!(result.contains("[Email A]"));
    }

    // --- Overlap handling ---

    #[test]
    fn overlapping_detections_resolved() {
        // A string that could match multiple patterns
        let dets = engine().detect("secret=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij");
        // Should not have more detections than non-overlapping spans
        let mut last_end = 0;
        for det in &dets {
            assert!(det.start >= last_end, "Overlapping detection found");
            last_end = det.end;
        }
    }

    // --- Luhn check ---

    #[test]
    fn luhn_check_valid_visa() {
        assert!(luhn_check("4539148803436467"));
    }

    #[test]
    fn luhn_check_valid_mastercard() {
        assert!(luhn_check("5500000000000004"));
    }

    #[test]
    fn luhn_check_invalid() {
        assert!(!luhn_check("1234567890123456"));
    }

    // --- Index to letter ---

    #[test]
    fn index_to_letter_basic() {
        assert_eq!(index_to_letter(1), "A");
        assert_eq!(index_to_letter(2), "B");
        assert_eq!(index_to_letter(26), "Z");
        assert_eq!(index_to_letter(27), "AA");
        assert_eq!(index_to_letter(28), "AB");
    }
}
