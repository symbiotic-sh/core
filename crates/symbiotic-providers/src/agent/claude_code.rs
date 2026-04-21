//! Claude Code agent provider.
//!
//! Executes the `claude` CLI as a subprocess, capturing stdout/stderr as task
//! output. Supports timeout, cancellation, and retry classification.

use async_trait::async_trait;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::{
    AgentProvider, CapabilitySet, ModelProvider, PricingInfo, ProviderAuth, ProviderCapability,
    ProviderClass, ProviderError, TaskRequest, TaskResult, TaskSession, TaskStatus,
};

/// Default timeout for agent tasks: 10 minutes.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Internal state for a running or completed task session.
struct SessionState {
    /// The subprocess handle; `None` once collected.
    child: Option<Child>,
    /// Collected result, populated once the process exits.
    result: Option<TaskResult>,
    /// Whether a timeout kill was issued.
    timed_out: bool,
    /// The configured timeout for this task.
    timeout_secs: u64,
    /// Unix timestamp when submitted.
    submitted_at: u64,
}

/// Claude Code agent provider.
///
/// Spawns `claude` CLI processes for task execution. The CLI binary name
/// can be overridden via `SYMBIOTIC_CLAUDE_CLI` env var.
pub struct ClaudeCodeProvider {
    auth: ProviderAuth,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
    sessions: Arc<Mutex<HashMap<String, SessionState>>>,
    /// Override for the CLI binary path (for testing).
    cli_command: String,
}

impl ClaudeCodeProvider {
    /// Create a new Claude Code provider.
    pub fn new(auth: ProviderAuth) -> Self {
        let cli_command =
            std::env::var("SYMBIOTIC_CLAUDE_CLI").unwrap_or_else(|_| "claude".to_string());
        Self {
            auth,
            capabilities: CapabilitySet::new(vec![
                ProviderCapability::AgentExecution,
                ProviderCapability::Completion,
                ProviderCapability::FunctionCall,
                ProviderCapability::Vision,
            ]),
            pricing: PricingInfo {
                input_per_1k_tokens: Some(0.015),
                output_per_1k_tokens: Some(0.075),
                per_agent_task: Some(0.10),
                ..Default::default()
            },
            sessions: Arc::new(Mutex::new(HashMap::new())),
            cli_command,
        }
    }

    /// Create a provider with a custom CLI command (for testing).
    #[cfg(test)]
    pub fn with_cli(auth: ProviderAuth, cli_command: String) -> Self {
        Self {
            auth,
            capabilities: CapabilitySet::new(vec![
                ProviderCapability::AgentExecution,
                ProviderCapability::Completion,
                ProviderCapability::FunctionCall,
                ProviderCapability::Vision,
            ]),
            pricing: PricingInfo {
                input_per_1k_tokens: Some(0.015),
                output_per_1k_tokens: Some(0.075),
                per_agent_task: Some(0.10),
                ..Default::default()
            },
            sessions: Arc::new(Mutex::new(HashMap::new())),
            cli_command,
        }
    }

    /// Build the CLI arguments for a task request.
    fn build_args(request: &TaskRequest) -> Vec<String> {
        let mut args = vec![
            "--print".to_string(),
            "--output-format".to_string(),
            "text".to_string(),
        ];

        if let Some(ref prompt) = request.system_prompt {
            if !prompt.trim().is_empty() {
                args.push("--system-prompt".to_string());
                args.push(prompt.clone());
            }
        }

        args.push(request.task.clone());
        args
    }

