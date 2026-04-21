//! PRD-First Execution: Phase-based task execution with validation gates.
//!
//! Complex tasks define ordered phases, each with validation criteria that must
//! pass before proceeding to the next phase. Supports:
//! - Progressive validation (cheap checks first, expensive later)
//! - Fail-fast on first validation failure
//! - Rollback to last known-good state
//! - Expert agent review (specialized LLM validators)
//! - Human approval gates
//!
//! Simple tasks skip the full PRD; this system activates for complex/critical work.

use std::fmt;
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::llm::{ChatMessage, LlmClient};

// ---------------------------------------------------------------------------
// Validation Types
// ---------------------------------------------------------------------------

/// A validation criterion for a phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Validation {
    /// Run a test command and check for success.
    TestPass {
        /// Shell command or test pattern to execute.
        test_pattern: String,
        /// Human-readable description of what this test validates.
        description: String,
    },

    /// Check that linting passes (no errors).
    LintClean {
        /// Lint command to run.
        command: String,
    },

    /// Request human review before proceeding.
    HumanReview {
        /// Who should review (role or person name).
        reviewer: String,
        /// What to review and what criteria to apply.
        criteria: String,
    },

    /// Spawn an expert agent for specialized validation.
    ExpertAgent {
        /// Type of expert (e.g., "security-reviewer", "performance-checker").
        agent_type: String,
        /// Criteria the expert should evaluate against.
        criteria: String,
        /// Optional system prompt override for the expert.
        system_prompt: Option<String>,
    },

    /// Require explicit human approval (blocking gate).
    HumanApproval,
}

impl Validation {
    /// Returns a sort key for progressive validation ordering.
    /// Lower values run first (cheaper checks before expensive ones).
    pub fn cost_order(&self) -> u8 {
        match self {
            Validation::LintClean { .. } => 0,
            Validation::TestPass { .. } => 1,
            Validation::ExpertAgent { .. } => 2,
            Validation::HumanReview { .. } => 3,
            Validation::HumanApproval => 4,
        }
    }

    /// Returns a human-readable label for this validation type.
    pub fn label(&self) -> &str {
        match self {
            Validation::TestPass { .. } => "test_pass",
            Validation::LintClean { .. } => "lint_clean",
            Validation::HumanReview { .. } => "human_review",
            Validation::ExpertAgent { .. } => "expert_agent",
            Validation::HumanApproval => "human_approval",
        }
    }
}

// ---------------------------------------------------------------------------
// Phase & Plan
// ---------------------------------------------------------------------------

/// A single phase in the execution plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Phase {
    /// Unique phase name (e.g., "design", "implement", "security_review").
    pub name: String,
    /// Description of what this phase accomplishes.
    pub description: String,
    /// Validation criteria that must pass before the next phase.
    /// Executed in progressive order (cheapest first).
    pub validations: Vec<Validation>,
}

/// Strategy for handling failures.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RollbackStrategy {
    /// No rollback; just report the failure.
    #[default]
    None,
    /// Rollback to a specific git commit/ref.
    GitReset { target_ref: String },
    /// Run a custom cleanup command.
    CustomCommand { command: String },
}

/// A complete execution plan (PRD) for a complex task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    /// Human-readable plan name.
    pub name: String,
    /// Ordered list of phases to execute.
    pub phases: Vec<Phase>,
    /// How to handle failures.
    pub rollback_strategy: RollbackStrategy,
}

