//! Claude Code CLI completion provider.
//!
//! Uses the `claude` CLI in print mode (`-p`) as a completion backend,
//! allowing the daemon's internal ReAct loop to use the user's existing
//! Claude Code subscription (Pro/Max) instead of a separate API key.
//!
//! The CLI is invoked with `--output-format json` and `--max-turns 1`
//! to disable Claude Code's own tool use — we only want the raw LLM
//! response for our ReAct loop to parse.

use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderCapability, ProviderClass, ProviderError, Role,
};

/// Completion provider that shells out to the `claude` CLI.
///
/// Spawns `claude -p --output-format json --max-turns 1` per completion
/// request, parsing the JSON result. Uses the user's existing Claude Code
/// authentication (OAuth/subscription) — no API key required.
pub struct ClaudeCodeCompletionProvider {
    /// Path to the `claude` binary (default: "claude").
    cli_command: String,
    /// Model override (e.g. "sonnet", "opus"). None = CLI default.
    model: Option<String>,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

impl ClaudeCodeCompletionProvider {
    /// Create a new provider.
    ///
    /// # Arguments
    /// * `cli_command` — Path to `claude` binary (or just `"claude"` for PATH lookup).
    /// * `model` — Optional model override. `None` uses the CLI's default.
    pub fn new(cli_command: String, model: Option<String>) -> Self {
        Self {
            cli_command,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
            pricing: PricingInfo {
                // Subscription-based — no per-token cost to the user.
                // We report zeros so metering doesn't flag phantom costs.
                input_per_1k_tokens: Some(0.0),
                output_per_1k_tokens: Some(0.0),
                ..Default::default()
            },
        }
    }

    /// Check if the CLI binary is available on PATH.
    pub fn is_available(&self) -> bool {
        std::process::Command::new(&self.cli_command)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl ModelProvider for ClaudeCodeCompletionProvider {
    fn name(&self) -> &str {
        "claude-code"
    }

    fn provider_class(&self) -> ProviderClass {
        // Subscription goes through cloud, but billing is flat-rate.
        ProviderClass::Cloud
    }

    fn model_name(&self) -> &str {
        match &self.model {
            Some(m) => m.as_str(),
            None => "claude-code-default",
        }
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        Some(&self.pricing)
    }
}

// -- Claude Code JSON output types -------------------------------------------

#[derive(Deserialize)]
struct ClaudeCodeJsonOutput {
    /// The text result from the LLM.
    result: Option<String>,
    /// Token usage (may not always be present).
    usage: Option<ClaudeCodeUsage>,
    /// Cost in USD for this call (logged, not billed — subscription).
    #[allow(dead_code)]
    cost_usd: Option<f64>,
    /// Whether the response is an error.
    is_error: Option<bool>,
}

#[derive(Deserialize)]
struct ClaudeCodeUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

// -- CompletionProvider impl -------------------------------------------------

/// Build the user prompt from non-system messages.
///
/// Concatenates user and assistant messages into a single text prompt
/// for the CLI's `-p` mode. Multi-turn conversations are formatted as
/// labeled turns.
fn build_prompt(messages: &[crate::ChatMessage]) -> String {
    let non_system: Vec<_> = messages.iter().filter(|m| m.role != Role::System).collect();

    if non_system.len() == 1 {
        return non_system[0].content.clone();
    }

    let mut prompt = String::new();
    for msg in non_system {
        let label = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => unreachable!(),
        };
        prompt.push_str(label);
        prompt.push_str(": ");
        prompt.push_str(&msg.content);
        prompt.push('\n');
    }
    prompt
}

/// Extract system messages from the conversation, concatenated.
fn extract_system_prompt(messages: &[crate::ChatMessage]) -> Option<String> {
    let parts: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == Role::System)
        .map(|m| m.content.as_str())
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

#[async_trait]
impl CompletionProvider for ClaudeCodeCompletionProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut cmd = Command::new(&self.cli_command);

        cmd.arg("-p"); // print mode (non-interactive)
        cmd.args(["--output-format", "json"]);
        cmd.args(["--max-turns", "1"]); // single turn, no tool use loops

        // Model override
        if let Some(ref model) = self.model {
            cmd.args(["--model", model]);
        }

        // System prompt
        if let Some(system) = extract_system_prompt(&request.messages) {
            cmd.args(["--system-prompt", &system]);
        }

        // User prompt (the actual conversation content)
        let prompt = build_prompt(&request.messages);
        cmd.arg(&prompt);

