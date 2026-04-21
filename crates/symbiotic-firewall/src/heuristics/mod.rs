//! Prompt-injection heuristics used by Stage B.
//!
//! All heuristics return a `Vec<HeuristicHit>`. Stage B combines the hits
//! into a single confidence score and a set of [`StageFinding`][crate::types::StageFinding]s.
//!
//! The rules live in three files so operators can reason about each class
//! independently:
//!
//! - [`injection_phrases`] — regex + literal-phrase matching for the classic
//!   prompt-injection surface ("ignore previous instructions", role tokens,
//!   tool-call impersonation).
//! - [`delimiters`] — nested Markdown / code-fence confusion.
//! - [`encoding`] — suspicious encoded blocks (base64, hex) and bidi tricks.

pub mod delimiters;
pub mod encoding;
pub mod injection_phrases;

use crate::types::{FindingKind, Stage, StageFinding};

/// Quantized severity of a single heuristic match.
///
/// Values are deliberately coarse — "this pattern fired" is the interesting
/// signal; the exact score per hit is best kept consistent across classes
/// so Stage B's aggregation stays legible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitSeverity {
    /// Low-signal pattern (e.g. role-token fragment, short base64 block).
    Weak,
    /// Medium-signal pattern (e.g. "ignore previous instructions").
    Moderate,
    /// High-signal pattern (e.g. ChatML-style `<|im_start|>`).
    Strong,
}

impl HitSeverity {
    /// Score contribution for this hit.
    pub fn weight(self) -> f32 {
        match self {
            HitSeverity::Weak => 0.15,
            HitSeverity::Moderate => 0.35,
            HitSeverity::Strong => 0.60,
        }
    }
}

/// A single heuristic match, normalized across all classes.
#[derive(Debug, Clone, PartialEq)]
pub struct HeuristicHit {
    /// Which rule fired (human-readable; used in findings).
    pub rule: &'static str,
    /// What the rule observed (free-form).
    pub detail: String,
    pub severity: HitSeverity,
}

impl HeuristicHit {
    /// Convert to a `StageFinding` tagged against Stage B.
    pub fn to_finding(&self) -> StageFinding {
        StageFinding {
            stage: Stage::B,
            kind: FindingKind::InjectionHeuristic,
            detail: format!("{}: {}", self.rule, self.detail),
            confidence: self.severity.weight(),
        }
    }
}

/// Run all Stage B heuristics against `input` and collect hits.
pub fn scan_all(input: &str) -> Vec<HeuristicHit> {
    let mut hits = Vec::new();
    hits.extend(injection_phrases::scan(input));
    hits.extend(delimiters::scan(input));
    hits.extend(encoding::scan(input));
    hits
}

/// Aggregate a set of hits into a single confidence score in `[0.0, 1.0]`.
///
/// Uses the **independent-evidence combination** rule:
///
/// ```text
/// P = 1 - product(1 - w_i)
/// ```
///
/// This is well-behaved for repeated hits of the same class (doesn't saturate
/// too fast) and cleanly reaches 1.0 when any strong hit + moderate evidence
/// pile up.
pub fn aggregate_confidence(hits: &[HeuristicHit]) -> f32 {
    if hits.is_empty() {
        return 0.0;
    }
    let mut neg = 1.0f32;
    for h in hits {
        let w = h.severity.weight();
        neg *= 1.0 - w;
    }
    (1.0 - neg).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_hits_score_zero() {
        assert_eq!(aggregate_confidence(&[]), 0.0);
    }

    #[test]
    fn single_weak_hit_low_score() {
        let hits = vec![HeuristicHit {
            rule: "t",
            detail: "x".into(),
            severity: HitSeverity::Weak,
        }];
        let c = aggregate_confidence(&hits);
        assert!(c < 0.3, "got {c}");
    }

    #[test]
    fn two_moderate_hits_crosses_threshold() {
        let hits = vec![
            HeuristicHit {
                rule: "a",
                detail: "x".into(),
                severity: HitSeverity::Moderate,
            },
            HeuristicHit {
                rule: "b",
                detail: "y".into(),
                severity: HitSeverity::Moderate,
            },
        ];
        let c = aggregate_confidence(&hits);
        // 1 - 0.65*0.65 = 0.5775
        assert!(c > 0.5 && c < 0.65, "got {c}");
    }

    #[test]
    fn strong_plus_moderate_is_very_high() {
        let hits = vec![
            HeuristicHit {
                rule: "a",
                detail: "x".into(),
                severity: HitSeverity::Strong,
            },
            HeuristicHit {
                rule: "b",
                detail: "y".into(),
                severity: HitSeverity::Moderate,
            },
        ];
        let c = aggregate_confidence(&hits);
        // 1 - 0.4*0.65 = 0.74
        assert!(c > 0.70 && c < 0.80, "got {c}");
    }

    #[test]
    fn confidence_bounded_by_one() {
        let hits: Vec<_> = (0..20)
            .map(|_| HeuristicHit {
                rule: "a",
                detail: "x".into(),
                severity: HitSeverity::Strong,
            })
            .collect();
        let c = aggregate_confidence(&hits);
        assert!(c <= 1.0);
        assert!(c > 0.99);
    }
}
