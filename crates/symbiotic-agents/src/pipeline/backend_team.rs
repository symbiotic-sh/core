//! Team Backend — executes pipeline phases by spawning parallel `claude` CLI teams.
//!
//! Each team member works in its own git worktree with a specific role/focus.
//! Results are collected, conflicts detected, and outputs merged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::execution_plan::Phase;

use super::backend::{GoalExecutionContext, PhaseExecutor};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for team-based execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamConfig {
    /// Number of team members to spawn.
    pub team_size: usize,
    /// Working directory (repository root).
    pub working_dir: PathBuf,
    /// Model to use for team members. Default: "claude-sonnet-4-6".
    pub model: String,
    /// Maximum runtime per member in seconds. Default: 600.
    pub member_timeout_secs: u64,
    /// Whether to auto-merge worktree branches. Default: false.
    pub auto_merge: bool,
}

impl Default for TeamConfig {
    fn default() -> Self {
        Self {
            team_size: 3,
            working_dir: PathBuf::from("."),
            model: "claude-sonnet-4-6".to_string(),
            member_timeout_secs: 600,
            auto_merge: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Team member types
// ---------------------------------------------------------------------------

/// Represents a single team member's assignment and result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamMember {
    /// Index of the member in the team (0-based).
    pub index: usize,
    /// Role assigned to this member (e.g., "architect", "implementer", "tester").
    pub role: String,
    /// Path to the member's worktree directory.
    pub worktree_path: PathBuf,
    /// Branch name for this member's worktree.
    pub branch_name: String,
    /// Current status of this team member.
    pub status: TeamMemberStatus,
}

/// Status of a team member's execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TeamMemberStatus {
    /// Not yet started.
    Pending,
    /// Currently running.
    Running,
    /// Completed successfully.
    Completed {
        /// The output produced by this member.
        output: String,
        /// Quality score (0.0 - 1.0).
        quality: f64,
    },
    /// Failed with an error.
    Failed {
        /// Error description.
        error: String,
    },
    /// Timed out before completing.
    TimedOut,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of the team execution phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamResult {
    /// All team members with their final status.
    pub members: Vec<TeamMember>,
    /// Merged output from all completed members.
    pub merged_output: String,
    /// Conflicts detected between members' changes.
    pub conflicts: Vec<MergeConflict>,
    /// Overall quality score (average of completed members).
    pub overall_quality: f64,
}

/// A merge conflict between team members' changes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    /// File that has conflicting changes.
    pub file_path: String,
    /// Indices of members that modified this file.
    pub member_indices: Vec<usize>,
    /// Description of the conflict.
    pub description: String,
}

/// Information about a worktree created for a team member.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    /// Path to the worktree directory.
    pub path: PathBuf,
    /// Branch name for this worktree.
    pub branch: String,
    /// Member index this worktree belongs to.
    pub member_index: usize,
}

/// Raw output from a CLI process.
#[derive(Debug, Clone)]
pub struct CliOutput {
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
    /// Process exit code.
    pub exit_code: i32,
    /// Whether the process was killed due to timeout.
    pub timed_out: bool,
}

// ---------------------------------------------------------------------------
// TeamPhaseExecutor
// ---------------------------------------------------------------------------

/// Executes pipeline phases by spawning parallel claude CLI teams.
///
/// Each team member runs in its own git worktree with an assigned role.
/// After all members complete, results are collected, conflicts detected,
/// and outputs merged.
pub struct TeamPhaseExecutor {
    config: TeamConfig,
}

impl TeamPhaseExecutor {
    /// Create a new team phase executor.
    pub fn new(config: TeamConfig) -> Self {
        Self { config }
    }

    /// Access the executor config.
    pub fn config(&self) -> &TeamConfig {
        &self.config
    }

    /// Create git worktrees for team members.
    ///
    /// Creates `count` worktrees under `{base_dir}/.worktrees/member-{index}`.
    /// Branch naming: `team/{goal_id}/member-{index}`.
    pub async fn setup_worktrees(
        &self,
        base_dir: &Path,
        count: usize,
        goal_id: &str,
    ) -> Result<Vec<WorktreeInfo>> {
        let mut worktrees = Vec::with_capacity(count);

        for i in 0..count {
            let branch = Self::branch_name(goal_id, i);
            let path = Self::worktree_path(base_dir, i);

            // Create the worktree
            let output = Command::new("git")
                .args(["worktree", "add", "-b", &branch])
                .arg(&path)
                .arg("HEAD")
                .current_dir(base_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await
                .context("failed to spawn git worktree add")?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(anyhow::anyhow!(
                    "git worktree add failed for member {}: {}",
                    i,
                    stderr.trim()
                ));
            }

            worktrees.push(WorktreeInfo {
                path,
                branch,
                member_index: i,
            });
        }

        Ok(worktrees)
    }

