//! Goal complexity classifier with weighted scoring.
//!
//! Scores incoming goals across multiple dimensions and maps the aggregate
//! score to a complexity level: Simple, Moderate, Complex, or Critical.

use serde::{Deserialize, Serialize};

use super::types::{AutonomyLevel, GoalMetadata};
use crate::llm::LlmClient;

/// Complexity level of a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalComplexity {
    Simple,
    Moderate,
    Complex,
    Critical,
}

/// Individual scoring factor with its computed value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoringFactor {
    pub name: String,
    pub weight: f32,
    /// Raw score in the range 0.0 - 3.0 (maps to Simple..Critical).
    pub raw_score: f32,
    /// Weighted score: `weight * (raw_score / 3.0)`.
    pub weighted_score: f32,
}

/// Result of complexity assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplexityAssessment {
    pub complexity: GoalComplexity,
    pub aggregate_score: f32,
    pub factors: Vec<ScoringFactor>,
    pub llm_override: Option<GoalComplexity>,
    pub reasoning: String,
}

/// Configuration for the complexity classifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassifierConfig {
    /// Thresholds: [simple_max, moderate_max, complex_max].
    /// Scores in [0, simple_max) -> Simple, [simple_max, moderate_max) -> Moderate,
    /// [moderate_max, complex_max) -> Complex, [complex_max, 1.0] -> Critical.
    pub thresholds: [f32; 3],
    /// Whether to use LLM classification for borderline cases.
    pub use_llm_classification: bool,
    /// Tolerance around boundaries for LLM re-classification.
    pub boundary_tolerance: f32,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            thresholds: [0.30, 0.60, 0.85],
            use_llm_classification: false,
            boundary_tolerance: 0.05,
        }
    }
}

/// Classifies goal complexity from parsed metadata.
pub struct GoalComplexityClassifier {
    config: ClassifierConfig,
}

impl GoalComplexityClassifier {
    pub fn new(config: ClassifierConfig) -> Self {
        Self { config }
    }

    /// Classify a goal based on its metadata using deterministic weighted scoring.
    pub fn classify(&self, metadata: &GoalMetadata) -> ComplexityAssessment {
        let factors = vec![
            ScoringFactor {
                name: "domain_count".to_string(),
                weight: 0.15,
                raw_score: score_domain_count(&metadata.domains),
                weighted_score: 0.0, // computed below
            },
            ScoringFactor {
                name: "phase_count".to_string(),
                weight: 0.15,
                raw_score: score_phase_count(&metadata.phases),
                weighted_score: 0.0,
            },
            ScoringFactor {
                name: "workflow_novelty".to_string(),
                weight: 0.20,
                raw_score: score_workflow_novelty(
                    metadata.has_known_template,
                    &metadata.template_name,
                ),
                weighted_score: 0.0,
            },
            ScoringFactor {
                name: "external_deps".to_string(),
                weight: 0.15,
                raw_score: score_external_deps(&metadata.external_dependencies),
                weighted_score: 0.0,
            },
            ScoringFactor {
                name: "estimated_cost".to_string(),
                weight: 0.10,
                raw_score: score_estimated_cost(metadata.estimated_cost_usd),
                weighted_score: 0.0,
            },
            ScoringFactor {
                name: "security_surface".to_string(),
                weight: 0.15,
                raw_score: score_security_surface(&metadata.required_scopes),
                weighted_score: 0.0,
            },
            ScoringFactor {
                name: "autonomy".to_string(),
                weight: 0.10,
                raw_score: score_autonomy(metadata.autonomy_level),
                weighted_score: 0.0,
            },
        ];

        // Compute weighted scores and aggregate.
        let mut factors = factors;
        let mut aggregate = 0.0f32;
        for factor in &mut factors {
            factor.weighted_score = factor.weight * (factor.raw_score / 3.0);
            aggregate += factor.weighted_score;
        }

        // Check for critical security override: if security_surface raw score == 3.0,
        // force Critical regardless of aggregate score.
        let security_factor = factors
            .iter()
            .find(|f| f.name == "security_surface")
            .expect("security_surface factor must exist");
        let security_override = (security_factor.raw_score - 3.0).abs() < f32::EPSILON;

        let complexity = if security_override {
            GoalComplexity::Critical
        } else {
            self.score_to_complexity(aggregate)
        };

        let reasoning = self.build_reasoning(&factors, aggregate, complexity, security_override);

        ComplexityAssessment {
            complexity,
            aggregate_score: aggregate,
            factors,
            llm_override: None,
            reasoning,
        }
    }

