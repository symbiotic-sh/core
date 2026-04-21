//! Deliberation-First Goal Execution Pipeline.
//!
//! Routes goals through complexity-aware deliberation paths:
//! Simple -> auto-execute, Moderate -> plan + confidence gate,
//! Complex -> council deliberation, Critical -> mandatory human approval.

pub mod audit;
pub mod backend;
pub mod backend_claude;
pub mod backend_native;
pub mod backend_team;
pub mod classifier;
pub mod plan_gen;
pub mod refinement;
pub mod types;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::execution_plan::{ExecutionPlan, PlanResult};
use crate::llm::LlmClient;

use self::audit::{AuditEvent, AuditLog};
use self::backend::ExecutionBackend;
use self::classifier::{GoalComplexity, GoalComplexityClassifier};
use self::plan_gen::PlanGenerator;
use self::types::GoalSubmission;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Confidence threshold for auto-execution (default: 0.95).
    pub auto_execute_confidence: f32,
    /// Whether critical goals always require human approval (default: true).
    pub require_critical_approval: bool,
    /// Maximum number of concurrent goals (default: 5).
    pub max_concurrent_goals: usize,
    /// Default execution backend.
    pub default_backend: ExecutionBackend,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            auto_execute_confidence: 0.95,
            require_critical_approval: true,
            max_concurrent_goals: 5,
            default_backend: ExecutionBackend::Native,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PipelineOutcome {
    Executed {
        plan_result: PlanResult,
        audit_id: String,
    },
    AwaitingApproval {
        plan: ExecutionPlan,
        confidence: f32,
        audit_id: String,
    },
    Deliberating {
        council_session_id: String,
        audit_id: String,
    },
    Rejected {
        reason: String,
        audit_id: String,
    },
}

pub struct DeliberationPipeline {
    classifier: GoalComplexityClassifier,
    plan_generator: PlanGenerator,
    config: PipelineConfig,
    audit: AuditLog,
}

impl DeliberationPipeline {
    pub fn new(
        classifier: GoalComplexityClassifier,
        plan_generator: PlanGenerator,
        audit: AuditLog,
        config: PipelineConfig,
    ) -> Self {
        Self {
            classifier,
            plan_generator,
            config,
            audit,
        }
    }

    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Process a goal through the full deliberation pipeline.
    pub async fn process_goal(
        &self,
        goal: &GoalSubmission,
        llm_clients: &[&dyn LlmClient],
    ) -> Result<PipelineOutcome> {
        let metadata = goal
            .metadata
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("goal submission missing metadata"))?;

        // Record submission.
        let audit_id = format!("audit-{}", goal.id);
        self.audit.record(&AuditEvent::GoalSubmitted {
            goal_id: goal.id.clone(),
            source: goal.source.clone(),
            timestamp: crate::now_unix(),
        })?;

        // Classify complexity.
        let assessment = self.classifier.classify(metadata);
        self.audit.record(&AuditEvent::ComplexityAssessed {
            goal_id: goal.id.clone(),
            assessment: assessment.clone(),
        })?;