impl ExecutionPlan {
    /// Create a simple plan with a single implementation phase and test validation.
    pub fn simple(name: &str, test_command: &str) -> Self {
        Self {
            name: name.to_string(),
            phases: vec![Phase {
                name: "implement".to_string(),
                description: "Implement and test".to_string(),
                validations: vec![
                    Validation::LintClean {
                        command: "cargo clippy".to_string(),
                    },
                    Validation::TestPass {
                        test_pattern: test_command.to_string(),
                        description: "All tests pass".to_string(),
                    },
                ],
            }],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    /// Returns the total number of validations across all phases.
    pub fn total_validations(&self) -> usize {
        self.phases.iter().map(|p| p.validations.len()).sum()
    }
}

// ---------------------------------------------------------------------------
// Validation Result
// ---------------------------------------------------------------------------

/// The outcome of running a single validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    /// Which validation was run.
    pub validation_label: String,
    /// Whether the validation passed.
    pub passed: bool,
    /// Output or feedback from the validation.
    pub output: String,
    /// How long the validation took.
    pub duration_ms: u64,
}

/// The outcome of executing an entire phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhaseResult {
    /// Phase name.
    pub phase_name: String,
    /// Results for each validation in this phase.
    pub validation_results: Vec<ValidationResult>,
    /// Whether all validations passed.
    pub passed: bool,
    /// Index of the first failed validation, if any.
    pub failed_at: Option<usize>,
}

/// The outcome of executing an entire plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanResult {
    /// Plan name.
    pub plan_name: String,
    /// Results for each phase.
    pub phase_results: Vec<PhaseResult>,
    /// Whether all phases passed.
    pub passed: bool,
    /// Index of the first failed phase, if any.
    pub failed_at_phase: Option<usize>,
    /// Whether rollback was triggered.
    pub rollback_triggered: bool,
}

impl fmt::Display for PlanResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status = if self.passed { "PASSED" } else { "FAILED" };
        writeln!(f, "## Plan: {} [{}]", self.plan_name, status)?;
        for pr in &self.phase_results {
            let phase_status = if pr.passed { "OK" } else { "FAIL" };
            writeln!(f, "\n### Phase: {} [{}]", pr.phase_name, phase_status)?;
            for vr in &pr.validation_results {
                let mark = if vr.passed { "PASS" } else { "FAIL" };
                writeln!(
                    f,
                    "  - [{}] {} ({}ms): {}",
                    mark,
                    vr.validation_label,
                    vr.duration_ms,
                    if vr.output.len() > 200 {
                        format!("{}...", &vr.output[..200])
                    } else {
                        vr.output.clone()
                    }
                )?;
            }
        }
        if self.rollback_triggered {
            writeln!(f, "\n**Rollback was triggered.**")?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Validation Runner (trait for dependency injection)
// ---------------------------------------------------------------------------

/// Handles the execution of command-based validations (tests, lints).
/// Injected to allow testing without actual shell execution.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Execute a shell command. Returns (success, output).
    async fn run_command(&self, command: &str) -> Result<(bool, String)>;
}

/// Handles human-in-the-loop validations.
/// In production, this would send a Matrix message and wait for response.
/// In tests, this is mocked to auto-approve or auto-reject.
#[async_trait::async_trait]
pub trait HumanGate: Send + Sync {
    /// Request human review and return (approved, feedback).
    async fn request_review(&self, criteria: &str, reviewer: &str) -> Result<(bool, String)>;

    /// Request explicit human approval and return (approved, feedback).
    async fn request_approval(&self) -> Result<(bool, String)>;
}

// ---------------------------------------------------------------------------
// Plan Executor
// ---------------------------------------------------------------------------

/// Executes an `ExecutionPlan` phase by phase with validation gates.
pub struct PlanExecutor<'a> {
    plan: &'a ExecutionPlan,
    cmd_runner: &'a dyn CommandRunner,
    human_gate: &'a dyn HumanGate,
    expert_llm: Option<&'a dyn LlmClient>,
}

impl<'a> PlanExecutor<'a> {
    /// Create a new executor for the given plan.
    pub fn new(
        plan: &'a ExecutionPlan,
        cmd_runner: &'a dyn CommandRunner,
        human_gate: &'a dyn HumanGate,
    ) -> Self {
        Self {
            plan,
            cmd_runner,
            human_gate,
            expert_llm: None,
        }
    }