    /// Spawn the CLI process.
    fn spawn_process(&self, request: &TaskRequest) -> Result<Child, ProviderError> {
        let args = Self::build_args(request);
        let mut cmd = Command::new(&self.cli_command);
        cmd.args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .stdin(Stdio::null());

        if let Some(ref cwd) = request.working_directory {
            if !cwd.trim().is_empty() {
                cmd.current_dir(cwd);
            }
        }

        // Pass API key via environment if available.
        if let ProviderAuth::ApiKey(ref key) = self.auth {
            cmd.env("ANTHROPIC_API_KEY", key);
        }

        cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable(format!(
                    "claude CLI not found: {}. Install with: npm install -g @anthropic-ai/claude-code",
                    self.cli_command
                ))
            } else {
                ProviderError::RequestFailed(format!("failed to spawn claude CLI: {e}"))
            }
        })
    }

    /// Collect output from a finished child process.
    async fn collect_output(child: &mut Child, timed_out: bool, session_id: &str) -> TaskResult {
        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        if let Some(ref mut stdout) = child.stdout {
            let _ = stdout.read_to_end(&mut stdout_buf).await;
        }
        if let Some(ref mut stderr) = child.stderr {
            let _ = stderr.read_to_end(&mut stderr_buf).await;
        }

        let exit_status = child.try_wait().ok().flatten();
        let stdout_str = String::from_utf8_lossy(&stdout_buf);
        let stderr_str = String::from_utf8_lossy(&stderr_buf);

        let mut output = String::new();
        if !stdout_str.is_empty() {
            output.push_str(&stdout_str);
        }
        if !stderr_str.is_empty() {
            if !output.is_empty() {
                output.push_str("\n--- stderr ---\n");
            }
            output.push_str(&stderr_str);
        }

        let (status, tokens_in, tokens_out, cost) = if timed_out {
            (TaskStatus::TimedOut, None, None, None)
        } else {
            match exit_status {
                Some(status) if status.success() => {
                    let (t_in, t_out, c) = parse_usage_from_output(&stdout_str);
                    (TaskStatus::Completed, t_in, t_out, c)
                }
                Some(_) => {
                    let (t_in, t_out, c) = parse_usage_from_output(&stdout_str);
                    (TaskStatus::Failed, t_in, t_out, c)
                }
                None => (TaskStatus::Running, None, None, None),
            }
        };

        TaskResult {
            session_id: session_id.to_string(),
            status,
            output,
            artifacts: Vec::new(),
            total_input_tokens: tokens_in,
            total_output_tokens: tokens_out,
            cost_usd: cost,
        }
    }
}

impl ModelProvider for ClaudeCodeProvider {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Cloud
    }

    fn model_name(&self) -> &str {
        "claude-code"
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        Some(&self.pricing)
    }
}

