//! Plan generator for the deliberation-first pipeline.
//!
//! Creates `ExecutionPlan` instances from goal metadata, templates, LLM synthesis,
//! or council verdicts. The plan generator is the bridge between classification
//! (what kind of goal is this?) and execution (what phases/validations should run?).

use std::collections::HashMap;

use anyhow::Result;
use serde::Deserialize;

use crate::council::CouncilVerdict;
use crate::execution_plan::{ExecutionPlan, Phase, RollbackStrategy, Validation};
use crate::llm::{ChatMessage, LlmClient};

use super::types::GoalMetadata;

// ---------------------------------------------------------------------------
// Plan Generator
// ---------------------------------------------------------------------------

/// Generates execution plans from goal metadata via templates, heuristics, or LLM synthesis.
pub struct PlanGenerator {
    /// Known workflow templates (template_name -> ExecutionPlan).
    templates: HashMap<String, ExecutionPlan>,
}

impl PlanGenerator {
    /// Create a new plan generator with the given templates.
    pub fn new(templates: HashMap<String, ExecutionPlan>) -> Self {
        Self { templates }
    }

    /// Clone and return a template by name, if it exists.
    pub fn from_template(&self, template_name: &str) -> Option<ExecutionPlan> {
        self.templates.get(template_name).cloned()
    }

    /// Generate a minimal single-phase plan for Simple goals.
    pub fn generate_brief(&self, metadata: &GoalMetadata) -> ExecutionPlan {
        let mut validations = vec![Validation::LintClean {
            command: "cargo clippy".to_string(),
        }];

        if !metadata.phases.is_empty() {
            validations.push(Validation::TestPass {
                test_pattern: "cargo test".to_string(),
                description: "All tests pass".to_string(),
            });
        }

        ExecutionPlan {
            name: format!("brief: {}", metadata.title),
            phases: vec![Phase {
                name: "execute".to_string(),
                description: metadata.description.clone(),
                validations,
            }],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    /// Generate a multi-phase plan for Moderate goals.
    ///
    /// Creates phases from metadata.phases (or defaults to ["plan", "implement", "verify"]).
    /// Each phase gets appropriate validations based on its name and position.
    pub fn generate_full(&self, metadata: &GoalMetadata) -> ExecutionPlan {
        let phase_names = if metadata.phases.is_empty() {
            default_phases()
        } else {
            metadata.phases.clone()
        };

        let total = phase_names.len();
        let phases: Vec<Phase> = phase_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_last = i == total - 1;
                Phase {
                    name: name.clone(),
                    description: format!("Phase: {name}"),
                    validations: validations_for_phase(name, is_last, metadata),
                }
            })
            .collect();

        let rollback_strategy = if metadata.required_scopes.iter().any(|s| s.contains("write")) {
            RollbackStrategy::GitReset {
                target_ref: "HEAD~1".to_string(),
            }
        } else {
            RollbackStrategy::None
        };

        ExecutionPlan {
            name: format!("full: {}", metadata.title),
            phases,
            rollback_strategy,
        }
    }

