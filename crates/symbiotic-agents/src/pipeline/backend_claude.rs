//! ClaudeCode Backend — executes pipeline phases by spawning `claude` CLI processes.
//!
//! Writes phase description + context to a temp instruction, spawns the CLI,
//! captures output with timeout, and parses the result.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::execution_plan::Phase;

use super::backend::{GoalExecutionContext, PhaseExecutor};

/// Configuration for Claude Code CLI execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCodeConfig {
    /// Working directory for the CLI session.
    pub working_dir: PathBuf,
    /// Model to use (default: "claude-sonnet-4-6").
    pub model: String,
    /// Maximum runtime in seconds before kill.
    pub timeout_secs: u64,
    /// Optional system prompt content to prepend to the phase instructions.
    pub system_prompt: Option<String>,
}

impl Default for ClaudeCodeConfig {
    fn default() -> Self {
        Self {
            working_dir: PathBuf::from("."),
            model: "claude-sonnet-4-6".to_string(),
            timeout_secs: 300,
            system_prompt: None,
        }
    }
}

/// Executes pipeline phases by spawning `claude` CLI processes.
pub struct ClaudeCodePhaseExecutor {
    config: ClaudeCodeConfig,
}

impl ClaudeCodePhaseExecutor {
    /// Create a new ClaudeCode phase executor.
    pub fn new(config: ClaudeCodeConfig) -> Self {
        Self { config }
    }

    /// Access the executor config.
    pub fn config(&self) -> &ClaudeCodeConfig {
        &self.config
    }

    /// Build the prompt string from phase description and context.
    pub fn build_prompt(&self, phase: &Phase, context: &GoalExecutionContext) -> String {
        let mut prompt = String::new();

        // Prepend system prompt if configured.
        if let Some(ref system) = self.config.system_prompt {
            prompt.push_str(system);
            prompt.push_str("\n\n---\n\n");
        }

        prompt.push_str(&format!(
            "# Goal: {}\n\n## Phase: {}\n\n{}\n",
            context.goal_id, phase.name, phase.description,
        ));

        // Include validation requirements so the agent knows what to satisfy.
        if !phase.validations.is_empty() {
            prompt.push_str("\n## Validation Criteria\n\n");
            for (i, v) in phase.validations.iter().enumerate() {
                prompt.push_str(&format!("{}. {}\n", i + 1, format_validation(v)));
            }
        }

        prompt
    }

    /// Build the CLI command arguments.
    pub fn build_command_args(&self, prompt: &str) -> Vec<String> {
        vec![
            "--model".to_string(),
            self.config.model.clone(),
            "--print".to_string(),
            "--output-format".to_string(),
            "json".to_string(),
            "--dangerously-skip-permissions".to_string(),
            "-p".to_string(),
            prompt.to_string(),
        ]
    }