        match assessment.complexity {
            GoalComplexity::Simple => self.execute_simple(goal, &audit_id).await,
            GoalComplexity::Moderate => self.execute_moderate(goal, llm_clients, &audit_id).await,
            GoalComplexity::Complex => self.execute_complex(goal, &audit_id).await,
            GoalComplexity::Critical => self.execute_critical(goal, &audit_id).await,
        }
    }

    /// Simple: generate brief plan, auto-execute (returns AwaitingApproval for now).
    async fn execute_simple(
        &self,
        goal: &GoalSubmission,
        audit_id: &str,
    ) -> Result<PipelineOutcome> {
        let metadata = goal.metadata.as_ref().unwrap();
        let plan = self.plan_generator.generate_brief(metadata);

        self.audit.record(&AuditEvent::PlanGenerated {
            goal_id: goal.id.clone(),
            plan_name: plan.name.clone(),
            phase_count: plan.phases.len(),
            validation_count: plan.total_validations(),
            confidence: 1.0,
        })?;

        // For now, return AwaitingApproval as we need a CommandRunner + HumanGate
        // to execute. The daemon integration (C7) will wire actual execution.
        Ok(PipelineOutcome::AwaitingApproval {
            plan,
            confidence: 1.0,
            audit_id: audit_id.to_string(),
        })
    }

    /// Moderate: generate plan, auto-execute if confidence >= threshold.
    async fn execute_moderate(
        &self,
        goal: &GoalSubmission,
        llm_clients: &[&dyn LlmClient],
        audit_id: &str,
    ) -> Result<PipelineOutcome> {
        let metadata = goal.metadata.as_ref().unwrap();

        let (plan, confidence) = if let Some(template) = metadata
            .template_name
            .as_ref()
            .and_then(|name| self.plan_generator.from_template(name))
        {
            (template, 0.95)
        } else if let Some(llm) = llm_clients.first() {
            self.plan_generator.synthesize(metadata, *llm).await?
        } else {
            (self.plan_generator.generate_full(metadata), 0.7)
        };

        self.audit.record(&AuditEvent::PlanGenerated {
            goal_id: goal.id.clone(),
            plan_name: plan.name.clone(),
            phase_count: plan.phases.len(),
            validation_count: plan.total_validations(),
            confidence,
        })?;

        Ok(PipelineOutcome::AwaitingApproval {
            plan,
            confidence,
            audit_id: audit_id.to_string(),
        })
    }

    /// Complex: flag for council deliberation.
    async fn execute_complex(
        &self,
        goal: &GoalSubmission,
        audit_id: &str,
    ) -> Result<PipelineOutcome> {
        let session_id = format!("council-{}", goal.id);
        self.audit.record(&AuditEvent::CouncilConvened {
            goal_id: goal.id.clone(),
            session_id: session_id.clone(),
            member_count: 0, // Will be set when council is actually invoked.
        })?;

        Ok(PipelineOutcome::Deliberating {
            council_session_id: session_id,
            audit_id: audit_id.to_string(),
        })
    }

    /// Critical: same as Complex + mandatory human approval.
    async fn execute_critical(
        &self,
        goal: &GoalSubmission,
        audit_id: &str,
    ) -> Result<PipelineOutcome> {
        if self.config.require_critical_approval {
            let metadata = goal.metadata.as_ref().unwrap();
            let plan = self.plan_generator.generate_full(metadata);

            self.audit.record(&AuditEvent::PlanGenerated {
                goal_id: goal.id.clone(),
                plan_name: plan.name.clone(),
                phase_count: plan.phases.len(),
                validation_count: plan.total_validations(),
                confidence: 0.0,
            })?;

            Ok(PipelineOutcome::AwaitingApproval {
                plan,
                confidence: 0.0,
                audit_id: audit_id.to_string(),
            })
        } else {
            self.execute_complex(goal, audit_id).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_plan::{
        ExecutionPlan, Phase, PhaseResult, RollbackStrategy, Validation, ValidationResult,
    };
    use crate::pipeline::classifier::ClassifierConfig;
    use crate::pipeline::types::{AutonomyLevel, GoalMetadata, GoalSource};
    use std::collections::HashMap;

    fn make_audit_log() -> (AuditLog, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let db_path = tmp.path().join("audit.db");
        let log_path = tmp.path().join("audit.jsonl");
        let audit = AuditLog::new(db_path, log_path).expect("create audit log");
        (audit, tmp)
    }

    fn make_pipeline() -> (DeliberationPipeline, tempfile::TempDir) {
        let (audit, tmp) = make_audit_log();
        let classifier = GoalComplexityClassifier::new(ClassifierConfig::default());
        let plan_generator = PlanGenerator::new(HashMap::new());
        let pipeline =
            DeliberationPipeline::new(classifier, plan_generator, audit, PipelineConfig::default());
        (pipeline, tmp)
    }

    fn simple_goal() -> GoalSubmission {
        GoalSubmission {
            id: "goal-simple".to_string(),
            title: "Send digest".to_string(),
            description: "Send a summary email".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            metadata: Some(GoalMetadata {
                title: "Send digest".to_string(),
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
            }),
        }
    }

    fn moderate_goal() -> GoalSubmission {
        GoalSubmission {
            id: "goal-moderate".to_string(),
            title: "Research topic".to_string(),
            description: "Research and summarize a topic".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            metadata: Some(GoalMetadata {
                title: "Research topic".to_string(),
                description: "Research and summarize a topic".to_string(),
                domains: vec!["web".to_string(), "archive".to_string()],
                phases: vec!["research".to_string(), "summarize".to_string()],
                has_known_template: false,
                template_name: Some("research-summary".to_string()),
                external_dependencies: vec!["web-api".to_string()],
                required_scopes: vec!["archive.write".to_string()],
                estimated_cost_usd: Some(2.0),
                autonomy_level: AutonomyLevel::Semi,
                constraints: None,
            }),
        }
    }

    fn complex_goal() -> GoalSubmission {
        GoalSubmission {
            id: "goal-complex".to_string(),
            title: "Multi-domain deploy".to_string(),
            description: "Deploy across infrastructure".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            metadata: Some(GoalMetadata {
                title: "Multi-domain deploy".to_string(),
                description: "Deploy across infrastructure".to_string(),
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
            }),
        }
    }

    fn critical_goal() -> GoalSubmission {
        GoalSubmission {
            id: "goal-critical".to_string(),
            title: "External action".to_string(),
            description: "Send external notification".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            metadata: Some(GoalMetadata {
                title: "External action".to_string(),
                description: "Send external notification".to_string(),
                domains: vec!["notifications".to_string()],
                phases: vec!["execute".to_string()],
                has_known_template: true,
                template_name: Some("notify".to_string()),
                external_dependencies: vec![],
                required_scopes: vec!["external_act.send".to_string()],
                estimated_cost_usd: Some(0.01),
                autonomy_level: AutonomyLevel::Auto,
                constraints: None,
            }),
        }
    }

    #[test]
    fn test_pipeline_config_default() {
        let config = PipelineConfig::default();
        assert!((config.auto_execute_confidence - 0.95).abs() < f32::EPSILON);
        assert!(config.require_critical_approval);
        assert_eq!(config.max_concurrent_goals, 5);
        assert!(matches!(config.default_backend, ExecutionBackend::Native));
    }

    #[test]
    fn test_pipeline_outcome_serde() {
        // Executed variant
        let outcome = PipelineOutcome::Executed {
            plan_result: PlanResult {
                plan_name: "test".to_string(),
                phase_results: vec![PhaseResult {
                    phase_name: "build".to_string(),
                    validation_results: vec![ValidationResult {
                        validation_label: "lint".to_string(),
                        passed: true,
                        output: "ok".to_string(),
                        duration_ms: 100,
                    }],
                    passed: true,
                    failed_at: None,
                }],
                passed: true,
                failed_at_phase: None,
                rollback_triggered: false,
            },
            audit_id: "audit-1".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let _: PipelineOutcome = serde_json::from_str(&json).unwrap();

        // AwaitingApproval variant
        let outcome = PipelineOutcome::AwaitingApproval {
            plan: ExecutionPlan {
                name: "plan".to_string(),
                phases: vec![Phase {
                    name: "p".to_string(),
                    description: "d".to_string(),
                    validations: vec![Validation::LintClean {
                        command: "cargo clippy".to_string(),
                    }],
                }],
                rollback_strategy: RollbackStrategy::None,
            },
            confidence: 0.85,
            audit_id: "audit-2".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let _: PipelineOutcome = serde_json::from_str(&json).unwrap();

        // Deliberating variant
        let outcome = PipelineOutcome::Deliberating {
            council_session_id: "session-1".to_string(),
            audit_id: "audit-3".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let _: PipelineOutcome = serde_json::from_str(&json).unwrap();

        // Rejected variant
        let outcome = PipelineOutcome::Rejected {
            reason: "Too risky".to_string(),
            audit_id: "audit-4".to_string(),
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let _: PipelineOutcome = serde_json::from_str(&json).unwrap();
    }

    #[tokio::test]
    async fn test_process_goal_simple() {
        let (pipeline, _tmp) = make_pipeline();
        let goal = simple_goal();

        let outcome = pipeline.process_goal(&goal, &[]).await.unwrap();
        match outcome {
            PipelineOutcome::AwaitingApproval {
                plan, confidence, ..
            } => {
                assert!(plan.name.starts_with("brief:"));
                assert!((confidence - 1.0).abs() < f32::EPSILON);
            }
            other => panic!("expected AwaitingApproval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_goal_moderate_no_llm() {
        let (pipeline, _tmp) = make_pipeline();
        let goal = moderate_goal();

        // No LLM clients -> falls back to generate_full with confidence 0.7.
        let outcome = pipeline.process_goal(&goal, &[]).await.unwrap();
        match outcome {
            PipelineOutcome::AwaitingApproval {
                plan, confidence, ..
            } => {
                assert!(plan.name.starts_with("full:"));
                assert!((confidence - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected AwaitingApproval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_goal_complex() {
        let (pipeline, _tmp) = make_pipeline();
        let goal = complex_goal();

        let outcome = pipeline.process_goal(&goal, &[]).await.unwrap();
        match outcome {
            PipelineOutcome::Deliberating {
                council_session_id, ..
            } => {
                assert!(council_session_id.contains("goal-complex"));
            }
            other => panic!("expected Deliberating, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_goal_critical() {
        let (pipeline, _tmp) = make_pipeline();
        let goal = critical_goal();

        let outcome = pipeline.process_goal(&goal, &[]).await.unwrap();
        match outcome {
            PipelineOutcome::AwaitingApproval { confidence, .. } => {
                assert!((confidence - 0.0).abs() < f32::EPSILON);
            }
            other => panic!("expected AwaitingApproval, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_process_goal_missing_metadata() {
        let (pipeline, _tmp) = make_pipeline();
        let goal = GoalSubmission {
            id: "goal-no-meta".to_string(),
            title: "No metadata".to_string(),
            description: "Missing metadata".to_string(),
            source: GoalSource::Api {
                client_id: "test".to_string(),
            },
            metadata: None,
        };

        let result = pipeline.process_goal(&goal, &[]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("missing metadata"));
    }
}