    /// Set the LLM client used for expert agent validations.
    pub fn with_expert_llm(mut self, llm: &'a dyn LlmClient) -> Self {
        self.expert_llm = Some(llm);
        self
    }

    /// Execute the full plan. Stops on the first phase failure (fail-fast).
    pub async fn execute(&self) -> Result<PlanResult> {
        let mut phase_results = Vec::new();
        let mut failed_at_phase = None;

        for (phase_idx, phase) in self.plan.phases.iter().enumerate() {
            let phase_result = self.execute_phase(phase).await?;
            let phase_passed = phase_result.passed;
            phase_results.push(phase_result);

            if !phase_passed {
                failed_at_phase = Some(phase_idx);
                break; // Fail-fast
            }
        }

        let passed = failed_at_phase.is_none();
        let rollback_triggered =
            !passed && !matches!(self.plan.rollback_strategy, RollbackStrategy::None);

        // Trigger rollback if needed
        if rollback_triggered {
            if let Err(e) = self.execute_rollback().await {
                tracing::warn!(error = %e, "rollback failed");
            }
        }

        Ok(PlanResult {
            plan_name: self.plan.name.clone(),
            phase_results,
            passed,
            failed_at_phase,
            rollback_triggered,
        })
    }

    /// Execute a single phase, running validations in progressive order.
    async fn execute_phase(&self, phase: &Phase) -> Result<PhaseResult> {
        let mut sorted_validations: Vec<(usize, &Validation)> =
            phase.validations.iter().enumerate().collect();
        sorted_validations.sort_by_key(|(_, v)| v.cost_order());

        let mut validation_results = Vec::new();
        let mut failed_at = None;

        for (original_idx, validation) in &sorted_validations {
            let start = Instant::now();
            let (passed, output) = self.run_validation(validation).await?;
            let duration_ms = start.elapsed().as_millis() as u64;

            validation_results.push(ValidationResult {
                validation_label: validation.label().to_string(),
                passed,
                output,
                duration_ms,
            });

            if !passed {
                failed_at = Some(*original_idx);
                break; // Fail-fast within phase
            }
        }

        let passed = failed_at.is_none();
        Ok(PhaseResult {
            phase_name: phase.name.clone(),
            validation_results,
            passed,
            failed_at,
        })
    }

    /// Run a single validation and return (passed, output).
    async fn run_validation(&self, validation: &Validation) -> Result<(bool, String)> {
        match validation {
            Validation::TestPass {
                test_pattern,
                description,
            } => {
                let (success, output) = self.cmd_runner.run_command(test_pattern).await?;
                let msg = if success {
                    format!("Test passed: {description}")
                } else {
                    format!("Test failed: {description}\n{output}")
                };
                Ok((success, msg))
            }

            Validation::LintClean { command } => {
                let (success, output) = self.cmd_runner.run_command(command).await?;
                let msg = if success {
                    "Lint clean".to_string()
                } else {
                    format!("Lint errors:\n{output}")
                };
                Ok((success, msg))
            }

            Validation::HumanReview { reviewer, criteria } => {
                self.human_gate.request_review(criteria, reviewer).await
            }

            Validation::HumanApproval => self.human_gate.request_approval().await,

            Validation::ExpertAgent {
                agent_type,
                criteria,
                system_prompt,
            } => {
                let llm = match self.expert_llm {
                    Some(llm) => llm,
                    None => {
                        return Ok((
                            false,
                            format!(
                                "Expert agent '{agent_type}' requested but no LLM client configured"
                            ),
                        ))
                    }
                };

                let system = system_prompt.clone().unwrap_or_else(|| {
                    format!(
                        "You are an expert {agent_type}. Evaluate the following against these criteria:\n\
                         {criteria}\n\n\
                         Respond with JSON: {{\"passed\": true/false, \"feedback\": \"...\"}}"
                    )
                });

                let messages = vec![
                    ChatMessage {
                        role: "system".to_string(),
                        content: system,
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: format!("Evaluate against criteria: {criteria}"),
                    },
                ];

                let response = llm.chat(&messages, true).await?;

                // Try to parse the response as JSON
                match serde_json::from_str::<ExpertResponse>(&response) {
                    Ok(parsed) => Ok((parsed.passed, parsed.feedback)),
                    Err(_) => {
                        // If not JSON, treat as feedback and fail (conservative)
                        Ok((
                            false,
                            format!("Expert response (unparseable as JSON): {response}"),
                        ))
                    }
                }
            }
        }
    }