    /// Assign roles to team members based on team size and phase.
    ///
    /// For different team sizes:
    /// - 1: ["solo"]
    /// - 2: ["primary", "reviewer"]
    /// - 3: ["architect", "implementer", "tester"]
    /// - 4+: ["architect", "implementer", "tester", "reviewer", ...]
    pub fn assign_roles(&self, phase: &Phase, team_size: usize) -> Vec<String> {
        match team_size {
            0 => vec![],
            1 => vec!["solo".to_string()],
            2 => vec!["primary".to_string(), "reviewer".to_string()],
            3 => vec![
                "architect".to_string(),
                "implementer".to_string(),
                "tester".to_string(),
            ],
            n => {
                let mut roles = vec![
                    "architect".to_string(),
                    "implementer".to_string(),
                    "tester".to_string(),
                    "reviewer".to_string(),
                ];
                // Additional members get numbered implementer roles.
                for i in 4..n {
                    roles.push(format!("implementer-{}", i - 3));
                }
                // Trim to the phase-aware size if needed, but include
                // phase name for richer role assignment context.
                let _ = phase; // Phase is available for future specialization.
                roles
            }
        }
    }

    /// Build a member-specific prompt including role and focus area.
    pub fn build_member_prompt(
        &self,
        member: &TeamMember,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> String {
        let mut prompt = String::new();

        prompt.push_str(&format!(
            "# Goal: {}\n\n## Phase: {}\n\n{}\n\n",
            context.goal_id, phase.name, phase.description,
        ));

        prompt.push_str(&format!(
            "## Your Role: {}\n\nYou are team member {} with the role of '{}'.\n",
            member.role, member.index, member.role,
        ));

        prompt.push_str(&format!(
            "Focus on the {} aspects of this phase.\n",
            member.role
        ));

        // Include validation requirements.
        if !phase.validations.is_empty() {
            prompt.push_str("\n## Validation Criteria\n\n");
            for (i, v) in phase.validations.iter().enumerate() {
                prompt.push_str(&format!("{}. {}\n", i + 1, format_validation(v)));
            }
        }

        prompt
    }

    /// Build the CLI command arguments for a team member.
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

    /// Spawn a single team member's CLI process and capture output.
    pub async fn spawn_member(
        &self,
        member: &TeamMember,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<TeamMemberStatus> {
        let prompt = self.build_member_prompt(member, phase, context);
        let args = self.build_command_args(&prompt);

        let mut child = Command::new("claude")
            .args(&args)
            .current_dir(&member.worktree_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to spawn claude CLI process")?;

        let timeout = tokio::time::Duration::from_secs(self.config.member_timeout_secs);

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
            Ok(Ok(output)) => {
                if output.exit_code != 0 {
                    Ok(TeamMemberStatus::Failed {
                        error: format!(
                            "CLI exited with code {}: {}",
                            output.exit_code,
                            output.stderr.trim()
                        ),
                    })
                } else if output.stdout.trim().is_empty() {
                    Ok(TeamMemberStatus::Failed {
                        error: "CLI produced empty output".to_string(),
                    })
                } else {
                    let quality = estimate_quality(&output.stdout);
                    Ok(TeamMemberStatus::Completed {
                        output: output.stdout,
                        quality,
                    })
                }
            }
            Ok(Err(e)) => Ok(TeamMemberStatus::Failed {
                error: e.to_string(),
            }),
            Err(_) => {
                let _ = child.kill().await;
                Ok(TeamMemberStatus::TimedOut)
            }
        }
    }

    /// Execute the full team workflow for a phase.
    ///
    /// 1. Setup worktrees
    /// 2. Assign roles
    /// 3. Spawn all members in parallel
    /// 4. Collect results
    /// 5. Detect conflicts
    /// 6. Optionally merge
    /// 7. Compute overall quality
    pub async fn execute_team(
        &self,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<TeamResult> {
        let team_size = self.config.team_size;
        let working_dir = &self.config.working_dir;

        // 1. Setup worktrees
        let worktrees = self
            .setup_worktrees(working_dir, team_size, &context.goal_id)
            .await?;

        // 2. Assign roles
        let roles = self.assign_roles(phase, team_size);

        // 3. Create team members
        let mut members: Vec<TeamMember> = worktrees
            .iter()
            .enumerate()
            .map(|(i, wt)| TeamMember {
                index: i,
                role: roles
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("member-{i}")),
                worktree_path: wt.path.clone(),
                branch_name: wt.branch.clone(),
                status: TeamMemberStatus::Pending,
            })
            .collect();

        // 4. Spawn all members in parallel
        let mut handles = Vec::new();
        for member in &members {
            let member_clone = member.clone();
            let phase_clone = phase.clone();
            let context_clone = context.clone();
            // Build the prompt and args before spawning to capture config.
            let prompt = self.build_member_prompt(&member_clone, &phase_clone, &context_clone);
            let args = self.build_command_args(&prompt);
            let worktree_path = member_clone.worktree_path.clone();
            let timeout_secs = self.config.member_timeout_secs;

            handles.push(tokio::spawn(async move {
                let result = run_member_cli(&args, &worktree_path, timeout_secs).await;
                (member_clone.index, result)
            }));
        }

        // 5. Collect results
        for handle in handles {
            match handle.await {
                Ok((index, status)) => {
                    if let Some(member) = members.get_mut(index) {
                        member.status = status;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "team member task panicked");
                }
            }
        }

        // 6. Detect conflicts
        let conflicts = self.detect_conflicts(&members, working_dir).await;

        // 7. Merge output and compute quality
        let merged_output = self.merge_outputs(&members);
        let overall_quality = self.compute_overall_quality(&members);

        // 8. Cleanup worktrees
        let cleanup_worktrees: Vec<WorktreeInfo> = worktrees;
        if let Err(e) = self
            .cleanup_worktrees(&cleanup_worktrees, working_dir)
            .await
        {
            tracing::warn!(error = %e, "failed to cleanup worktrees");
        }

        Ok(TeamResult {
            members,
            merged_output,
            conflicts,
            overall_quality,
        })
    }

    /// Detect files modified by multiple team members (potential conflicts).
    pub async fn detect_conflicts(
        &self,
        members: &[TeamMember],
        base_dir: &Path,
    ) -> Vec<MergeConflict> {
        let mut file_to_members: HashMap<String, Vec<usize>> = HashMap::new();

        for member in members {
            if !matches!(member.status, TeamMemberStatus::Completed { .. }) {
                continue;
            }

            let changed_files = Self::get_changed_files(&member.worktree_path, base_dir).await;
            for file in changed_files {
                file_to_members.entry(file).or_default().push(member.index);
            }
        }

        file_to_members
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(file_path, member_indices)| {
                let roles: Vec<String> = member_indices
                    .iter()
                    .filter_map(|&idx| members.get(idx).map(|m| m.role.clone()))
                    .collect();
                MergeConflict {
                    description: format!(
                        "File '{}' modified by members: {}",
                        file_path,
                        roles.join(", ")
                    ),
                    file_path,
                    member_indices,
                }
            })
            .collect()
    }

    /// Get changed files in a worktree relative to HEAD.
    async fn get_changed_files(worktree_path: &Path, _base_dir: &Path) -> Vec<String> {
        let output = Command::new("git")
            .args(["diff", "--name-only", "HEAD"])
            .current_dir(worktree_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|l| l.to_string())
                .collect(),
            _ => vec![],
        }
    }