#[async_trait]
impl AgentProvider for ClaudeCodeProvider {
    async fn submit_task(&self, request: &TaskRequest) -> Result<TaskSession, ProviderError> {
        let session_id = build_session_id("claude_code", &request.task);
        let submitted_at = now_unix();
        let timeout_secs = request.timeout_seconds.unwrap_or(DEFAULT_TIMEOUT_SECS);

        let child = self.spawn_process(request)?;

        let state = SessionState {
            child: Some(child),
            result: None,
            timed_out: false,
            timeout_secs,
            submitted_at,
        };

        let mut sessions = self.sessions.lock().await;
        sessions.insert(session_id.clone(), state);

        // Spawn a background task to enforce the timeout.
        let sessions_ref = Arc::clone(&self.sessions);
        let sid = session_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(timeout_secs)).await;
            let mut sessions = sessions_ref.lock().await;
            if let Some(state) = sessions.get_mut(&sid) {
                if state.result.is_none() {
                    // Process still running -- kill it.
                    state.timed_out = true;
                    if let Some(ref mut child) = state.child {
                        let _ = child.kill().await;
                    }
                }
            }
        });

        Ok(TaskSession {
            session_id,
            provider: self.name().to_string(),
            status: TaskStatus::Running,
            submitted_at,
        })
    }

    async fn poll_status(&self, session_id: &str) -> Result<TaskStatus, ProviderError> {
        let mut sessions = self.sessions.lock().await;
        let state = sessions.get_mut(session_id).ok_or_else(|| {
            ProviderError::InvalidResponse(format!("claude session not found: {session_id}"))
        })?;

        // If we already have a result, return its status.
        if let Some(ref result) = state.result {
            return Ok(result.status);
        }

        // Check if the process has exited.
        if let Some(ref mut child) = state.child {
            match child.try_wait() {
                Ok(Some(_exit)) => {
                    // Process finished -- collect output.
                    let mut child = state.child.take().expect("child exists");
                    let result =
                        Self::collect_output(&mut child, state.timed_out, session_id).await;
                    let status = result.status;
                    state.result = Some(result);
                    Ok(status)
                }
                Ok(None) => {
                    // Still running; check timeout.
                    let elapsed = now_unix().saturating_sub(state.submitted_at);
                    if elapsed >= state.timeout_secs {
                        state.timed_out = true;
                        let _ = child.kill().await;
                        let mut child = state.child.take().expect("child exists");
                        let result = Self::collect_output(&mut child, true, session_id).await;
                        let status = result.status;
                        state.result = Some(result);
                        Ok(status)
                    } else {
                        Ok(TaskStatus::Running)
                    }
                }
                Err(e) => Err(ProviderError::RequestFailed(format!(
                    "failed to poll claude process: {e}"
                ))),
            }
        } else {
            // No child and no result -- shouldn't happen.
            Err(ProviderError::InvalidResponse(format!(
                "claude session in invalid state: {session_id}"
            )))
        }
    }

    async fn get_result(&self, session_id: &str) -> Result<TaskResult, ProviderError> {
        let mut sessions = self.sessions.lock().await;
        let state = sessions.get_mut(session_id).ok_or_else(|| {
            ProviderError::InvalidResponse(format!("claude session not found: {session_id}"))
        })?;

        // If result already collected, return it.
        if let Some(ref result) = state.result {
            return Ok(result.clone());
        }

        // Try to collect if process has finished.
        if let Some(ref mut child) = state.child {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let mut child = state.child.take().expect("child exists");
                    let result =
                        Self::collect_output(&mut child, state.timed_out, session_id).await;
                    state.result = Some(result.clone());
                    Ok(result)
                }
                Ok(None) => Err(ProviderError::RequestFailed(format!(
                    "claude task still running: {session_id}"
                ))),
                Err(e) => Err(ProviderError::RequestFailed(format!(
                    "failed to check claude process: {e}"
                ))),
            }
        } else {
            Err(ProviderError::InvalidResponse(format!(
                "claude session in invalid state: {session_id}"
            )))
        }
    }

    async fn cancel(&self, session_id: &str) -> Result<(), ProviderError> {
        let mut sessions = self.sessions.lock().await;
        let state = sessions.get_mut(session_id).ok_or_else(|| {
            ProviderError::InvalidResponse(format!("claude session not found: {session_id}"))
        })?;

        if let Some(ref mut child) = state.child {
            let _ = child.kill().await;
            let mut child = state.child.take().expect("child exists");
            let mut result = Self::collect_output(&mut child, false, session_id).await;
            result.status = TaskStatus::Cancelled;
            state.result = Some(result);
        }

        sessions.remove(session_id);
        Ok(())
    }
}

/// Classify whether a provider error is retryable.
pub fn is_retryable(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::RateLimited { .. }
            | ProviderError::TaskTimeout { .. }
            | ProviderError::RequestFailed(_)
    )
}

/// Classify whether a provider error is terminal (should not retry).
pub fn is_terminal(error: &ProviderError) -> bool {
    matches!(
        error,
        ProviderError::AuthFailed(_)
            | ProviderError::ConfigError(_)
            | ProviderError::Unavailable(_)
            | ProviderError::UnsupportedCapability(_)
            | ProviderError::SensitivityViolation { .. }
    )
}