    /// Execute the rollback strategy.
    async fn execute_rollback(&self) -> Result<()> {
        match &self.plan.rollback_strategy {
            RollbackStrategy::None => Ok(()),
            RollbackStrategy::GitReset { target_ref } => {
                let cmd = format!("git checkout {target_ref}");
                let (success, output) = self.cmd_runner.run_command(&cmd).await?;
                if !success {
                    return Err(anyhow::anyhow!("git rollback failed: {output}"));
                }
                Ok(())
            }
            RollbackStrategy::CustomCommand { command } => {
                let (success, output) = self.cmd_runner.run_command(command).await?;
                if !success {
                    return Err(anyhow::anyhow!("custom rollback failed: {output}"));
                }
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Expert response parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ExpertResponse {
    passed: bool,
    feedback: String,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- Mock CommandRunner --

    struct MockCommandRunner {
        results: Vec<(bool, String)>,
        call_count: AtomicUsize,
    }

    impl MockCommandRunner {
        fn new(results: Vec<(bool, String)>) -> Self {
            Self {
                results,
                call_count: AtomicUsize::new(0),
            }
        }

        fn all_pass(count: usize) -> Self {
            Self::new((0..count).map(|_| (true, "ok".to_string())).collect())
        }
    }

    #[async_trait::async_trait]
    impl CommandRunner for MockCommandRunner {
        async fn run_command(&self, _command: &str) -> Result<(bool, String)> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(self
                .results
                .get(idx)
                .cloned()
                .unwrap_or((false, "no more mock results".to_string())))
        }
    }

    // -- Mock HumanGate --

    struct AutoApproveGate;

    #[async_trait::async_trait]
    impl HumanGate for AutoApproveGate {
        async fn request_review(&self, _criteria: &str, _reviewer: &str) -> Result<(bool, String)> {
            Ok((true, "Auto-approved in test".to_string()))
        }

        async fn request_approval(&self) -> Result<(bool, String)> {
            Ok((true, "Auto-approved in test".to_string()))
        }
    }

    struct AutoRejectGate;

    #[async_trait::async_trait]
    impl HumanGate for AutoRejectGate {
        async fn request_review(&self, _criteria: &str, _reviewer: &str) -> Result<(bool, String)> {
            Ok((false, "Rejected in test".to_string()))
        }

        async fn request_approval(&self) -> Result<(bool, String)> {
            Ok((false, "Rejected in test".to_string()))
        }
    }

    // -- Mock LLM for expert agents --

    struct MockExpertLlm {
        responses: Vec<String>,
        call_count: AtomicUsize,
    }

    impl MockExpertLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockExpertLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("mock expert ran out of responses"))
        }
    }

    // -- Helper builders --

    fn simple_plan() -> ExecutionPlan {
        ExecutionPlan::simple("Test Plan", "cargo test")
    }