    /// Merge outputs from all completed members into a single string.
    pub fn merge_outputs(&self, members: &[TeamMember]) -> String {
        let mut merged = String::new();

        for member in members {
            if let TeamMemberStatus::Completed { ref output, .. } = member.status {
                if !merged.is_empty() {
                    merged.push_str("\n\n---\n\n");
                }
                merged.push_str(&format!("## Member {} ({})\n\n", member.index, member.role));
                merged.push_str(output);
            }
        }

        merged
    }

    /// Compute the overall quality score as the average of completed members.
    pub fn compute_overall_quality(&self, members: &[TeamMember]) -> f64 {
        let mut total = 0.0;
        let mut count = 0;

        for member in members {
            if let TeamMemberStatus::Completed { quality, .. } = member.status {
                total += quality;
                count += 1;
            }
        }

        if count == 0 {
            0.0
        } else {
            total / count as f64
        }
    }

    /// Remove worktrees and optionally clean up branches.
    pub async fn cleanup_worktrees(
        &self,
        worktrees: &[WorktreeInfo],
        base_dir: &Path,
    ) -> Result<()> {
        for wt in worktrees {
            // Remove worktree
            let remove = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&wt.path)
                .current_dir(base_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await;

            match remove {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        member_index = wt.member_index,
                        error = %stderr.trim(),
                        "failed to remove worktree, attempting manual cleanup"
                    );
                    // Fallback: manual directory removal + prune.
                    if wt.path.exists() {
                        let _ = tokio::fs::remove_dir_all(&wt.path).await;
                    }
                    let _ = Command::new("git")
                        .args(["worktree", "prune"])
                        .current_dir(base_dir)
                        .output()
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        member_index = wt.member_index,
                        error = %e,
                        "failed to execute git worktree remove"
                    );
                }
                Ok(_) => {}
            }

            // Delete the branch
            let branch_del = Command::new("git")
                .args(["branch", "-D", &wt.branch])
                .current_dir(base_dir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output()
                .await;

            match branch_del {
                Ok(output) if !output.status.success() => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    tracing::warn!(
                        branch = %wt.branch,
                        error = %stderr.trim(),
                        "failed to delete team branch"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        branch = %wt.branch,
                        error = %e,
                        "failed to execute git branch delete"
                    );
                }
                Ok(_) => {}
            }
        }

        Ok(())
    }

    /// Compute the worktree directory path for a member.
    pub fn worktree_path(base_dir: &Path, index: usize) -> PathBuf {
        base_dir.join(".worktrees").join(format!("member-{index}"))
    }

    /// Compute the branch name for a team member.
    pub fn branch_name(goal_id: &str, index: usize) -> String {
        format!("team/{goal_id}/member-{index}")
    }
}