    /// Classify with optional LLM override for borderline cases.
    ///
    /// Currently delegates to the deterministic `classify` method. LLM override
    /// will be wired in a future chunk.
    pub async fn classify_with_llm(
        &self,
        metadata: &GoalMetadata,
        _llm: &dyn LlmClient,
    ) -> ComplexityAssessment {
        // TODO: Add LLM override for borderline cases
        self.classify(metadata)
    }

    /// Map an aggregate score to a complexity level using configured thresholds.
    fn score_to_complexity(&self, score: f32) -> GoalComplexity {
        let [simple_max, moderate_max, complex_max] = self.config.thresholds;
        if score < simple_max {
            GoalComplexity::Simple
        } else if score < moderate_max {
            GoalComplexity::Moderate
        } else if score < complex_max {
            GoalComplexity::Complex
        } else {
            GoalComplexity::Critical
        }
    }

    /// Build a human-readable reasoning string explaining the classification.
    fn build_reasoning(
        &self,
        factors: &[ScoringFactor],
        aggregate: f32,
        complexity: GoalComplexity,
        security_override: bool,
    ) -> String {
        let mut parts = Vec::new();

        if security_override {
            parts.push(
                "CRITICAL OVERRIDE: Security surface requires external_act scope.".to_string(),
            );
        }

        parts.push(format!(
            "Aggregate score: {:.3} -> {:?}.",
            aggregate, complexity
        ));

        // List top contributing factors (weighted_score > 0).
        let mut sorted_factors: Vec<&ScoringFactor> = factors.iter().collect();
        sorted_factors.sort_by(|a, b| {
            b.weighted_score
                .partial_cmp(&a.weighted_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        parts.push("Top factors:".to_string());
        for factor in sorted_factors.iter().take(3) {
            if factor.weighted_score > 0.0 {
                parts.push(format!(
                    "  - {} (raw={:.1}, weighted={:.3})",
                    factor.name, factor.raw_score, factor.weighted_score
                ));
            }
        }

        parts.join(" ")
    }
}

// --- Private scoring helper functions ---

/// Score the number of domains involved.
/// 1 domain -> 0, 2 domains -> 1, 3+ domains -> 2 (capped).
fn score_domain_count(domains: &[String]) -> f32 {
    match domains.len() {
        0 | 1 => 0.0,
        2 => 1.0,
        _ => 2.0,
    }
}

/// Score the number of execution phases.
/// 1 phase -> 0, 2-3 phases -> 1, 4+ phases -> 2 (capped).
fn score_phase_count(phases: &[String]) -> f32 {
    match phases.len() {
        0 | 1 => 0.0,
        2 | 3 => 1.0,
        _ => 2.0,
    }
}

/// Score workflow novelty based on template availability.
/// has_known_template=true -> 0, template_name.is_some() but !has_known_template -> 1,
/// !has_known_template && template_name.is_none() -> 2 (capped).
fn score_workflow_novelty(has_template: bool, template_name: &Option<String>) -> f32 {
    if has_template {
        0.0
    } else if template_name.is_some() {
        1.0
    } else {
        2.0
    }
}

/// Score external dependencies.
/// 0 deps -> 0, deps without "credential" -> 1, any dep containing "credential" -> 2,
/// any dep containing "financial" or "legal" -> 3 (Critical).
fn score_external_deps(deps: &[String]) -> f32 {
    if deps.is_empty() {
        return 0.0;
    }

    let has_financial_or_legal = deps
        .iter()
        .any(|d| d.contains("financial") || d.contains("legal"));
    if has_financial_or_legal {
        return 3.0;
    }

    let has_credential = deps.iter().any(|d| d.contains("credential"));
    if has_credential {
        return 2.0;
    }

    1.0
}

/// Score estimated cost in USD.
/// < 0.50 -> 0, 0.50-5.0 -> 1, 5.0-50.0 -> 2, > 50.0 -> 3 (Critical).
fn score_estimated_cost(cost: Option<f64>) -> f32 {
    match cost {
        None => 0.0,
        Some(c) if c < 0.50 => 0.0,
        Some(c) if c <= 5.0 => 1.0,
        Some(c) if c <= 50.0 => 2.0,
        Some(_) => 3.0,
    }
}

/// Score security surface based on required scopes.
/// All read-only -> 0, any write scope -> 1, any credential scope -> 2,
/// any scope containing "external_act" -> 3 (Critical).
fn score_security_surface(scopes: &[String]) -> f32 {
    if scopes.is_empty() {
        return 0.0;
    }

    let has_external_act = scopes.iter().any(|s| s.contains("external_act"));
    if has_external_act {
        return 3.0;
    }

    let has_credential = scopes.iter().any(|s| s.contains("credential"));
    if has_credential {
        return 2.0;
    }

    let has_write = scopes.iter().any(|s| s.contains("write"));
    if has_write {
        return 1.0;
    }

    0.0
}

/// Score autonomy level.
/// Auto -> 0, Semi -> 1, Manual -> 2 (capped).
fn score_autonomy(level: AutonomyLevel) -> f32 {
    match level {
        AutonomyLevel::Auto => 0.0,
        AutonomyLevel::Semi => 1.0,
        AutonomyLevel::Manual => 2.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to build a minimal GoalMetadata with defaults for easy test construction.
    fn base_metadata() -> GoalMetadata {
        GoalMetadata {
            title: "Test goal".to_string(),
            description: "A test goal".to_string(),
            domains: vec![],
            phases: vec![],
            has_known_template: false,
            template_name: None,
            external_dependencies: vec![],
            required_scopes: vec![],
            estimated_cost_usd: None,
            autonomy_level: AutonomyLevel::Auto,
            constraints: None,
        }
    }

    #[test]
    fn test_simple_goal_classification() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Send daily digest".to_string(),
            description: "Send a summary email".to_string(),
            domains: vec!["email".to_string()],
            phases: vec!["execute".to_string()],
            has_known_template: true,
            template_name: Some("daily-digest".to_string()),
            external_dependencies: vec![],
            required_scopes: vec!["archive.read".to_string()],
            estimated_cost_usd: Some(0.10),
            autonomy_level: AutonomyLevel::Auto,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(assessment.complexity, GoalComplexity::Simple);
        assert!(
            assessment.aggregate_score < 0.30,
            "Expected aggregate < 0.30, got {}",
            assessment.aggregate_score
        );
    }

    #[test]
    fn test_moderate_goal_classification() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Research and summarize topic".to_string(),
            description: "Research a topic across web and archive".to_string(),
            domains: vec!["web".to_string(), "archive".to_string()],
            phases: vec!["research".to_string(), "summarize".to_string()],
            has_known_template: false,
            template_name: Some("research-summary".to_string()),
            external_dependencies: vec!["web-api".to_string()],
            required_scopes: vec!["archive.write".to_string()],
            estimated_cost_usd: Some(2.0),
            autonomy_level: AutonomyLevel::Semi,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Moderate,
            "Expected Moderate, got {:?} (score={})",
            assessment.complexity,
            assessment.aggregate_score
        );
        assert!(assessment.aggregate_score >= 0.30);
        assert!(assessment.aggregate_score < 0.60);
    }

    #[test]
    fn test_complex_goal_classification() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Multi-domain deployment".to_string(),
            description: "Deploy across infrastructure, DNS, and monitoring".to_string(),
            domains: vec![
                "infrastructure".to_string(),
                "dns".to_string(),
                "monitoring".to_string(),
            ],
            phases: vec![
                "plan".to_string(),
                "provision".to_string(),
                "deploy".to_string(),
                "verify".to_string(),
            ],
            has_known_template: false,
            template_name: None,
            external_dependencies: vec!["aws-credential".to_string()],
            required_scopes: vec!["credential.read".to_string()],
            estimated_cost_usd: Some(20.0),
            autonomy_level: AutonomyLevel::Manual,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Complex,
            "Expected Complex, got {:?} (score={})",
            assessment.complexity,
            assessment.aggregate_score
        );
        assert!(assessment.aggregate_score >= 0.60);
        assert!(assessment.aggregate_score < 0.85);
    }

    #[test]
    fn test_critical_via_score() {
        // The max aggregate score without external_act is ~0.75 (all factors
        // at their caps). Use a custom config with complex_max=0.70 so the
        // high score maps to Critical purely via the threshold, not the
        // security override.
        let config = ClassifierConfig {
            thresholds: [0.25, 0.50, 0.70],
            ..ClassifierConfig::default()
        };
        let classifier = GoalComplexityClassifier::new(config);
        let metadata = GoalMetadata {
            title: "Full financial migration".to_string(),
            description: "Migrate all financial data across systems".to_string(),
            domains: vec![
                "finance".to_string(),
                "compliance".to_string(),
                "infrastructure".to_string(),
            ],
            phases: vec![
                "audit".to_string(),
                "plan".to_string(),
                "migrate".to_string(),
                "verify".to_string(),
                "rollback-plan".to_string(),
            ],
            has_known_template: false,
            template_name: None,
            external_dependencies: vec!["financial-api".to_string()],
            required_scopes: vec!["credential.write".to_string()],
            estimated_cost_usd: Some(100.0),
            autonomy_level: AutonomyLevel::Manual,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Critical,
            "Expected Critical, got {:?} (score={})",
            assessment.complexity,
            assessment.aggregate_score
        );
        // Verify it was score-based, not a security override.
        assert!(
            assessment.aggregate_score >= 0.70,
            "Score should be >= 0.70 for Critical threshold"
        );
        assert!(
            !assessment.reasoning.contains("CRITICAL OVERRIDE"),
            "Should be score-based, not security override"
        );
    }

    #[test]
    fn test_critical_override_via_security() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        // Build a goal that would otherwise score Simple, but has external_act scope.
        let metadata = GoalMetadata {
            title: "Simple external action".to_string(),
            description: "A simple task that requires external action".to_string(),
            domains: vec!["notifications".to_string()],
            phases: vec!["execute".to_string()],
            has_known_template: true,
            template_name: Some("notify".to_string()),
            external_dependencies: vec![],
            required_scopes: vec!["external_act.send".to_string()],
            estimated_cost_usd: Some(0.01),
            autonomy_level: AutonomyLevel::Auto,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Critical,
            "External act scope should force Critical regardless of aggregate score"
        );
        // Aggregate score itself should be low since everything else is simple.
        assert!(
            assessment.aggregate_score < 0.30,
            "Aggregate score should be low; the Critical comes from security override"
        );
        assert!(assessment.reasoning.contains("CRITICAL OVERRIDE"));
    }

    #[test]
    fn test_default_config() {
        let config = ClassifierConfig::default();
        assert_eq!(config.thresholds, [0.30, 0.60, 0.85]);
        assert!(!config.use_llm_classification);
        assert!((config.boundary_tolerance - 0.05).abs() < f32::EPSILON);
    }

    #[test]
    fn test_scoring_factors_sum_to_one() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = base_metadata();
        let assessment = classifier.classify(&metadata);

        let weight_sum: f32 = assessment.factors.iter().map(|f| f.weight).sum();
        assert!(
            (weight_sum - 1.0).abs() < 0.001,
            "Factor weights should sum to ~1.0, got {}",
            weight_sum
        );
    }

    #[test]
    fn test_boundary_simple_moderate() {
        // Construct metadata that lands right at the 0.30 boundary.
        // We need aggregate ~= 0.30. Let's target exactly 0.30.
        //
        // Strategy: Use 2 domains (raw=1, weight=0.15, weighted=0.05),
        // 2 phases (raw=1, weight=0.15, weighted=0.05),
        // adapted template (raw=1, weight=0.20, weighted=0.0667),
        // 1 API dep (raw=1, weight=0.15, weighted=0.05),
        // cost $1 (raw=1, weight=0.10, weighted=0.0333),
        // read-only scopes (raw=0, weight=0.15, weighted=0),
        // Semi autonomy (raw=1, weight=0.10, weighted=0.0333).
        // Total = 0.05 + 0.05 + 0.0667 + 0.05 + 0.0333 + 0 + 0.0333 = 0.2833 -> Simple
        //
        // To get exactly at boundary, add a write scope (raw=1, weighted=0.05):
        // Total = 0.2833 + 0.05 = 0.3333 -> Moderate
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Boundary test".to_string(),
            description: "Near the simple/moderate boundary".to_string(),
            domains: vec!["a".to_string(), "b".to_string()],
            phases: vec!["p1".to_string(), "p2".to_string()],
            has_known_template: false,
            template_name: Some("adapted".to_string()),
            external_dependencies: vec!["api-dep".to_string()],
            required_scopes: vec!["archive.write".to_string()],
            estimated_cost_usd: Some(1.0),
            autonomy_level: AutonomyLevel::Semi,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Moderate,
            "Score {:.4} should be Moderate (>= 0.30)",
            assessment.aggregate_score
        );
        assert!(assessment.aggregate_score >= 0.30);
    }

    #[test]
    fn test_boundary_moderate_complex() {
        // Target aggregate ~= 0.60 (the moderate/complex boundary).
        // 3+ domains (raw=2, weighted=0.10), 4+ phases (raw=2, weighted=0.10),
        // no template (raw=2, weighted=0.1333), credential dep (raw=2, weighted=0.10),
        // cost $10 (raw=2, weighted=0.0667), credential scope (raw=2, weighted=0.10),
        // Manual (raw=2, weighted=0.0667).
        // Total = 0.10 + 0.10 + 0.1333 + 0.10 + 0.0667 + 0.10 + 0.0667 = 0.6667 -> Complex
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Moderate-complex boundary".to_string(),
            description: "Near the moderate/complex boundary".to_string(),
            domains: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            phases: vec![
                "p1".to_string(),
                "p2".to_string(),
                "p3".to_string(),
                "p4".to_string(),
            ],
            has_known_template: false,
            template_name: None,
            external_dependencies: vec!["some-credential-api".to_string()],
            required_scopes: vec!["credential.read".to_string()],
            estimated_cost_usd: Some(10.0),
            autonomy_level: AutonomyLevel::Manual,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Complex,
            "Score {:.4} should be Complex (>= 0.60)",
            assessment.aggregate_score
        );
        assert!(assessment.aggregate_score >= 0.60);
        assert!(assessment.aggregate_score < 0.85);
    }

    #[test]
    fn test_empty_metadata() {
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: String::new(),
            description: String::new(),
            domains: vec![],
            phases: vec![],
            has_known_template: false,
            template_name: None,
            external_dependencies: vec![],
            required_scopes: vec![],
            estimated_cost_usd: None,
            autonomy_level: AutonomyLevel::Auto,
            constraints: None,
        };

        let assessment = classifier.classify(&metadata);
        // With no template and no template name, workflow_novelty scores 2.0.
        // All other factors score 0. So aggregate = 0.20 * (2.0/3.0) = 0.1333.
        // That is below 0.30, so Simple.
        assert_eq!(
            assessment.complexity,
            GoalComplexity::Simple,
            "Empty metadata should classify as Simple, got {:?} (score={})",
            assessment.complexity,
            assessment.aggregate_score
        );
    }

    #[test]
    fn test_serde_roundtrip() {
        // GoalComplexity roundtrip
        let complexity = GoalComplexity::Complex;
        let json = serde_json::to_string(&complexity).expect("serialize GoalComplexity");
        let deserialized: GoalComplexity =
            serde_json::from_str(&json).expect("deserialize GoalComplexity");
        assert_eq!(complexity, deserialized);

        // ComplexityAssessment roundtrip
        let assessment = ComplexityAssessment {
            complexity: GoalComplexity::Moderate,
            aggregate_score: 0.45,
            factors: vec![ScoringFactor {
                name: "domain_count".to_string(),
                weight: 0.15,
                raw_score: 1.0,
                weighted_score: 0.05,
            }],
            llm_override: None,
            reasoning: "Test reasoning".to_string(),
        };
        let json = serde_json::to_string(&assessment).expect("serialize ComplexityAssessment");
        let deserialized: ComplexityAssessment =
            serde_json::from_str(&json).expect("deserialize ComplexityAssessment");
        assert_eq!(deserialized.complexity, GoalComplexity::Moderate);
        assert!((deserialized.aggregate_score - 0.45).abs() < f32::EPSILON);
        assert_eq!(deserialized.factors.len(), 1);
        assert_eq!(deserialized.reasoning, "Test reasoning");
    }

    #[test]
    fn test_goal_metadata_serde() {
        let metadata = GoalMetadata {
            title: "Test goal".to_string(),
            description: "A test".to_string(),
            domains: vec!["web".to_string()],
            phases: vec!["plan".to_string(), "execute".to_string()],
            has_known_template: true,
            template_name: Some("web-scrape".to_string()),
            external_dependencies: vec!["http-api".to_string()],
            required_scopes: vec!["archive.read".to_string()],
            estimated_cost_usd: Some(0.25),
            autonomy_level: AutonomyLevel::Semi,
            constraints: Some(super::super::types::GoalConstraints {
                max_budget_usd: Some(10.0),
                max_duration_secs: Some(300),
                deadline: None,
            }),
        };

        let json = serde_json::to_string(&metadata).expect("serialize GoalMetadata");
        let deserialized: GoalMetadata =
            serde_json::from_str(&json).expect("deserialize GoalMetadata");

        assert_eq!(deserialized.title, "Test goal");
        assert_eq!(deserialized.domains.len(), 1);
        assert_eq!(deserialized.phases.len(), 2);
        assert!(deserialized.has_known_template);
        assert_eq!(deserialized.template_name, Some("web-scrape".to_string()));
        assert_eq!(deserialized.autonomy_level, AutonomyLevel::Semi);
        let constraints = deserialized.constraints.expect("constraints should exist");
        assert_eq!(constraints.max_budget_usd, Some(10.0));
        assert_eq!(constraints.max_duration_secs, Some(300));
        assert!(constraints.deadline.is_none());
    }

    #[test]
    fn test_autonomy_level_default() {
        let level = AutonomyLevel::default();
        assert_eq!(level, AutonomyLevel::Semi);
    }

    #[tokio::test]
    async fn test_classify_with_llm_delegates() {
        use crate::llm::{ChatMessage, LlmClient};
        use anyhow::Result;

        struct MockLlm;

        #[async_trait::async_trait]
        impl LlmClient for MockLlm {
            async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
                Ok("mock response".to_string())
            }
        }

        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let metadata = GoalMetadata {
            title: "Simple task".to_string(),
            description: "A simple task".to_string(),
            domains: vec!["web".to_string()],
            phases: vec!["execute".to_string()],
            has_known_template: true,
            template_name: Some("simple".to_string()),
            external_dependencies: vec![],
            required_scopes: vec![],
            estimated_cost_usd: None,
            autonomy_level: AutonomyLevel::Auto,
            constraints: None,
        };

        let sync_result = classifier.classify(&metadata);
        let async_result = classifier.classify_with_llm(&metadata, &MockLlm).await;

        assert_eq!(sync_result.complexity, async_result.complexity);
        assert!((sync_result.aggregate_score - async_result.aggregate_score).abs() < f32::EPSILON);
        assert_eq!(sync_result.factors.len(), async_result.factors.len());
    }
}