    fn multi_phase_plan() -> ExecutionPlan {
        ExecutionPlan {
            name: "Multi-Phase Plan".to_string(),
            phases: vec![
                Phase {
                    name: "design".to_string(),
                    description: "Design phase".to_string(),
                    validations: vec![Validation::HumanReview {
                        reviewer: "architect".to_string(),
                        criteria: "Architecture approved".to_string(),
                    }],
                },
                Phase {
                    name: "implement".to_string(),
                    description: "Implementation phase".to_string(),
                    validations: vec![
                        Validation::LintClean {
                            command: "cargo clippy".to_string(),
                        },
                        Validation::TestPass {
                            test_pattern: "cargo test".to_string(),
                            description: "All tests pass".to_string(),
                        },
                    ],
                },
                Phase {
                    name: "review".to_string(),
                    description: "Final review".to_string(),
                    validations: vec![Validation::HumanApproval],
                },
            ],
            rollback_strategy: RollbackStrategy::None,
        }
    }

    // -- Validation ordering --

    #[test]
    fn validation_cost_order_progressive() {
        assert!(
            Validation::LintClean {
                command: String::new()
            }
            .cost_order()
                < Validation::TestPass {
                    test_pattern: String::new(),
                    description: String::new()
                }
                .cost_order()
        );

        assert!(
            Validation::TestPass {
                test_pattern: String::new(),
                description: String::new()
            }
            .cost_order()
                < Validation::ExpertAgent {
                    agent_type: String::new(),
                    criteria: String::new(),
                    system_prompt: None,
                }
                .cost_order()
        );

        assert!(
            Validation::ExpertAgent {
                agent_type: String::new(),
                criteria: String::new(),
                system_prompt: None,
            }
            .cost_order()
                < Validation::HumanReview {
                    reviewer: String::new(),
                    criteria: String::new(),
                }
                .cost_order()
        );

        assert!(
            Validation::HumanReview {
                reviewer: String::new(),
                criteria: String::new(),
            }
            .cost_order()
                < Validation::HumanApproval.cost_order()
        );
    }

    #[test]
    fn validation_labels() {
        assert_eq!(
            Validation::TestPass {
                test_pattern: String::new(),
                description: String::new()
            }
            .label(),
            "test_pass"
        );
        assert_eq!(
            Validation::LintClean {
                command: String::new()
            }
            .label(),
            "lint_clean"
        );
        assert_eq!(
            Validation::HumanReview {
                reviewer: String::new(),
                criteria: String::new()
            }
            .label(),
            "human_review"
        );
        assert_eq!(
            Validation::ExpertAgent {
                agent_type: String::new(),
                criteria: String::new(),
                system_prompt: None,
            }
            .label(),
            "expert_agent"
        );
        assert_eq!(Validation::HumanApproval.label(), "human_approval");
    }

    // -- ExecutionPlan --

    #[test]
    fn simple_plan_has_correct_structure() {
        let plan = ExecutionPlan::simple("My Plan", "cargo test -p my-crate");
        assert_eq!(plan.name, "My Plan");
        assert_eq!(plan.phases.len(), 1);
        assert_eq!(plan.phases[0].validations.len(), 2);
        assert_eq!(plan.total_validations(), 2);
    }

    #[test]
    fn multi_phase_plan_total_validations() {
        let plan = multi_phase_plan();
        // design=1, implement=2, review=1
        assert_eq!(plan.total_validations(), 4);
    }

    // -- Plan Execution --

    #[tokio::test]
    async fn execute_simple_plan_all_pass() {
        let plan = simple_plan();
        // 2 validations: lint + test
        let runner = MockCommandRunner::all_pass(2);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(result.passed);
        assert!(result.failed_at_phase.is_none());
        assert!(!result.rollback_triggered);
        assert_eq!(result.phase_results.len(), 1);
        assert_eq!(result.phase_results[0].validation_results.len(), 2);
        assert!(result.phase_results[0].validation_results[0].passed);
        assert!(result.phase_results[0].validation_results[1].passed);
    }