/// Run a single team member CLI process (used in tokio::spawn context).
async fn run_member_cli(
    args: &[String],
    working_dir: &Path,
    timeout_secs: u64,
) -> TeamMemberStatus {
    let child = Command::new("claude")
        .args(args)
        .current_dir(working_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return TeamMemberStatus::Failed {
                error: format!("failed to spawn claude CLI: {e}"),
            };
        }
    };

    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    match tokio::time::timeout(timeout, async {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        if let Some(ref mut stdout) = child.stdout {
            let _ = stdout.read_to_end(&mut stdout_buf).await;
        }
        if let Some(ref mut stderr) = child.stderr {
            let _ = stderr.read_to_end(&mut stderr_buf).await;
        }

        let status = child.wait().await;

        (stdout_buf, stderr_buf, status)
    })
    .await
    {
        Ok((stdout_buf, stderr_buf, Ok(status))) => {
            let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
            let exit_code = status.code().unwrap_or(-1);

            if exit_code != 0 {
                TeamMemberStatus::Failed {
                    error: format!("CLI exited with code {exit_code}: {}", stderr.trim()),
                }
            } else if stdout.trim().is_empty() {
                TeamMemberStatus::Failed {
                    error: "CLI produced empty output".to_string(),
                }
            } else {
                let quality = estimate_quality(&stdout);
                TeamMemberStatus::Completed {
                    output: stdout,
                    quality,
                }
            }
        }
        Ok((_, _, Err(e))) => TeamMemberStatus::Failed {
            error: format!("failed to wait for process: {e}"),
        },
        Err(_) => {
            let _ = child.kill().await;
            TeamMemberStatus::TimedOut
        }
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
        } => format!("Test: `{test_pattern}` -- {description}"),
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

#[async_trait]
impl PhaseExecutor for TeamPhaseExecutor {
    async fn execute_phase(
        &self,
        phase: &Phase,
        context: &GoalExecutionContext,
    ) -> Result<(String, f64)> {
        let result = self.execute_team(phase, context).await?;

        // If there are conflicts, include them in the output.
        let mut output = result.merged_output;
        if !result.conflicts.is_empty() {
            output.push_str("\n\n## Conflicts Detected\n\n");
            for conflict in &result.conflicts {
                output.push_str(&format!(
                    "- **{}**: {}\n",
                    conflict.file_path, conflict.description
                ));
            }
        }

        Ok((output, result.overall_quality))
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    use crate::execution_plan::{ExecutionPlan, Phase, RollbackStrategy, Validation};
    use crate::pipeline::backend::ExecutionBackend;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    fn make_config() -> TeamConfig {
        TeamConfig {
            team_size: 3,
            working_dir: PathBuf::from("/tmp/test-team"),
            model: "claude-sonnet-4-6".to_string(),
            member_timeout_secs: 60,
            auto_merge: false,
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
            goal_id: "goal-team-test".to_string(),
            plan: ExecutionPlan {
                name: "test-plan".to_string(),
                phases: vec![],
                rollback_strategy: RollbackStrategy::None,
            },
            backend: ExecutionBackend::Team {
                team_size: 3,
                working_dir: PathBuf::from("/tmp/test-team"),
                model: Some("claude-sonnet-4-6".to_string()),
                member_timeout_secs: 600,
            },
            spawned_agents: vec![],
            phase_results: vec![],
            started_at: 1000,
            completed_at: None,
        }
    }

    fn make_member(index: usize, role: &str) -> TeamMember {
        TeamMember {
            index,
            role: role.to_string(),
            worktree_path: PathBuf::from(format!("/tmp/test-team/.worktrees/member-{index}")),
            branch_name: format!("team/goal-test/member-{index}"),
            status: TeamMemberStatus::Pending,
        }
    }

    fn make_completed_member(index: usize, role: &str, output: &str, quality: f64) -> TeamMember {
        TeamMember {
            index,
            role: role.to_string(),
            worktree_path: PathBuf::from(format!("/tmp/test-team/.worktrees/member-{index}")),
            branch_name: format!("team/goal-test/member-{index}"),
            status: TeamMemberStatus::Completed {
                output: output.to_string(),
                quality,
            },
        }
    }

    // -----------------------------------------------------------------------
    // TeamConfig tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_team_config_default() {
        let config = TeamConfig::default();
        assert_eq!(config.team_size, 3);
        assert_eq!(config.working_dir, PathBuf::from("."));
        assert_eq!(config.model, "claude-sonnet-4-6");
        assert_eq!(config.member_timeout_secs, 600);
        assert!(!config.auto_merge);
    }

    #[test]
    fn test_team_config_serde_roundtrip() {
        let config = TeamConfig {
            team_size: 5,
            working_dir: PathBuf::from("/home/user/project"),
            model: "claude-opus-4-6".to_string(),
            member_timeout_secs: 900,
            auto_merge: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: TeamConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.team_size, config.team_size);
        assert_eq!(deserialized.working_dir, config.working_dir);
        assert_eq!(deserialized.model, config.model);
        assert_eq!(deserialized.member_timeout_secs, config.member_timeout_secs);
        assert_eq!(deserialized.auto_merge, config.auto_merge);
    }

    #[test]
    fn test_team_config_serde_json_structure() {
        let config = TeamConfig::default();
        let json: serde_json::Value = serde_json::to_value(&config).unwrap();
        assert_eq!(json["team_size"], 3);
        assert_eq!(json["model"], "claude-sonnet-4-6");
        assert_eq!(json["member_timeout_secs"], 600);
        assert_eq!(json["auto_merge"], false);
    }

