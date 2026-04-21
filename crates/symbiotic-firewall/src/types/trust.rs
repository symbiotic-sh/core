//! Trust levels per source (design §2.3).
//!
//! Each untrusted source gets a baseline trust level that influences scan
//! strictness. The mapping from `TrustLevel` to scan-strictness defaults is
//! resolved by the scan engine (future chunks); this module only defines the
//! enum + wire format.

use serde::{Deserialize, Serialize};

/// Baseline trust for a content source.
///
/// Ordering (highest trust → lowest): `Trusted` > `Medium` > `Low` > `VeryLow`.
/// See design §2.3 for the mapping of sources (operator input, tool output,
/// web fetches, etc.) to these levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// Operator input, operator vault Markdown, operator-originated recalls.
    /// Firewall is skipped for trusted sources.
    Trusted,
    /// Peer-agent opinions, cross-agent handoff messages. Reduced scan —
    /// already inside the trust domain.
    Medium,
    /// Tool observations (sandboxed), recalled external content, swarm-repo
    /// artifacts. Full scan required.
    Low,
    /// Web fetches, browser-automation HTML. Full scan + strict thresholds.
    VeryLow,
}

impl TrustLevel {
    /// Whether the firewall should be skipped entirely for this trust level.
    pub fn skip_firewall(self) -> bool {
        matches!(self, TrustLevel::Trusted)
    }

    /// Whether Stage C (LLM-lite semantic review) should run unconditionally
    /// for this trust level, regardless of Stage B confidence.
    pub fn force_stage_c(self) -> bool {
        matches!(self, TrustLevel::VeryLow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(level: TrustLevel, wire: &str) {
        let json = serde_json::to_string(&level).expect("serialize");
        assert_eq!(json, format!("\"{wire}\""));
        let back: TrustLevel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, level);
    }

    #[test]
    fn trust_level_trusted_round_trips() {
        round_trip(TrustLevel::Trusted, "trusted");
    }

    #[test]
    fn trust_level_medium_round_trips() {
        round_trip(TrustLevel::Medium, "medium");
    }

    #[test]
    fn trust_level_low_round_trips() {
        round_trip(TrustLevel::Low, "low");
    }

    #[test]
    fn trust_level_very_low_round_trips() {
        round_trip(TrustLevel::VeryLow, "very_low");
    }

    #[test]
    fn trusted_skips_firewall() {
        assert!(TrustLevel::Trusted.skip_firewall());
        assert!(!TrustLevel::Medium.skip_firewall());
        assert!(!TrustLevel::Low.skip_firewall());
        assert!(!TrustLevel::VeryLow.skip_firewall());
    }

    #[test]
    fn very_low_forces_stage_c() {
        assert!(!TrustLevel::Trusted.force_stage_c());
        assert!(!TrustLevel::Medium.force_stage_c());
        assert!(!TrustLevel::Low.force_stage_c());
        assert!(TrustLevel::VeryLow.force_stage_c());
    }
}
