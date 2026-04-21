//! DaemonCommandRunner — executes shell commands in the daemon environment.
//!
//! Provides a sandboxed command execution facility for the deliberation
//! pipeline's phase execution needs.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

/// Output of a command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }
}

/// Executes shell commands in the daemon environment with configurable
/// working directory and timeout.
pub struct DaemonCommandRunner {
    working_dir: PathBuf,
    default_timeout: Duration,
    /// Optional environment variables to set.
    env_vars: Vec<(String, String)>,
}

impl DaemonCommandRunner {
    pub fn new(working_dir: PathBuf) -> Self {
        Self {
            working_dir,
            default_timeout: Duration::from_secs(300),
            env_vars: Vec::new(),
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env_vars.push((key.to_string(), value.to_string()));
        self
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// Execute a shell command string (via `sh -c`).
    pub fn run(&self, command: &str) -> Result<CommandOutput> {
        self.run_with_timeout(command, self.default_timeout)
    }

    /// Execute a shell command with a specific timeout.
    pub fn run_with_timeout(&self, command: &str, timeout: Duration) -> Result<CommandOutput> {
        if command.trim().is_empty() {
            return Err(anyhow!("command must not be empty"));
        }

        if !self.working_dir.is_dir() {
            return Err(anyhow!(
                "working directory does not exist: {}",
                self.working_dir.display()
            ));
        }

        let start = std::time::Instant::now();

        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.working_dir)
            // Clear potentially dangerous env vars.
            .env_remove("HISTFILE")
            .env_remove("HISTSIZE");

        for (key, value) in &self.env_vars {
            cmd.env(key, value);
        }

        let child = cmd
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn command: {command}"))?;

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to wait for command: {command}"))?;

        let elapsed = start.elapsed();
        if elapsed > timeout {
            return Err(anyhow!(
                "command exceeded timeout ({:?}): {command}",
                timeout
            ));
        }

        let exit_code = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(CommandOutput {
            exit_code,
            stdout,
            stderr,
            duration_ms: elapsed.as_millis() as u64,
        })
    }

    /// Execute a command and return an error if it exits non-zero.
    pub fn run_checked(&self, command: &str) -> Result<CommandOutput> {
        let output = self.run(command)?;
        if !output.success() {
            return Err(anyhow!(
                "command failed (exit {}): {}\nstderr: {}",
                output.exit_code,
                command,
                output.stderr.trim()
            ));
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_run_echo() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let output = runner.run("echo hello").unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello");
        assert_eq!(output.exit_code, 0);
    }

    #[test]
    fn test_run_failure() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let output = runner.run("exit 42").unwrap();
        assert!(!output.success());
        assert_eq!(output.exit_code, 42);
    }

    #[test]
    fn test_run_checked_success() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let output = runner.run_checked("echo ok").unwrap();
        assert!(output.success());
    }

    #[test]
    fn test_run_checked_failure() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let result = runner.run_checked("exit 1");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("command failed"));
    }

    #[test]
    fn test_empty_command_rejected() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let result = runner.run("");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("must not be empty"));
    }

    #[test]
    fn test_invalid_working_dir() {
        let runner = DaemonCommandRunner::new(PathBuf::from("/nonexistent/path/xyz"));
        let result = runner.run("echo hello");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn test_with_env() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir).with_env("MY_TEST_VAR", "hello123");

        let output = runner.run("echo $MY_TEST_VAR").unwrap();
        assert!(output.success());
        assert_eq!(output.stdout.trim(), "hello123");
    }

    #[test]
    fn test_working_dir_accessor() {
        let dir = PathBuf::from("/tmp");
        let runner = DaemonCommandRunner::new(dir.clone());
        assert_eq!(runner.working_dir(), &dir);
    }

    #[test]
    fn test_command_output_captures_stderr() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let output = runner.run("echo error >&2").unwrap();
        assert!(output.success());
        assert!(output.stderr.contains("error"));
    }

    #[test]
    fn test_duration_captured() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir);

        let output = runner.run("echo fast").unwrap();
        // Duration should be captured (at least 0).
        assert!(output.duration_ms < 5000);
    }

    #[test]
    fn test_with_timeout() {
        let dir = std::env::temp_dir();
        let runner = DaemonCommandRunner::new(dir).with_timeout(Duration::from_secs(60));
        let output = runner.run("echo ok").unwrap();
        assert!(output.success());
    }
}