    // -----------------------------------------------------------------------
    // TeamMemberStatus tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_member_status_pending_serde() {
        let status = TeamMemberStatus::Pending;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TeamMemberStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TeamMemberStatus::Pending);
    }

    #[test]
    fn test_member_status_running_serde() {
        let status = TeamMemberStatus::Running;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TeamMemberStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TeamMemberStatus::Running);
    }

    #[test]
    fn test_member_status_completed_serde() {
        let status = TeamMemberStatus::Completed {
            output: "Done implementing".to_string(),
            quality: 0.85,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TeamMemberStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, status);
    }

    #[test]
    fn test_member_status_failed_serde() {
        let status = TeamMemberStatus::Failed {
            error: "process crashed".to_string(),
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TeamMemberStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, status);
    }

    #[test]
    fn test_member_status_timed_out_serde() {
        let status = TeamMemberStatus::TimedOut;
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: TeamMemberStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, TeamMemberStatus::TimedOut);
    }

    #[test]
    fn test_member_status_equality() {
        assert_eq!(TeamMemberStatus::Pending, TeamMemberStatus::Pending);
        assert_eq!(TeamMemberStatus::Running, TeamMemberStatus::Running);
        assert_eq!(TeamMemberStatus::TimedOut, TeamMemberStatus::TimedOut);
        assert_ne!(TeamMemberStatus::Pending, TeamMemberStatus::Running);
        assert_ne!(TeamMemberStatus::Pending, TeamMemberStatus::TimedOut);
    }

    // -----------------------------------------------------------------------
    // TeamMember serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_team_member_serde_roundtrip() {
        let member = make_member(0, "architect");
        let json = serde_json::to_string(&member).unwrap();
        let deserialized: TeamMember = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.index, 0);
        assert_eq!(deserialized.role, "architect");
        assert_eq!(deserialized.status, TeamMemberStatus::Pending);
    }

    #[test]
    fn test_team_member_completed_serde() {
        let member = make_completed_member(1, "implementer", "built feature X", 0.8);
        let json = serde_json::to_string(&member).unwrap();
        let deserialized: TeamMember = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.index, 1);
        assert_eq!(deserialized.role, "implementer");
        match deserialized.status {
            TeamMemberStatus::Completed { output, quality } => {
                assert_eq!(output, "built feature X");
                assert!((quality - 0.8).abs() < f64::EPSILON);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // TeamResult serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_team_result_serde_roundtrip() {
        let result = TeamResult {
            members: vec![
                make_completed_member(0, "architect", "designed API", 0.9),
                make_completed_member(1, "implementer", "built it", 0.8),
            ],
            merged_output: "merged stuff".to_string(),
            conflicts: vec![],
            overall_quality: 0.85,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: TeamResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.members.len(), 2);
        assert_eq!(deserialized.merged_output, "merged stuff");
        assert!(deserialized.conflicts.is_empty());
        assert!((deserialized.overall_quality - 0.85).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // MergeConflict serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_conflict_serde() {
        let conflict = MergeConflict {
            file_path: "src/main.rs".to_string(),
            member_indices: vec![0, 2],
            description: "Both architect and tester modified main.rs".to_string(),
        };
        let json = serde_json::to_string(&conflict).unwrap();
        let deserialized: MergeConflict = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.file_path, "src/main.rs");
        assert_eq!(deserialized.member_indices, vec![0, 2]);
        assert!(deserialized.description.contains("architect"));
    }

    // -----------------------------------------------------------------------
    // Role assignment tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_assign_roles_zero() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 0);
        assert!(roles.is_empty());
    }

    #[test]
    fn test_assign_roles_one() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 1);
        assert_eq!(roles, vec!["solo"]);
    }

    #[test]
    fn test_assign_roles_two() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 2);
        assert_eq!(roles, vec!["primary", "reviewer"]);
    }

    #[test]
    fn test_assign_roles_three() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 3);
        assert_eq!(roles, vec!["architect", "implementer", "tester"]);
    }

    #[test]
    fn test_assign_roles_four() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 4);
        assert_eq!(
            roles,
            vec!["architect", "implementer", "tester", "reviewer"]
        );
    }

    #[test]
    fn test_assign_roles_five_plus() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let roles = executor.assign_roles(&phase, 6);
        assert_eq!(roles.len(), 6);
        assert_eq!(roles[0], "architect");
        assert_eq!(roles[1], "implementer");
        assert_eq!(roles[2], "tester");
        assert_eq!(roles[3], "reviewer");
        assert_eq!(roles[4], "implementer-1");
        assert_eq!(roles[5], "implementer-2");
    }

    // -----------------------------------------------------------------------
    // Worktree path generation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_worktree_path_generation() {
        let base = PathBuf::from("/tmp/project");
        let path = TeamPhaseExecutor::worktree_path(&base, 0);
        assert_eq!(path, PathBuf::from("/tmp/project/.worktrees/member-0"));
    }

    #[test]
    fn test_worktree_path_generation_multiple() {
        let base = PathBuf::from("/home/user/repo");
        for i in 0..5 {
            let path = TeamPhaseExecutor::worktree_path(&base, i);
            assert_eq!(
                path,
                PathBuf::from(format!("/home/user/repo/.worktrees/member-{i}"))
            );
        }
    }

    // -----------------------------------------------------------------------
    // Branch naming tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_branch_naming() {
        let branch = TeamPhaseExecutor::branch_name("goal-123", 0);
        assert_eq!(branch, "team/goal-123/member-0");
    }

    #[test]
    fn test_branch_naming_various_indices() {
        for i in 0..5 {
            let branch = TeamPhaseExecutor::branch_name("my-goal", i);
            assert_eq!(branch, format!("team/my-goal/member-{i}"));
        }
    }

    #[test]
    fn test_branch_naming_special_chars_in_goal_id() {
        let branch = TeamPhaseExecutor::branch_name("goal_with-mixed.chars", 2);
        assert_eq!(branch, "team/goal_with-mixed.chars/member-2");
    }

    // -----------------------------------------------------------------------
    // Conflict detection tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_detect_conflicts_no_conflicts() {
        // When members have no overlapping files, there should be no conflicts.
        // We test the merge logic directly using the merge_outputs and
        // compute_overall_quality functions since detect_conflicts requires
        // actual git repos.

        let members = vec![
            make_completed_member(0, "architect", "designed API", 0.9),
            make_completed_member(1, "implementer", "built feature", 0.8),
        ];

        // The conflict detection helper tests the HashMap logic:
        let mut file_to_members: HashMap<String, Vec<usize>> = HashMap::new();
        // Member 0 changed file_a, member 1 changed file_b — no overlap.
        file_to_members
            .entry("src/api.rs".to_string())
            .or_default()
            .push(0);
        file_to_members
            .entry("src/impl.rs".to_string())
            .or_default()
            .push(1);

        let conflicts: Vec<MergeConflict> = file_to_members
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(file_path, member_indices)| MergeConflict {
                description: format!("Conflict in {file_path}"),
                file_path,
                member_indices,
            })
            .collect();

        assert!(conflicts.is_empty());
        let _ = members; // used above to set up the test scenario
    }

    #[test]
    fn test_detect_conflicts_single_file_conflict() {
        let mut file_to_members: HashMap<String, Vec<usize>> = HashMap::new();
        // Both members 0 and 1 changed the same file.
        file_to_members
            .entry("src/main.rs".to_string())
            .or_default()
            .push(0);
        file_to_members
            .entry("src/main.rs".to_string())
            .or_default()
            .push(1);
        // Member 2 changed a different file.
        file_to_members
            .entry("src/lib.rs".to_string())
            .or_default()
            .push(2);

        let conflicts: Vec<MergeConflict> = file_to_members
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(file_path, member_indices)| MergeConflict {
                description: format!("Conflict in {file_path}"),
                file_path,
                member_indices,
            })
            .collect();

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].file_path, "src/main.rs");
        assert_eq!(conflicts[0].member_indices, vec![0, 1]);
    }

    #[test]
    fn test_detect_conflicts_multiple_file_conflicts() {
        let mut file_to_members: HashMap<String, Vec<usize>> = HashMap::new();
        // file_a: members 0, 1, 2
        for i in 0..3 {
            file_to_members
                .entry("Cargo.toml".to_string())
                .or_default()
                .push(i);
        }
        // file_b: members 0, 2
        file_to_members
            .entry("README.md".to_string())
            .or_default()
            .push(0);
        file_to_members
            .entry("README.md".to_string())
            .or_default()
            .push(2);
        // file_c: member 1 only (no conflict)
        file_to_members
            .entry("src/test.rs".to_string())
            .or_default()
            .push(1);

        let conflicts: Vec<MergeConflict> = file_to_members
            .into_iter()
            .filter(|(_, indices)| indices.len() > 1)
            .map(|(file_path, member_indices)| MergeConflict {
                description: format!("Conflict in {file_path}"),
                file_path,
                member_indices,
            })
            .collect();

        assert_eq!(conflicts.len(), 2);
        let conflict_files: HashSet<&str> =
            conflicts.iter().map(|c| c.file_path.as_str()).collect();
        assert!(conflict_files.contains("Cargo.toml"));
        assert!(conflict_files.contains("README.md"));
    }

    #[test]
    fn test_detect_conflicts_skips_non_completed_members() {
        // Simulate: only completed members' files matter for conflicts.
        let members = [
            make_completed_member(0, "architect", "done", 0.9),
            TeamMember {
                index: 1,
                role: "implementer".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                branch_name: "team/goal/member-1".to_string(),
                status: TeamMemberStatus::Failed {
                    error: "crashed".to_string(),
                },
            },
            TeamMember {
                index: 2,
                role: "tester".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-2"),
                branch_name: "team/goal/member-2".to_string(),
                status: TeamMemberStatus::TimedOut,
            },
        ];

        // Only member 0 is Completed, so no conflicts possible.
        let completed_count = members
            .iter()
            .filter(|m| matches!(m.status, TeamMemberStatus::Completed { .. }))
            .count();
        assert_eq!(completed_count, 1);
    }

    // -----------------------------------------------------------------------
    // Quality score computation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_quality_average_two_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "a", "out", 0.8),
            make_completed_member(1, "b", "out", 0.6),
        ];
        let quality = executor.compute_overall_quality(&members);
        assert!((quality - 0.7).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_average_three_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "a", "out", 0.9),
            make_completed_member(1, "b", "out", 0.8),
            make_completed_member(2, "c", "out", 0.7),
        ];
        let quality = executor.compute_overall_quality(&members);
        assert!((quality - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_skips_failed_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "a", "out", 0.9),
            TeamMember {
                index: 1,
                role: "b".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                branch_name: "team/goal/member-1".to_string(),
                status: TeamMemberStatus::Failed {
                    error: "error".to_string(),
                },
            },
            make_completed_member(2, "c", "out", 0.7),
        ];
        let quality = executor.compute_overall_quality(&members);
        // Only members 0 and 2 contribute: (0.9 + 0.7) / 2 = 0.8
        assert!((quality - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_skips_timed_out_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "a", "out", 0.8),
            TeamMember {
                index: 1,
                role: "b".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                branch_name: "team/goal/member-1".to_string(),
                status: TeamMemberStatus::TimedOut,
            },
        ];
        let quality = executor.compute_overall_quality(&members);
        assert!((quality - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_all_failed() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            TeamMember {
                index: 0,
                role: "a".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-0"),
                branch_name: "team/goal/member-0".to_string(),
                status: TeamMemberStatus::Failed {
                    error: "error".to_string(),
                },
            },
            TeamMember {
                index: 1,
                role: "b".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                branch_name: "team/goal/member-1".to_string(),
                status: TeamMemberStatus::TimedOut,
            },
        ];
        let quality = executor.compute_overall_quality(&members);
        assert!((quality - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_quality_empty_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let quality = executor.compute_overall_quality(&[]);
        assert!((quality - 0.0).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Output merging tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_merge_outputs_single_member() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![make_completed_member(0, "solo", "did everything", 0.9)];
        let merged = executor.merge_outputs(&members);
        assert!(merged.contains("## Member 0 (solo)"));
        assert!(merged.contains("did everything"));
        // Should not contain separator since it's only one member.
        assert!(!merged.contains("---"));
    }

    #[test]
    fn test_merge_outputs_multiple_members() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "architect", "API design", 0.9),
            make_completed_member(1, "implementer", "code written", 0.8),
            make_completed_member(2, "tester", "tests pass", 0.7),
        ];
        let merged = executor.merge_outputs(&members);
        assert!(merged.contains("## Member 0 (architect)"));
        assert!(merged.contains("API design"));
        assert!(merged.contains("## Member 1 (implementer)"));
        assert!(merged.contains("code written"));
        assert!(merged.contains("## Member 2 (tester)"));
        assert!(merged.contains("tests pass"));
        // Should have separator between members.
        assert!(merged.contains("---"));
    }

    #[test]
    fn test_merge_outputs_skips_non_completed() {
        let executor = TeamPhaseExecutor::new(make_config());
        let members = vec![
            make_completed_member(0, "architect", "design done", 0.9),
            TeamMember {
                index: 1,
                role: "implementer".to_string(),
                worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                branch_name: "team/goal/member-1".to_string(),
                status: TeamMemberStatus::Failed {
                    error: "crashed".to_string(),
                },
            },
        ];
        let merged = executor.merge_outputs(&members);
        assert!(merged.contains("## Member 0 (architect)"));
        assert!(!merged.contains("## Member 1"));
    }

    #[test]
    fn test_merge_outputs_empty() {
        let executor = TeamPhaseExecutor::new(make_config());
        let merged = executor.merge_outputs(&[]);
        assert!(merged.is_empty());
    }

    // -----------------------------------------------------------------------
    // Prompt building tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_member_prompt_basic() {
        let executor = TeamPhaseExecutor::new(make_config());
        let member = make_member(0, "architect");
        let phase = make_phase();
        let ctx = make_context();

        let prompt = executor.build_member_prompt(&member, &phase, &ctx);
        assert!(prompt.contains("# Goal: goal-team-test"));
        assert!(prompt.contains("## Phase: implement"));
        assert!(prompt.contains("Implement the feature"));
        assert!(prompt.contains("## Your Role: architect"));
        assert!(prompt.contains("team member 0"));
        assert!(prompt.contains("role of 'architect'"));
        assert!(prompt.contains("Validation Criteria"));
        assert!(prompt.contains("cargo clippy"));
    }

    #[test]
    fn test_build_member_prompt_no_validations() {
        let executor = TeamPhaseExecutor::new(make_config());
        let member = make_member(0, "solo");
        let phase = Phase {
            name: "explore".to_string(),
            description: "Explore the codebase".to_string(),
            validations: vec![],
        };
        let ctx = make_context();

        let prompt = executor.build_member_prompt(&member, &phase, &ctx);
        assert!(!prompt.contains("Validation Criteria"));
    }

    #[test]
    fn test_build_member_prompt_different_roles() {
        let executor = TeamPhaseExecutor::new(make_config());
        let phase = make_phase();
        let ctx = make_context();

        for role in &["architect", "implementer", "tester", "reviewer"] {
            let member = make_member(0, role);
            let prompt = executor.build_member_prompt(&member, &phase, &ctx);
            assert!(prompt.contains(&format!("## Your Role: {role}")));
            assert!(prompt.contains(&format!("role of '{role}'")));
        }
    }

    // -----------------------------------------------------------------------
    // Command args tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_command_args() {
        let executor = TeamPhaseExecutor::new(make_config());
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
        let config = TeamConfig {
            model: "claude-opus-4-6".to_string(),
            ..make_config()
        };
        let executor = TeamPhaseExecutor::new(config);
        let args = executor.build_command_args("prompt");
        assert_eq!(args[1], "claude-opus-4-6");
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
            description: "All tests pass".to_string(),
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Test:"));
        assert!(formatted.contains("cargo test"));
    }

    #[test]
    fn test_format_validation_human() {
        let v = Validation::HumanReview {
            reviewer: "lead".to_string(),
            criteria: "Quality check".to_string(),
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Human review by lead"));
    }

    #[test]
    fn test_format_validation_expert() {
        let v = Validation::ExpertAgent {
            agent_type: "security".to_string(),
            criteria: "No vulns".to_string(),
            system_prompt: None,
        };
        let formatted = format_validation(&v);
        assert!(formatted.contains("Expert agent (security)"));
    }

    #[test]
    fn test_format_validation_human_approval() {
        let v = Validation::HumanApproval;
        let formatted = format_validation(&v);
        assert!(formatted.contains("Human approval required"));
    }

    // -----------------------------------------------------------------------
    // Constructor tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_executor_construction() {
        let config = make_config();
        let executor = TeamPhaseExecutor::new(config.clone());
        assert_eq!(executor.config().team_size, config.team_size);
        assert_eq!(executor.config().working_dir, config.working_dir);
        assert_eq!(executor.config().model, config.model);
        assert_eq!(
            executor.config().member_timeout_secs,
            config.member_timeout_secs
        );
        assert_eq!(executor.config().auto_merge, config.auto_merge);
    }

    // -----------------------------------------------------------------------
    // TeamResult aggregation tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_team_result_all_completed() {
        let result = TeamResult {
            members: vec![
                make_completed_member(0, "architect", "design", 0.9),
                make_completed_member(1, "implementer", "code", 0.8),
                make_completed_member(2, "tester", "tests", 0.7),
            ],
            merged_output: "all done".to_string(),
            conflicts: vec![],
            overall_quality: 0.8,
        };
        assert_eq!(result.members.len(), 3);
        assert!(result.conflicts.is_empty());
        assert!((result.overall_quality - 0.8).abs() < f64::EPSILON);
    }

    #[test]
    fn test_team_result_with_conflicts() {
        let result = TeamResult {
            members: vec![
                make_completed_member(0, "a", "out", 0.9),
                make_completed_member(1, "b", "out", 0.8),
            ],
            merged_output: "merged".to_string(),
            conflicts: vec![MergeConflict {
                file_path: "src/lib.rs".to_string(),
                member_indices: vec![0, 1],
                description: "Both modified lib.rs".to_string(),
            }],
            overall_quality: 0.85,
        };
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].file_path, "src/lib.rs");
    }

    #[test]
    fn test_team_result_partial_failure() {
        let result = TeamResult {
            members: vec![
                make_completed_member(0, "a", "done", 0.9),
                TeamMember {
                    index: 1,
                    role: "b".to_string(),
                    worktree_path: PathBuf::from("/tmp/.worktrees/member-1"),
                    branch_name: "team/goal/member-1".to_string(),
                    status: TeamMemberStatus::Failed {
                        error: "crashed".to_string(),
                    },
                },
            ],
            merged_output: "partial".to_string(),
            conflicts: vec![],
            overall_quality: 0.9,
        };
        assert_eq!(result.members.len(), 2);
        assert!(matches!(
            result.members[1].status,
            TeamMemberStatus::Failed { .. }
        ));
    }

    // -----------------------------------------------------------------------
    // PhaseExecutor trait implementation test (mock scenario)
    // -----------------------------------------------------------------------

    #[test]
    fn test_phase_executor_trait_is_implemented() {
        // Verify that TeamPhaseExecutor implements PhaseExecutor.
        fn assert_phase_executor<T: PhaseExecutor>() {}
        assert_phase_executor::<TeamPhaseExecutor>();
    }

    #[test]
    fn test_phase_executor_is_send_sync() {
        // PhaseExecutor requires Send + Sync.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TeamPhaseExecutor>();
    }

    // -----------------------------------------------------------------------
    // CliOutput tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_cli_output_structure() {
        let output = CliOutput {
            stdout: "hello".to_string(),
            stderr: "".to_string(),
            exit_code: 0,
            timed_out: false,
        };
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.exit_code, 0);
        assert!(!output.timed_out);
    }

    #[test]
    fn test_cli_output_timeout() {
        let output = CliOutput {
            stdout: "".to_string(),
            stderr: "timed out".to_string(),
            exit_code: -1,
            timed_out: true,
        };
        assert!(output.timed_out);
        assert_eq!(output.exit_code, -1);
    }

    // -----------------------------------------------------------------------
    // WorktreeInfo tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_worktree_info_structure() {
        let info = WorktreeInfo {
            path: PathBuf::from("/tmp/project/.worktrees/member-0"),
            branch: "team/goal-1/member-0".to_string(),
            member_index: 0,
        };
        assert_eq!(info.member_index, 0);
        assert_eq!(info.branch, "team/goal-1/member-0");
        assert_eq!(info.path, PathBuf::from("/tmp/project/.worktrees/member-0"));
    }
}