        // No stdin, capture stdout/stderr
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable(format!(
                    "claude CLI not found at '{}'. Install: npm i -g @anthropic-ai/claude-code",
                    self.cli_command
                ))
            } else {
                ProviderError::RequestFailed(format!("failed to spawn claude CLI: {e}"))
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(ProviderError::RequestFailed(format!(
                "claude CLI exited with {}: {}{}",
                output.status,
                stderr.chars().take(500).collect::<String>(),
                if !stdout.is_empty() {
                    format!("\nstdout: {}", stdout.chars().take(500).collect::<String>())
                } else {
                    String::new()
                }
            )));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);

        // Parse JSON output
        let parsed: ClaudeCodeJsonOutput = serde_json::from_str(&stdout).map_err(|e| {
            ProviderError::InvalidResponse(format!(
                "failed to parse claude CLI JSON: {e}\nraw output: {}",
                stdout.chars().take(500).collect::<String>()
            ))
        })?;

        if parsed.is_error == Some(true) {
            return Err(ProviderError::RequestFailed(format!(
                "claude CLI returned error: {}",
                parsed.result.as_deref().unwrap_or("unknown error")
            )));
        }

        let content = parsed.result.unwrap_or_default();
        let (input_tokens, output_tokens) = match parsed.usage {
            Some(u) => (u.input_tokens, u.output_tokens),
            None => (None, None),
        };

        Ok(CompletionResponse {
            content,
            model: self
                .model
                .clone()
                .unwrap_or_else(|| "claude-code".to_string()),
            input_tokens,
            output_tokens,
            finish_reason: Some("stop".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatMessage;

    #[test]
    fn provider_metadata() {
        let provider = ClaudeCodeCompletionProvider::new("claude".into(), None);
        assert_eq!(provider.name(), "claude-code");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "claude-code-default");
        assert!(provider.capabilities().has(ProviderCapability::Completion));
    }

    #[test]
    fn provider_metadata_with_model() {
        let provider = ClaudeCodeCompletionProvider::new("claude".into(), Some("sonnet".into()));
        assert_eq!(provider.model_name(), "sonnet");
    }

    #[test]
    fn build_prompt_single_message() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Hello".into(),
        }];
        assert_eq!(build_prompt(&messages), "Hello");
    }

    #[test]
    fn build_prompt_multi_turn() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "You are helpful.".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "Hi!".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "How are you?".into(),
            },
        ];
        let prompt = build_prompt(&messages);
        assert!(!prompt.contains("You are helpful")); // system excluded
        assert!(prompt.contains("User: Hello"));
        assert!(prompt.contains("Assistant: Hi!"));
        assert!(prompt.contains("User: How are you?"));
    }

    #[test]
    fn extract_system_prompt_works() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "Rule 1".into(),
            },
            ChatMessage {
                role: Role::System,
                content: "Rule 2".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            },
        ];
        assert_eq!(
            extract_system_prompt(&messages),
            Some("Rule 1\nRule 2".into())
        );
    }

    #[test]
    fn extract_system_prompt_none() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Hello".into(),
        }];
        assert_eq!(extract_system_prompt(&messages), None);
    }

    #[test]
    fn parse_json_output() {
        let json = r#"{
            "session_id": "abc",
            "result": "Hello! How can I help?",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 25
            },
            "cost_usd": 0.003,
            "is_error": false
        }"#;
        let parsed: ClaudeCodeJsonOutput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("Hello! How can I help?"));
        assert_eq!(parsed.usage.as_ref().unwrap().input_tokens, Some(100));
        assert_eq!(parsed.usage.as_ref().unwrap().output_tokens, Some(25));
        assert_eq!(parsed.is_error, Some(false));
    }

    #[test]
    fn parse_json_output_minimal() {
        let json = r#"{"result": "ok"}"#;
        let parsed: ClaudeCodeJsonOutput = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("ok"));
        assert!(parsed.usage.is_none());
        assert!(parsed.is_error.is_none());
    }

    #[tokio::test]
    async fn missing_binary_returns_unavailable() {
        let provider =
            ClaudeCodeCompletionProvider::new("nonexistent-claude-binary-xyz".into(), None);
        let request = CompletionRequest {
            messages: vec![ChatMessage {
                role: Role::User,
                content: "hello".into(),
            }],
            max_tokens: None,
            temperature: None,
            stop: None,
            model_hint: crate::types::ModelHint::Default,
        };
        let err = provider.complete(&request).await.unwrap_err();
        assert!(matches!(err, ProviderError::Unavailable(_)));
    }
}