    /// Spawn the claude CLI and capture output with timeout.
    ///
    /// Returns `(stdout, stderr, exit_code)`.
    async fn run_cli(&self, prompt: &str) -> Result<CliOutput> {
        let args = self.build_command_args(prompt);

        let mut child = Command::new("claude")
            .args(&args)
            .current_dir(&self.config.working_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn claude CLI process")?;

        let timeout = tokio::time::Duration::from_secs(self.config.timeout_secs);

        match tokio::time::timeout(timeout, async {
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();

            if let Some(ref mut stdout) = child.stdout {
                stdout
                    .read_to_end(&mut stdout_buf)
                    .await
                    .context("failed to read stdout")?;
            }
            if let Some(ref mut stderr) = child.stderr {
                stderr
                    .read_to_end(&mut stderr_buf)
                    .await
                    .context("failed to read stderr")?;
            }

            let status = child.wait().await.context("failed to wait for process")?;

            Ok::<_, anyhow::Error>(CliOutput {
                stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
                stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
            })
        })
        .await
        {
            Ok(result) => result,
            Err(_) => {
                // Timeout: kill the process.
                let _ = child.kill().await;
                Ok(CliOutput {
                    stdout: String::new(),
                    stderr: format!(
                        "Process timed out after {} seconds",
                        self.config.timeout_secs
                    ),
                    exit_code: -1,
                    timed_out: true,
                })
            }
        }
    }

    /// Parse the CLI output into a result text and quality score.
    pub fn parse_output(&self, output: &CliOutput) -> Result<(String, f64)> {
        if output.timed_out {
            return Err(anyhow::anyhow!(
                "Claude CLI timed out after {} seconds",
                self.config.timeout_secs
            ));
        }

        if output.exit_code != 0 {
            return Err(anyhow::anyhow!(
                "Claude CLI exited with code {}: {}",
                output.exit_code,
                output.stderr.trim()
            ));
        }

        // Try to parse as JSON (--output-format json produces structured output).
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&output.stdout) {
            // The claude CLI JSON output has a "result" field with the text.
            if let Some(result) = json.get("result").and_then(|v| v.as_str()) {
                return Ok((result.to_string(), estimate_quality(&output.stdout)));
            }
            // Fallback: use the full JSON stringified.
            return Ok((output.stdout.clone(), estimate_quality(&output.stdout)));
        }

        // Not JSON — use raw stdout.
        if output.stdout.trim().is_empty() {
            return Err(anyhow::anyhow!("Claude CLI produced empty output"));
        }

        Ok((output.stdout.clone(), estimate_quality(&output.stdout)))
    }
}

/// Raw output from the CLI process.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
}

#[async_trait]
impl PhaseExecutor for ClaudeCodePhaseExecutor {
    async fn execute_phase(
        &self,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<(String, f64)> {
        let prompt = self.build_prompt(phase, context);
        let output = self.run_cli(&prompt).await?;
        self.parse_output(&output)
    }
}

/// Estimate a quality score from the output length.
/// Longer, more substantive outputs generally indicate higher quality.
fn estimate_quality(output: &str) -> f64 {
    let len = output.len();
    match len {
        0 => 0.0,
        1..=100 => 0.5,
        101..=500 => 0.7,
        501..=2000 => 0.8,
        _ => 0.9,
    }
}

/// Format a validation for inclusion in the prompt.
fn format_validation(v: &crate::execution_plan::Validation) -> String {
    use crate::execution_plan::Validation;
    match v {
        Validation::TestPass {
            test_pattern,
            description,
        } => format!("Test: `{test_pattern}` — {description}"),
        Validation::LintClean { command } => format!("Lint clean: `{command}`"),
        Validation::HumanReview {
            reviewer, criteria, ..
        } => format!("Human review by {reviewer}: {criteria}"),
        Validation::ExpertAgent {
            agent_type,
            criteria,
            ..
        } => format!("Expert agent ({agent_type}): {criteria}"),
        Validation::HumanApproval => "Human approval required before proceeding".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution_plan::{ExecutionPlan, Phase, RollbackStrategy, Validation};
    use crate::pipeline::backend::ExecutionBackend;

    fn make_config() -> ClaudeCodeConfig {
        ClaudeCodeConfig {
            working_dir: PathBuf::from("/tmp/test-work"),
            model: "claude-sonnet-4-6".to_string(),
            timeout_secs: 60,
            system_prompt: None,
        }
    }

    fn make_phase() -> Phase {
        Phase {
            name: "implement".to_string(),
            description: "Implement the feature".to_string(),
            validations: vec![Validation::LintClean {
                command: "cargo clippy".to_string(),
            }],
        }
    }

    fn make_context() -> GoalExecutionContext {
        GoalExecutionContext {
            goal_id: "goal-test".to_string(),
            plan: ExecutionPlan {
                name: "test-plan".to_string(),
                phases: vec![],
                rollback_strategy: RollbackStrategy::None,
            },
            backend: ExecutionBackend::Native,
            spawned_agents: vec![],
            phase_results: vec![],
            started_at: 1000,
            completed_at: None,
        }
    }

    // -----------------------------------------------------------------------
    // Config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_config_default() {
        let config = ClaudeCodeConfig::default();
        assert_eq!(config.working_dir, PathBuf::from("."));
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.timeout_secs, 300);
        assert!(config.system_prompt.is_none());
    }