    /// Generate a plan via LLM synthesis when no template matches.
    ///
    /// Builds a prompt asking the LLM to generate a JSON execution plan, parses the
    /// response, and returns (plan, confidence). Falls back to `generate_full()` with
    /// confidence 0.5 if LLM parsing fails.
    pub async fn synthesize(
        &self,
        metadata: &GoalMetadata,
        llm: &dyn LlmClient,
    ) -> Result<(ExecutionPlan, f32)> {
        let domains = metadata.domains.join(", ");
        let phases = if metadata.phases.is_empty() {
            "plan, implement, verify".to_string()
        } else {
            metadata.phases.join(", ")
        };

        let prompt = format!(
            "You are a planning agent. Generate an execution plan for the following goal.\n\n\
             Title: {}\n\
             Description: {}\n\
             Domains: {}\n\
             Suggested phases: {}\n\n\
             Respond in JSON format:\n\
             {{\n\
               \"plan_name\": \"...\",\n\
               \"phases\": [\n\
                 {{\"name\": \"...\", \"description\": \"...\", \"validations\": [\"lint\", \"test\"]}}\n\
               ],\n\
               \"confidence\": 0.0-1.0,\n\
               \"rollback\": \"none\" | \"git_reset\"\n\
             }}",
            metadata.title, metadata.description, domains, phases,
        );

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "You are a planning agent that generates execution plans in JSON."
                    .to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: prompt,
            },
        ];

        let response = llm.chat(&messages, true).await?;

        match serde_json::from_str::<SynthesizedPlan>(&response) {
            Ok(parsed) => {
                let confidence = parsed.confidence.unwrap_or(0.7).clamp(0.0, 1.0);

                let rollback_strategy = match parsed.rollback.as_deref() {
                    Some("git_reset") => RollbackStrategy::GitReset {
                        target_ref: "HEAD~1".to_string(),
                    },
                    _ => RollbackStrategy::None,
                };

                let phases = parsed
                    .phases
                    .into_iter()
                    .map(|sp| Phase {
                        name: sp.name,
                        description: sp.description,
                        validations: sp
                            .validations
                            .iter()
                            .filter_map(|v| map_validation_string(v))
                            .collect(),
                    })
                    .collect();

                let plan = ExecutionPlan {
                    name: parsed.plan_name,
                    phases,
                    rollback_strategy,
                };

                Ok((plan, confidence))
            }
            Err(_) => {
                // Fall back to heuristic plan with lower confidence.
                let plan = self.generate_full(metadata);
                Ok((plan, 0.5))
            }
        }
    }

    /// Generate a plan from a council verdict.
    ///
    /// Uses the verdict's synthesis as the plan name, creates phases from metadata,
    /// and adds human review / approval gates based on disagreements and confidence.
    pub fn from_verdict(
        &self,
        verdict: &CouncilVerdict,
        metadata: &GoalMetadata,
    ) -> Result<ExecutionPlan> {
        let phase_names = if metadata.phases.is_empty() {
            default_phases()
        } else {
            metadata.phases.clone()
        };

        let total = phase_names.len();
        let mut phases: Vec<Phase> = phase_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_last = i == total - 1;
                Phase {
                    name: name.clone(),
                    description: format!("Phase: {name}"),
                    validations: validations_for_phase(name, is_last, metadata),
                }
            })
            .collect();

        // Add HumanReview for each disagreement on the last phase.
        if let Some(last_phase) = phases.last_mut() {
            for disagreement in &verdict.disagreements {
                last_phase.validations.push(Validation::HumanReview {
                    reviewer: "user".to_string(),
                    criteria: disagreement.clone(),
                });
            }

            // Low confidence -> require explicit human approval.
            if verdict.confidence < 0.7 {
                last_phase.validations.push(Validation::HumanApproval);
            }
        }

        Ok(ExecutionPlan {
            name: verdict.synthesis.clone(),
            phases,
            rollback_strategy: RollbackStrategy::GitReset {
                target_ref: "HEAD~1".to_string(),
            },
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default phase names when metadata doesn't specify any.
fn default_phases() -> Vec<String> {
    vec![
        "plan".to_string(),
        "implement".to_string(),
        "verify".to_string(),
    ]
}

/// Determine appropriate validations for a phase based on its name/position.
fn validations_for_phase(
    phase_name: &str,
    is_last: bool,
    metadata: &GoalMetadata,
) -> Vec<Validation> {
    let lower = phase_name.to_ascii_lowercase();
    let mut validations = Vec::new();

    // Planning/design phases get no automatic validations.
    let is_planning = lower == "plan" || lower == "design";

    if !is_planning {
        // Implementation phases get lint + test.
        validations.push(Validation::LintClean {
            command: "cargo clippy".to_string(),
        });
        validations.push(Validation::TestPass {
            test_pattern: "cargo test".to_string(),
            description: "All tests pass".to_string(),
        });
    }

    // Last phase gets human review.
    if is_last && !is_planning {
        validations.push(Validation::HumanReview {
            reviewer: "user".to_string(),
            criteria: format!("Review phase: {phase_name}"),
        });
    }

    // Security-sensitive phases get an expert agent.
    if lower.contains("security") || lower.contains("auth") {
        validations.push(Validation::ExpertAgent {
            agent_type: "security-reviewer".to_string(),
            criteria: format!("Security review for phase: {phase_name}"),
            system_prompt: None,
        });
    }

    // Suppress the empty-plan-phase-has-no-validations concern: planning phases
    // intentionally have zero validations since they produce documents, not code.
    let _ = metadata;

    validations
}

/// Map a validation string from LLM output to a `Validation` variant.
fn map_validation_string(s: &str) -> Option<Validation> {
    match s.to_ascii_lowercase().as_str() {
        "lint" => Some(Validation::LintClean {
            command: "cargo clippy".to_string(),
        }),
        "test" => Some(Validation::TestPass {
            test_pattern: "cargo test".to_string(),
            description: "All tests pass".to_string(),
        }),
        "human_review" => Some(Validation::HumanReview {
            reviewer: "user".to_string(),
            criteria: "Review required".to_string(),
        }),
        "human_approval" => Some(Validation::HumanApproval),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// LLM Synthesis Deserialization
// ---------------------------------------------------------------------------

/// Deserialization target for LLM-generated execution plans.
#[derive(Debug, Deserialize)]
struct SynthesizedPlan {
    plan_name: String,
    phases: Vec<SynthesizedPhase>,
    confidence: Option<f32>,
    rollback: Option<String>,
}

/// A single phase from the LLM synthesis response.
#[derive(Debug, Deserialize)]
struct SynthesizedPhase {
    name: String,
    description: String,
    validations: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::types::AutonomyLevel;

    // -- Mock LLM --

    struct MockLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            Ok(self.response.clone())
        }
    }

    // -- Test helpers --

    fn make_metadata(title: &str, phases: Vec<&str>, scopes: Vec<&str>) -> GoalMetadata {
        GoalMetadata {
            title: title.to_string(),
            description: format!("Description for {title}"),
            domains: vec!["code".to_string()],
            phases: phases.into_iter().map(|s| s.to_string()).collect(),
            has_known_template: false,
            template_name: None,
            external_dependencies: vec![],
            required_scopes: scopes.into_iter().map(|s| s.to_string()).collect(),
            estimated_cost_usd: None,
            autonomy_level: AutonomyLevel::Semi,
            constraints: None,
        }
    }

    fn make_generator() -> PlanGenerator {
        PlanGenerator::new(HashMap::new())
    }

    fn make_generator_with_template(name: &str, plan: ExecutionPlan) -> PlanGenerator {
        let mut templates = HashMap::new();
        templates.insert(name.to_string(), plan);
        PlanGenerator::new(templates)
    }

    fn sample_plan(name: &str) -> ExecutionPlan {
        ExecutionPlan {
            name: name.to_string(),
            phases: vec![Phase {
                name: "build".to_string(),
                description: "Build it".to_string(),
                validations: vec![Validation::LintClean {
                    command: "cargo clippy".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    fn make_verdict(synthesis: &str, confidence: f32, disagreements: Vec<&str>) -> CouncilVerdict {
        CouncilVerdict {
            synthesis: synthesis.to_string(),
            member_analyses: vec![],
            confidence,
            disagreements: disagreements.into_iter().map(|s| s.to_string()).collect(),
        }
    }

    // -- Template tests --

    #[test]
    fn test_from_template_found() {
        let plan = sample_plan("deploy-workflow");
        let gen = make_generator_with_template("deploy", plan);

        let result = gen.from_template("deploy");
        assert!(result.is_some());
        let plan = result.unwrap();
        assert_eq!(plan.name, "deploy-workflow");
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].name, "build");
    }

    #[test]
    fn test_from_template_not_found() {
        let gen = make_generator();
        let result = gen.from_template("nonexistent");
        assert!(result.is_none());
    }

    // -- Brief plan tests --

    #[test]
    fn test_generate_brief_single_phase() {
        let gen = make_generator();
        let metadata = make_metadata("Quick fix", vec!["patch"], vec![]);

        let plan = gen.generate_brief(&metadata);
        assert_eq!(plan.name, "brief: Quick fix");
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].name, "execute");
        assert!(matches!(plan.rollback_strategy, RollbackStrategy::None));
    }

    #[test]
    fn test_generate_brief_with_phases() {
        let gen = make_generator();
        let metadata = make_metadata("Fix tests", vec!["implement", "verify"], vec![]);

        let plan = gen.generate_brief(&metadata);
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].name, "execute");
        // Should have LintClean + TestPass since metadata has phases.
        assert_eq!(plan.phases[0].validations.len(), 2);
        assert!(matches!(
            plan.phases[0].validations[0],
            Validation::LintClean { .. }
        ));
        assert!(matches!(
            plan.phases[0].validations[1],
            Validation::TestPass { .. }
        ));
    }

    #[test]
    fn test_generate_brief_no_phases() {
        let gen = make_generator();
        let metadata = make_metadata("Simple query", vec![], vec![]);

        let plan = gen.generate_brief(&metadata);
        assert_eq!(plan.phases.len(), 1);
        // Only LintClean, no TestPass.
        assert_eq!(plan.phases[0].validations.len(), 1);
        assert!(matches!(
            plan.phases[0].validations[0],
            Validation::LintClean { .. }
        ));
    }

    // -- Full plan tests --

    #[test]
    fn test_generate_full_default_phases() {
        let gen = make_generator();
        let metadata = make_metadata("Refactor module", vec![], vec![]);

        let plan = gen.generate_full(&metadata);
        assert_eq!(plan.phases.len(), 3);
        assert_eq!(plan.phases[0].name, "plan");
        assert_eq!(plan.phases[1].name, "implement");
        assert_eq!(plan.phases[2].name, "verify");
        // Plan phase should have no validations.
        assert!(plan.phases[0].validations.is_empty());
        // Implement phase should have lint + test (not last, so no human review).
        assert_eq!(plan.phases[1].validations.len(), 2);
        // Verify phase (last) should have lint + test + human review.
        assert_eq!(plan.phases[2].validations.len(), 3);
    }

    #[test]
    fn test_generate_full_custom_phases() {
        let gen = make_generator();
        let metadata = make_metadata("Deploy service", vec!["design", "code", "deploy"], vec![]);

        let plan = gen.generate_full(&metadata);
        assert_eq!(plan.phases.len(), 3);
        assert_eq!(plan.phases[0].name, "design");
        assert_eq!(plan.phases[1].name, "code");
        assert_eq!(plan.phases[2].name, "deploy");
        // Design is a planning phase -> no validations.
        assert!(plan.phases[0].validations.is_empty());
        // Code is a middle phase -> lint + test.
        assert_eq!(plan.phases[1].validations.len(), 2);
        // Deploy is last -> lint + test + human review.
        assert_eq!(plan.phases[2].validations.len(), 3);
    }

    #[test]
    fn test_generate_full_security_phase() {
        let gen = make_generator();
        let metadata = make_metadata(
            "Security audit",
            vec!["plan", "security-audit", "verify"],
            vec![],
        );

        let plan = gen.generate_full(&metadata);
        assert_eq!(plan.phases.len(), 3);
        // Security-audit phase (middle) should have lint + test + ExpertAgent.
        let sec_phase = &plan.phases[1];
        assert_eq!(sec_phase.name, "security-audit");
        let has_expert = sec_phase.validations.iter().any(|v| {
            matches!(v, Validation::ExpertAgent { agent_type, .. } if agent_type == "security-reviewer")
        });
        assert!(
            has_expert,
            "security phase should have ExpertAgent validation"
        );
    }

    #[test]
    fn test_generate_full_write_scope_rollback() {
        let gen = make_generator();
        let metadata = make_metadata("Write data", vec!["implement"], vec!["archive.write"]);

        let plan = gen.generate_full(&metadata);
        assert!(
            matches!(plan.rollback_strategy, RollbackStrategy::GitReset { .. }),
            "write scope should trigger GitReset rollback"
        );
    }

    #[test]
    fn test_generate_full_no_write_scope() {
        let gen = make_generator();
        let metadata = make_metadata("Read data", vec!["implement"], vec!["archive.read"]);

        let plan = gen.generate_full(&metadata);
        assert!(
            matches!(plan.rollback_strategy, RollbackStrategy::None),
            "read-only scope should have no rollback"
        );
    }

    // -- Verdict tests --

    #[test]
    fn test_from_verdict_basic() {
        let gen = make_generator();
        let verdict = make_verdict("Use approach A", 0.85, vec![]);
        let metadata = make_metadata("Feature X", vec!["plan", "implement", "verify"], vec![]);

        let plan = gen.from_verdict(&verdict, &metadata).unwrap();
        assert_eq!(plan.name, "Use approach A");
        assert_eq!(plan.phases.len(), 3);
        assert!(matches!(
            plan.rollback_strategy,
            RollbackStrategy::GitReset { .. }
        ));
    }

    #[test]
    fn test_from_verdict_with_disagreements() {
        let gen = make_generator();
        let verdict = make_verdict(
            "Compromise plan",
            0.75,
            vec!["Error handling approach", "API design"],
        );
        let metadata = make_metadata("API redesign", vec!["plan", "implement"], vec![]);

        let plan = gen.from_verdict(&verdict, &metadata).unwrap();
        let last_phase = plan.phases.last().unwrap();

        // Should have HumanReview for each disagreement.
        let human_reviews: Vec<_> = last_phase
            .validations
            .iter()
            .filter(|v| matches!(v, Validation::HumanReview { .. }))
            .collect();
        // 1 base HumanReview from validations_for_phase (last phase) + 2 disagreements = 3
        assert_eq!(
            human_reviews.len(),
            3,
            "should have base HumanReview + one per disagreement"
        );
    }

    #[test]
    fn test_from_verdict_low_confidence() {
        let gen = make_generator();
        let verdict = make_verdict("Uncertain plan", 0.5, vec![]);
        let metadata = make_metadata("Risky change", vec!["implement"], vec![]);

        let plan = gen.from_verdict(&verdict, &metadata).unwrap();
        let last_phase = plan.phases.last().unwrap();

        let has_approval = last_phase
            .validations
            .iter()
            .any(|v| matches!(v, Validation::HumanApproval));
        assert!(
            has_approval,
            "low confidence verdict should require HumanApproval"
        );
    }

    // -- Synthesize tests --

    #[tokio::test]
    async fn test_synthesize_with_mock_llm() {
        let gen = make_generator();
        let metadata = make_metadata("Build feature", vec!["design", "code"], vec![]);

        let llm = MockLlm {
            response: serde_json::json!({
                "plan_name": "synthesized-plan",
                "phases": [
                    {"name": "design", "description": "Design the feature", "validations": ["lint"]},
                    {"name": "code", "description": "Implement it", "validations": ["lint", "test"]},
                    {"name": "review", "description": "Final review", "validations": ["human_review"]}
                ],
                "confidence": 0.82,
                "rollback": "git_reset"
            })
            .to_string(),
        };

        let (plan, confidence) = gen.synthesize(&metadata, &llm).await.unwrap();
        assert_eq!(plan.name, "synthesized-plan");
        assert_eq!(plan.phases.len(), 3);
        assert_eq!(plan.phases[0].name, "design");
        assert_eq!(plan.phases[1].name, "code");
        assert_eq!(plan.phases[2].name, "review");
        assert!((confidence - 0.82).abs() < f32::EPSILON);
        assert!(matches!(
            plan.rollback_strategy,
            RollbackStrategy::GitReset { .. }
        ));

        // Verify validations were mapped correctly.
        assert_eq!(plan.phases[0].validations.len(), 1); // lint
        assert_eq!(plan.phases[1].validations.len(), 2); // lint + test
        assert_eq!(plan.phases[2].validations.len(), 1); // human_review
    }

    #[tokio::test]
    async fn test_synthesize_fallback_on_bad_json() {
        let gen = make_generator();
        let metadata = make_metadata("Broken plan", vec!["implement"], vec![]);

        let llm = MockLlm {
            response: "This is not valid JSON at all!!!".to_string(),
        };

        let (plan, confidence) = gen.synthesize(&metadata, &llm).await.unwrap();
        // Should fall back to generate_full().
        assert!(plan.name.starts_with("full: "));
        assert!((confidence - 0.5).abs() < f32::EPSILON);
    }

    // -- Serde roundtrip --

    #[test]
    fn test_plan_generator_serde() {
        let plan = ExecutionPlan {
            name: "template-plan".to_string(),
            phases: vec![
                Phase {
                    name: "build".to_string(),
                    description: "Build phase".to_string(),
                    validations: vec![
                        Validation::LintClean {
                            command: "cargo clippy".to_string(),
                        },
                        Validation::TestPass {
                            test_pattern: "cargo test".to_string(),
                            description: "Tests pass".to_string(),
                        },
                    ],
                },
                Phase {
                    name: "deploy".to_string(),
                    description: "Deploy phase".to_string(),
                    validations: vec![Validation::HumanApproval],
                },
            ],
            rollback_strategy: RollbackStrategy::GitReset {
                target_ref: "HEAD~1".to_string(),
            },
        };

        let json = serde_json::to_string(&plan).unwrap();
        let deserialized: ExecutionPlan = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.name, "template-plan");
        assert_eq!(deserialized.phases.len(), 2);
        assert_eq!(deserialized.phases[0].validations.len(), 2);
        assert_eq!(deserialized.phases[1].validations.len(), 1);
        assert!(matches!(
            deserialized.rollback_strategy,
            RollbackStrategy::GitReset { ref target_ref } if target_ref == "HEAD~1"
        ));
    }
}