    #[tokio::test]
    async fn execute_simple_plan_lint_fails_fast() {
        let plan = simple_plan();
        // Lint fails, test not reached
        let runner = MockCommandRunner::new(vec![(false, "clippy warning found".to_string())]);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.failed_at_phase, Some(0));
        // Only 1 validation ran (lint), test was skipped due to fail-fast
        assert_eq!(result.phase_results[0].validation_results.len(), 1);
        assert!(!result.phase_results[0].validation_results[0].passed);
    }

    #[tokio::test]
    async fn execute_multi_phase_stops_on_failure() {
        let plan = multi_phase_plan();
        // design: human_review passes, implement: lint passes but test fails
        let runner = MockCommandRunner::new(vec![
            (true, "lint ok".to_string()),       // lint
            (false, "test failure".to_string()), // test
        ]);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        // Phase 0 (design) passed, phase 1 (implement) failed
        assert_eq!(result.failed_at_phase, Some(1));
        // Only 2 phases ran (review phase was skipped)
        assert_eq!(result.phase_results.len(), 2);
        assert!(result.phase_results[0].passed);
        assert!(!result.phase_results[1].passed);
    }

    #[tokio::test]
    async fn execute_multi_phase_all_pass() {
        let plan = multi_phase_plan();
        // implement: lint + test both pass
        let runner = MockCommandRunner::all_pass(2);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(result.passed);
        assert_eq!(result.phase_results.len(), 3);
    }

    #[tokio::test]
    async fn execute_human_review_rejection_fails_phase() {
        let plan = ExecutionPlan {
            name: "Review Plan".to_string(),
            phases: vec![Phase {
                name: "review".to_string(),
                description: "Human review".to_string(),
                validations: vec![Validation::HumanReview {
                    reviewer: "lead".to_string(),
                    criteria: "Code quality".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoRejectGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert_eq!(result.failed_at_phase, Some(0));
    }

    #[tokio::test]
    async fn execute_human_approval_rejection_fails_phase() {
        let plan = ExecutionPlan {
            name: "Approval Plan".to_string(),
            phases: vec![Phase {
                name: "deploy".to_string(),
                description: "Deploy approval".to_string(),
                validations: vec![Validation::HumanApproval],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoRejectGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
    }

    // -- Expert Agent --

    #[tokio::test]
    async fn execute_expert_agent_passes() {
        let plan = ExecutionPlan {
            name: "Expert Plan".to_string(),
            phases: vec![Phase {
                name: "security".to_string(),
                description: "Security review".to_string(),
                validations: vec![Validation::ExpertAgent {
                    agent_type: "security-reviewer".to_string(),
                    criteria: "No OWASP top 10 vulnerabilities".to_string(),
                    system_prompt: None,
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoApproveGate;
        let expert = MockExpertLlm::new(vec![
            r#"{"passed": true, "feedback": "No vulnerabilities found."}"#.to_string(),
        ]);

        let executor = PlanExecutor::new(&plan, &runner, &gate).with_expert_llm(&expert);
        let result = executor.execute().await.unwrap();

        assert!(result.passed);
        assert!(result.phase_results[0].validation_results[0]
            .output
            .contains("No vulnerabilities found"));
    }

    #[tokio::test]
    async fn execute_expert_agent_fails() {
        let plan = ExecutionPlan {
            name: "Expert Fail Plan".to_string(),
            phases: vec![Phase {
                name: "security".to_string(),
                description: "Security review".to_string(),
                validations: vec![Validation::ExpertAgent {
                    agent_type: "security-reviewer".to_string(),
                    criteria: "No SQL injection".to_string(),
                    system_prompt: None,
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoApproveGate;
        let expert = MockExpertLlm::new(vec![
            r#"{"passed": false, "feedback": "SQL injection found in query builder."}"#.to_string(),
        ]);

        let executor = PlanExecutor::new(&plan, &runner, &gate).with_expert_llm(&expert);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert!(result.phase_results[0].validation_results[0]
            .output
            .contains("SQL injection found"));
    }

    #[tokio::test]
    async fn execute_expert_agent_without_llm_fails() {
        let plan = ExecutionPlan {
            name: "No LLM Plan".to_string(),
            phases: vec![Phase {
                name: "review".to_string(),
                description: "Expert review".to_string(),
                validations: vec![Validation::ExpertAgent {
                    agent_type: "reviewer".to_string(),
                    criteria: "Quality check".to_string(),
                    system_prompt: None,
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoApproveGate;

        // No expert_llm set
        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert!(result.phase_results[0].validation_results[0]
            .output
            .contains("no LLM client configured"));
    }

    #[tokio::test]
    async fn execute_expert_agent_non_json_response_fails_conservatively() {
        let plan = ExecutionPlan {
            name: "Non-JSON Expert".to_string(),
            phases: vec![Phase {
                name: "review".to_string(),
                description: "Review".to_string(),
                validations: vec![Validation::ExpertAgent {
                    agent_type: "reviewer".to_string(),
                    criteria: "Check quality".to_string(),
                    system_prompt: None,
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![]);
        let gate = AutoApproveGate;
        let expert = MockExpertLlm::new(vec![
            "I think everything looks fine, but I can't format as JSON.".to_string(),
        ]);

        let executor = PlanExecutor::new(&plan, &runner, &gate).with_expert_llm(&expert);
        let result = executor.execute().await.unwrap();

        // Conservative: treat unparseable as failure
        assert!(!result.passed);
    }

    // -- Rollback --

    #[tokio::test]
    async fn rollback_triggered_on_failure_with_git_reset() {
        let plan = ExecutionPlan {
            name: "Rollback Plan".to_string(),
            phases: vec![Phase {
                name: "implement".to_string(),
                description: "Implement".to_string(),
                validations: vec![Validation::TestPass {
                    test_pattern: "cargo test".to_string(),
                    description: "tests".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::GitReset {
                target_ref: "HEAD~1".to_string(),
            },
        };
        // Test fails, then rollback command succeeds
        let runner = MockCommandRunner::new(vec![
            (false, "test failed".to_string()),
            (true, "rolled back".to_string()),
        ]);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert!(result.rollback_triggered);
    }

    #[tokio::test]
    async fn no_rollback_when_strategy_is_none() {
        let plan = ExecutionPlan {
            name: "No Rollback".to_string(),
            phases: vec![Phase {
                name: "implement".to_string(),
                description: "Implement".to_string(),
                validations: vec![Validation::TestPass {
                    test_pattern: "cargo test".to_string(),
                    description: "tests".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::None,
        };
        let runner = MockCommandRunner::new(vec![(false, "test failed".to_string())]);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert!(!result.rollback_triggered);
    }

    #[tokio::test]
    async fn rollback_with_custom_command() {
        let plan = ExecutionPlan {
            name: "Custom Rollback".to_string(),
            phases: vec![Phase {
                name: "implement".to_string(),
                description: "Implement".to_string(),
                validations: vec![Validation::TestPass {
                    test_pattern: "cargo test".to_string(),
                    description: "tests".to_string(),
                }],
            }],
            rollback_strategy: RollbackStrategy::CustomCommand {
                command: "cleanup.sh".to_string(),
            },
        };
        let runner = MockCommandRunner::new(vec![
            (false, "test failed".to_string()),
            (true, "cleaned up".to_string()),
        ]);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(!result.passed);
        assert!(result.rollback_triggered);
    }

    // -- Progressive validation order --

    #[tokio::test]
    async fn validations_run_in_progressive_order() {
        // Define a phase with validations in reverse cost order to verify sorting
        let plan = ExecutionPlan {
            name: "Order Test".to_string(),
            phases: vec![Phase {
                name: "check".to_string(),
                description: "Check everything".to_string(),
                validations: vec![
                    // Put test_pass first, but lint should run first due to cost_order
                    Validation::TestPass {
                        test_pattern: "cargo test".to_string(),
                        description: "tests".to_string(),
                    },
                    Validation::LintClean {
                        command: "cargo clippy".to_string(),
                    },
                ],
            }],
            rollback_strategy: RollbackStrategy::None,
        };

        // Both pass
        let runner = MockCommandRunner::all_pass(2);
        let gate = AutoApproveGate;

        let executor = PlanExecutor::new(&plan, &runner, &gate);
        let result = executor.execute().await.unwrap();

        assert!(result.passed);
        // Verify lint ran first (lower cost_order)
        assert_eq!(
            result.phase_results[0].validation_results[0].validation_label,
            "lint_clean"
        );
        assert_eq!(
            result.phase_results[0].validation_results[1].validation_label,
            "test_pass"
        );
    }

    // -- Display --

    #[test]
    fn plan_result_display_shows_structure() {
        let result = PlanResult {
            plan_name: "Test Plan".to_string(),
            phase_results: vec![PhaseResult {
                phase_name: "implement".to_string(),
                validation_results: vec![
                    ValidationResult {
                        validation_label: "lint_clean".to_string(),
                        passed: true,
                        output: "Lint clean".to_string(),
                        duration_ms: 150,
                    },
                    ValidationResult {
                        validation_label: "test_pass".to_string(),
                        passed: false,
                        output: "Test failed: assertion error".to_string(),
                        duration_ms: 5000,
                    },
                ],
                passed: false,
                failed_at: Some(1),
            }],
            passed: false,
            failed_at_phase: Some(0),
            rollback_triggered: true,
        };

        let display = format!("{result}");
        assert!(display.contains("Test Plan [FAILED]"));
        assert!(display.contains("implement [FAIL]"));
        assert!(display.contains("[PASS] lint_clean"));
        assert!(display.contains("[FAIL] test_pass"));
        assert!(display.contains("Rollback was triggered"));
    }

    #[test]
    fn plan_result_display_passing() {
        let result = PlanResult {
            plan_name: "Good Plan".to_string(),
            phase_results: vec![PhaseResult {
                phase_name: "build".to_string(),
                validation_results: vec![ValidationResult {
                    validation_label: "test_pass".to_string(),
                    passed: true,
                    output: "All tests passed".to_string(),
                    duration_ms: 2000,
                }],
                passed: true,
                failed_at: None,
            }],
            passed: true,
            failed_at_phase: None,
            rollback_triggered: false,
        };

        let display = format!("{result}");
        assert!(display.contains("Good Plan [PASSED]"));
        assert!(display.contains("build [OK]"));
        assert!(!display.contains("Rollback"));
    }

    // -- Default rollback strategy --

    #[test]
    fn default_rollback_is_none() {
        assert!(matches!(
            RollbackStrategy::default(),
            RollbackStrategy::None
        ));
    }

    // -- Serialization round-trip --

    #[test]
    fn execution_plan_serialization_roundtrip() {
        let plan = multi_phase_plan();
        let json = serde_json::to_string_pretty(&plan).unwrap();
        let deserialized: ExecutionPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, plan.name);
        assert_eq!(deserialized.phases.len(), plan.phases.len());
    }

    #[test]
    fn validation_serialization_roundtrip() {
        let validations = vec![
            Validation::TestPass {
                test_pattern: "cargo test".to_string(),
                description: "Tests pass".to_string(),
            },
            Validation::LintClean {
                command: "cargo clippy".to_string(),
            },
            Validation::HumanReview {
                reviewer: "lead".to_string(),
                criteria: "Architecture".to_string(),
            },
            Validation::ExpertAgent {
                agent_type: "security".to_string(),
                criteria: "OWASP".to_string(),
                system_prompt: Some("Custom prompt".to_string()),
            },
            Validation::HumanApproval,
        ];

        let json = serde_json::to_string(&validations).unwrap();
        let deserialized: Vec<Validation> = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.len(), 5);
    }
}