    #[test]
    fn test_config_serde_roundtrip() {
        let config = ClaudeCodeConfig {
            working_dir: PathBuf::from("/home/user/project"),
            model: "claude-opus-4-6".to_string(),
            timeout_secs: 600,
            system_prompt: Some("You are a coding agent.".to_string()),
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ClaudeCodeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.working_dir, config.working_dir);
        assert_eq!(deserialized.model, config.model);
        assert_eq!(deserialized.timeout_secs, config.timeout_secs);
        assert_eq!(deserialized.system_prompt, config.system_prompt);
    }

    // -----------------------------------------------------------------------
    // Prompt building tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_prompt_basic() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let phase = make_phase();
        let ctx = make_context();

        let prompt = executor.build_prompt(&phase, &ctx);
        assert!(prompt.contains("# Goal: goal-test"));
        assert!(prompt.contains("## Phase: implement"));
        assert!(prompt.contains("Implement the feature"));
        assert!(prompt.contains("Validation Criteria"));
        assert!(prompt.contains("cargo clippy"));
    }

    #[test]
    fn test_build_prompt_with_system_prompt() {
        let config = ClaudeCodeConfig {
            system_prompt: Some("You are an expert Rust developer.".to_string()),
            ..make_config()
        };
        let executor = ClaudeCodePhaseExecutor::new(config);
        let phase = make_phase();
        let ctx = make_context();

        let prompt = executor.build_prompt(&phase, &ctx);
        assert!(prompt.starts_with("You are an expert Rust developer."));
        assert!(prompt.contains("---"));
        assert!(prompt.contains("# Goal: goal-test"));
    }

    #[test]
    fn test_build_prompt_no_validations() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let phase = Phase {
            name: "explore".to_string(),
            description: "Explore the codebase".to_string(),
            validations: vec![],
        };
        let ctx = make_context();

        let prompt = executor.build_prompt(&phase, &ctx);
        assert!(!prompt.contains("Validation Criteria"));
    }

    #[test]
    fn test_build_prompt_multiple_validations() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let phase = Phase {
            name: "verify".to_string(),
            description: "Verify the implementation".to_string(),
            validations: vec![
                Validation::LintClean {
                    command: "cargo clippy".to_string(),
                },
                Validation::TestPass {
                    test_pattern: "cargo test".to_string(),
                    description: "All tests pass".to_string(),
                },
                Validation::HumanReview {
                    reviewer: "lead".to_string(),
                    criteria: "Code quality".to_string(),
                },
                Validation::ExpertAgent {
                    agent_type: "security-reviewer".to_string(),
                    criteria: "No vulnerabilities".to_string(),
                    system_prompt: None,
                },
            ],
        };
        let ctx = make_context();

        let prompt = executor.build_prompt(&phase, &ctx);
        assert!(prompt.contains("1. Lint clean:"));
        assert!(prompt.contains("2. Test:"));
        assert!(prompt.contains("3. Human review by lead:"));
        assert!(prompt.contains("4. Expert agent (security-reviewer):"));
    }

    // -----------------------------------------------------------------------
    // Command args tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_command_args() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let args = executor.build_command_args("test prompt");

        assert_eq!(args[0], "--model");
        assert_eq!(args[1], "claude-sonnet-4-6");
        assert_eq!(args[2], "--print");
        assert_eq!(args[3], "--output-format");
        assert_eq!(args[4], "json");
        assert_eq!(args[5], "--dangerously-skip-permissions");
        assert_eq!(args[6], "-p");
        assert_eq!(args[7], "test prompt");
    }

    #[test]
    fn test_build_command_args_custom_model() {
        let config = ClaudeCodeConfig {
            model: "claude-opus-4-6".to_string(),
            ..make_config()
        };
        let executor = ClaudeCodePhaseExecutor::new(config);
        let args = executor.build_command_args("prompt");
        assert_eq!(args[1], "claude-opus-4-6");
    }

    // -----------------------------------------------------------------------
    // Output parsing tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_output_success_json() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: r#"{"result": "Phase completed successfully with all tests passing"}"#
                .to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };

        let (text, quality) = executor.parse_output(&output).unwrap();
        assert_eq!(text, "Phase completed successfully with all tests passing");
        assert!(quality > 0.0);
    }

    #[test]
    fn test_parse_output_success_plain_text() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: "I implemented the feature. Here is what I did:\n\n1. Added function\n2. Wrote tests".to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };

        let (text, quality) = executor.parse_output(&output).unwrap();
        assert!(text.contains("implemented the feature"));
        assert!(quality > 0.0);
    }

    #[test]
    fn test_parse_output_timeout() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: String::new(),
            stderr: "Process timed out after 60 seconds".to_string(),
            exit_code: -1,
            timed_out: true,
        };

        let err = executor.parse_output(&output).unwrap_err();
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn test_parse_output_nonzero_exit() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: String::new(),
            stderr: "Error: model not found".to_string(),
            exit_code: 1,
            timed_out: false,
        };

        let err = executor.parse_output(&output).unwrap_err();
        assert!(err.to_string().contains("exited with code 1"));
        assert!(err.to_string().contains("model not found"));
    }

    #[test]
    fn test_parse_output_empty() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: "   ".to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };

        let err = executor.parse_output(&output).unwrap_err();
        assert!(err.to_string().contains("empty output"));
    }

    #[test]
    fn test_parse_output_json_without_result_field() {
        let executor = ClaudeCodePhaseExecutor::new(make_config());
        let output = CliOutput {
            stdout: r#"{"status": "ok", "data": "some output"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
        };

        let (text, quality) = executor.parse_output(&output).unwrap();
        // Falls back to full stdout since there's no "result" field.
        assert!(text.contains("status"));
        assert!(quality > 0.0);
    }

    // -----------------------------------------------------------------------
    // Quality estimation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_estimate_quality_empty() {
        assert!((estimate_quality("") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_quality_short() {
        assert!((estimate_quality("ok") - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_quality_medium() {
        let medium = "x".repeat(200);
        assert!((estimate_quality(&medium) - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_quality_long() {
        let long = "x".repeat(1000);
        assert!((estimate_quality(&long) - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_estimate_quality_very_long() {
        let very_long = "x".repeat(5000);
        assert!((estimate_quality(&very_long) - 0.9).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Format validation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_format_validation_lint() {
        let v = Validation::LintClean {
            command: "cargo clippy".to_string(),
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Lint clean"));
        assert!(formatted.contains("cargo clippy"));
    }

    #[test]
    fn test_format_validation_test() {
        let v = Validation::TestPass {
            test_pattern: "cargo test".to_string(),
            description: "All tests must pass".to_string(),
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Test:"));
        assert!(formatted.contains("cargo test"));
        assert!(formatted.contains("All tests must pass"));
    }

    #[test]
    fn test_format_validation_human() {
        let v = Validation::HumanReview {
            reviewer: "tech-lead".to_string(),
            criteria: "Architectural review".to_string(),
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Human review by tech-lead"));
        assert!(formatted.contains("Architectural review"));
    }

    #[test]
    fn test_format_validation_expert() {
        let v = Validation::ExpertAgent {
            agent_type: "security-reviewer".to_string(),
            criteria: "No SQL injection".to_string(),
            system_prompt: None,
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Expert agent (security-reviewer)"));
        assert!(formatted.contains("No SQL injection"));
    }

    // -----------------------------------------------------------------------
    // Constructor tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_executor_construction() {
        let config = make_config();
        let executor = ClaudeCodePhaseExecutor::new(config.clone());
        assert_eq!(executor.config().working_dir, config.working_dir);
        assert_eq!(executor.config().model, config.model);
        assert_eq!(executor.config().timeout_secs, config.timeout_secs);
    }
}