/// Parse token usage from Claude Code CLI output.
///
/// Claude Code outputs usage info in its stderr/text when using `--output-format text`.
/// We look for patterns like "Input tokens: NNN" and "Output tokens: NNN".
fn parse_usage_from_output(output: &str) -> (Option<u64>, Option<u64>, Option<f64>) {
    let mut input_tokens = None;
    let mut output_tokens = None;
    let mut cost = None;

    for line in output.lines() {
        let line_lower = line.to_lowercase();
        if line_lower.contains("input tokens") || line_lower.contains("input_tokens") {
            if let Some(n) = extract_number(line) {
                input_tokens = Some(n);
            }
        }
        if line_lower.contains("output tokens") || line_lower.contains("output_tokens") {
            if let Some(n) = extract_number(line) {
                output_tokens = Some(n);
            }
        }
        if line_lower.contains("cost") && line_lower.contains('$') {
            if let Some(c) = extract_cost(line) {
                cost = Some(c);
            }
        }
    }

    (input_tokens, output_tokens, cost)
}

/// Extract the first numeric value from a string.
fn extract_number(s: &str) -> Option<u64> {
    s.split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .find_map(|part| part.parse::<u64>().ok())
}

/// Extract a dollar cost value from a string (e.g. "$1.23" or "cost: $0.05").
fn extract_cost(s: &str) -> Option<f64> {
    if let Some(dollar_pos) = s.find('$') {
        let after = &s[dollar_pos + 1..];
        let num_str: String = after
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        num_str.parse::<f64>().ok()
    } else {
        None
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs()
}

fn build_session_id(provider: &str, task: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    provider.hash(&mut hasher);
    task.hash(&mut hasher);
    now_unix().hash(&mut hasher);
    format!("{provider}_{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata() {
        let provider = ClaudeCodeProvider::new(ProviderAuth::ApiKey("test-key".into()));
        assert_eq!(provider.name(), "claude-code");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "claude-code");
        assert!(provider
            .capabilities()
            .has(ProviderCapability::AgentExecution));
        assert!(provider.capabilities().has(ProviderCapability::Completion));
        assert!(provider
            .capabilities()
            .has(ProviderCapability::FunctionCall));
        assert!(provider.capabilities().has(ProviderCapability::Vision));
        assert!(provider.pricing().is_some());
    }

    #[test]
    fn build_args_basic() {
        let request = TaskRequest {
            task: "fix the bug".into(),
            system_prompt: None,
            working_directory: None,
            timeout_seconds: None,
            context_files: vec![],
        };
        let args = ClaudeCodeProvider::build_args(&request);
        assert!(args.contains(&"--print".to_string()));
        assert!(args.contains(&"fix the bug".to_string()));
    }

    #[test]
    fn build_args_with_system_prompt() {
        let request = TaskRequest {
            task: "refactor code".into(),
            system_prompt: Some("be concise".into()),
            working_directory: Some("/tmp".into()),
            timeout_seconds: Some(300),
            context_files: vec!["main.rs".into()],
        };
        let args = ClaudeCodeProvider::build_args(&request);
        assert!(args.contains(&"--system-prompt".to_string()));
        assert!(args.contains(&"be concise".to_string()));
        assert!(args.contains(&"refactor code".to_string()));
    }

    #[test]
    fn parse_usage_extracts_tokens() {
        let output = "Some text\nInput tokens: 1234\nOutput tokens: 567\nCost: $0.05\n";
        let (input, output_t, cost) = parse_usage_from_output(output);
        assert_eq!(input, Some(1234));
        assert_eq!(output_t, Some(567));
        assert!((cost.unwrap() - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn parse_usage_no_data() {
        let output = "Hello world, no usage info here.";
        let (input, output_t, cost) = parse_usage_from_output(output);
        assert_eq!(input, None);
        assert_eq!(output_t, None);
        assert_eq!(cost, None);
    }

    #[test]
    fn error_classification() {
        assert!(is_retryable(&ProviderError::RateLimited {
            retry_after_ms: 1000
        }));
        assert!(is_retryable(&ProviderError::TaskTimeout {
            timeout_seconds: 600
        }));
        assert!(is_retryable(&ProviderError::RequestFailed(
            "network".into()
        )));
        assert!(!is_retryable(&ProviderError::AuthFailed("bad key".into())));

        assert!(is_terminal(&ProviderError::AuthFailed("bad key".into())));
        assert!(is_terminal(&ProviderError::Unavailable("gone".into())));
        assert!(!is_terminal(&ProviderError::RateLimited {
            retry_after_ms: 1000
        }));
    }

    #[tokio::test]
    async fn submit_with_missing_cli_returns_unavailable() {
        let provider = ClaudeCodeProvider::with_cli(
            ProviderAuth::ApiKey("test-key".into()),
            "nonexistent-claude-binary-xyz".to_string(),
        );
        let request = TaskRequest {
            task: "test task".into(),
            system_prompt: None,
            working_directory: None,
            timeout_seconds: None,
            context_files: vec![],
        };
        let err = provider.submit_task(&request).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(_)));
    }

    #[tokio::test]
    async fn submit_poll_result_lifecycle_with_echo() {
        // Use `echo` as a mock CLI to test the full lifecycle.
        let provider = ClaudeCodeProvider::with_cli(
            ProviderAuth::ApiKey("test-key".into()),
            "echo".to_string(),
        );
        let request = TaskRequest {
            task: "hello from test".into(),
            system_prompt: None,
            working_directory: None,
            timeout_seconds: Some(10),
            context_files: vec![],
        };
        let session = provider
            .submit_task(&request)
            .await
            .expect("submit should succeed");
        assert_eq!(session.provider, "claude-code");
        assert_eq!(session.status, TaskStatus::Running);
        assert!(session.session_id.starts_with("claude_code_"));

        // Give the echo process time to finish.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let status = provider
            .poll_status(&session.session_id)
            .await
            .expect("poll should succeed");
        assert_eq!(status, TaskStatus::Completed);

        let result = provider
            .get_result(&session.session_id)
            .await
            .expect("get_result should succeed");
        assert_eq!(result.status, TaskStatus::Completed);
        // echo outputs the args, so output should contain our task text.
        assert!(result.output.contains("hello from test"));
    }

    #[tokio::test]
    async fn cancel_kills_process() {
        // Use `sleep` as a long-running mock process.
        let provider = ClaudeCodeProvider::with_cli(
            ProviderAuth::ApiKey("test-key".into()),
            "sleep".to_string(),
        );
        let request = TaskRequest {
            task: "60".into(), // sleep 60 seconds
            system_prompt: None,
            working_directory: None,
            timeout_seconds: Some(60),
            context_files: vec![],
        };
        let session = provider
            .submit_task(&request)
            .await
            .expect("submit should succeed");

        // Cancel immediately.
        provider
            .cancel(&session.session_id)
            .await
            .expect("cancel should succeed");

        // Session should be removed after cancel.
        let err = provider.get_result(&session.session_id).await.unwrap_err();
        assert!(matches!(err, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        // Use `sleep` with a very short timeout.
        let provider = ClaudeCodeProvider::with_cli(
            ProviderAuth::ApiKey("test-key".into()),
            "sleep".to_string(),
        );
        let request = TaskRequest {
            task: "60".into(), // sleep 60 seconds
            system_prompt: None,
            working_directory: None,
            timeout_seconds: Some(1), // 1 second timeout
            context_files: vec![],
        };
        let session = provider
            .submit_task(&request)
            .await
            .expect("submit should succeed");

        // Wait for timeout to fire.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let status = provider
            .poll_status(&session.session_id)
            .await
            .expect("poll should succeed");
        assert_eq!(status, TaskStatus::TimedOut);
    }

    #[tokio::test]
    async fn failed_process_returns_failed_status() {
        // Use `false` which always exits with code 1.
        let provider = ClaudeCodeProvider::with_cli(
            ProviderAuth::ApiKey("test-key".into()),
            "false".to_string(),
        );
        let request = TaskRequest {
            task: String::new(),
            system_prompt: None,
            working_directory: None,
            timeout_seconds: Some(10),
            context_files: vec![],
        };
        let session = provider
            .submit_task(&request)
            .await
            .expect("submit should succeed");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let status = provider
            .poll_status(&session.session_id)
            .await
            .expect("poll should succeed");
        assert_eq!(status, TaskStatus::Failed);
    }
}
