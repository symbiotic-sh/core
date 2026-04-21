//! OpenAI Codex CLI completion provider.
//!
//! Uses the `codex` CLI in exec mode (`codex exec --json`) as a completion
//! backend. Codex CLI is Apache 2.0 open source and explicitly supports
//! headless/automated usage in daemons and CI/CD pipelines.
//!
//! Shell tools and web search are disabled so the CLI acts as a pure
//! completion provider for the daemon's internal ReAct loop.

use async_trait::async_trait;
use serde::Deserialize;
use std::process::Stdio;
use tokio::process::Command;

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, ModelProvider,
    PricingInfo, ProviderCapability, ProviderClass, ProviderError, Role,
};

/// Completion provider that shells out to the `codex` CLI.
///
/// Spawns `codex exec --json --ephemeral` per completion request, disabling
/// shell tools and web search so it acts as a pure LLM completion backend.
///
/// Authentication: `CODEX_API_KEY` env var or prior `codex login`.
pub struct CodexCompletionProvider {
    /// Path to the `codex` binary (default: "codex").
    cli_command: String,
    /// Model override (e.g. "o3", "gpt-4.1"). None = CLI default.
    model: Option<String>,
    capabilities: CapabilitySet,
    pricing: PricingInfo,
}

impl CodexCompletionProvider {
    /// Create a new provider.
    ///
    /// # Arguments
    /// * `cli_command` — Path to `codex` binary (or just `"codex"` for PATH lookup).
    /// * `model` — Optional model override. `None` uses the CLI's default.
    pub fn new(cli_command: String, model: Option<String>) -> Self {
        Self {
            cli_command,
            model,
            capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
            pricing: PricingInfo {
                // Subscription-based pricing — varies by plan.
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

impl ModelProvider for CodexCompletionProvider {
    fn name(&self) -> &str {
        "codex"
    }

    fn provider_class(&self) -> ProviderClass {
        ProviderClass::Cloud
    }

    fn model_name(&self) -> &str {
        match &self.model {
            Some(m) => m.as_str(),
            None => "codex-default",
        }
    }

    fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        Some(&self.pricing)
    }
}

// -- Codex JSONL output types ------------------------------------------------

/// A single event in the Codex CLI JSONL output stream.
#[derive(Deserialize)]
struct CodexEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// Present on `item.*` events.
    item: Option<CodexItem>,
    /// Present on `turn.completed` events.
    usage: Option<CodexUsage>,
    /// Present on `error` events.
    #[allow(dead_code)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct CodexItem {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    item_type: Option<String>,
    /// The text content (for `agent_message` items).
    text: Option<String>,
}

#[derive(Deserialize)]
struct CodexUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

// -- Helpers -----------------------------------------------------------------

/// Build the user prompt from messages, prepending system messages.
///
/// Codex CLI doesn't have a `--system-prompt` flag (as of 2026), so we
/// prepend system instructions to the user prompt.
fn build_prompt(messages: &[crate::ChatMessage]) -> String {
    let mut parts = Vec::new();

    // System messages first
    for msg in messages.iter().filter(|m| m.role == Role::System) {
        parts.push(msg.content.clone());
    }

    // Then user/assistant messages
    let non_system: Vec<_> = messages.iter().filter(|m| m.role != Role::System).collect();

    if non_system.len() == 1 && parts.is_empty() {
        return non_system[0].content.clone();
    }

    for msg in non_system {
        let label = match msg.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => unreachable!(),
        };
        parts.push(format!("{label}: {}", msg.content));
    }

    parts.join("\n\n")
}

/// Parse JSONL output from `codex exec --json`.
///
/// Extracts the final agent message text and usage info from the event stream.
fn parse_jsonl_output(output: &str) -> (String, Option<u64>, Option<u64>) {
    let mut text = String::new();
    let mut input_tokens = None;
    let mut output_tokens = None;

    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(event) = serde_json::from_str::<CodexEvent>(line) {
            // Collect agent message text from completed items
            if event.event_type == "item.completed" {
                if let Some(ref item) = event.item {
                    if let Some(ref t) = item.text {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                }
            }
            // Collect usage from turn.completed
            if event.event_type == "turn.completed" {
                if let Some(ref usage) = event.usage {
                    input_tokens = usage.input_tokens;
                    output_tokens = usage.output_tokens;
                }
            }
        }
    }

    (text, input_tokens, output_tokens)
}

// -- CompletionProvider impl -------------------------------------------------

#[async_trait]
impl CompletionProvider for CodexCompletionProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let mut cmd = Command::new(&self.cli_command);

        cmd.arg("exec");
        cmd.arg("--json"); // JSONL output
        cmd.arg("--ephemeral"); // Don't persist session files

        // Disable tools — we only want raw completion for our ReAct loop
        cmd.args(["-c", "features.shell_tool=false"]);
        cmd.args(["-c", "features.web_search=false"]);

        // Model override
        if let Some(ref model) = self.model {
            cmd.args(["--model", model]);
        }

        // User prompt (with system messages prepended)
        let prompt = build_prompt(&request.messages);
        cmd.arg(&prompt);

        // No stdin, capture stdout/stderr
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let output = cmd.output().await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable(format!(
                    "codex CLI not found at '{}'. Install: npm i -g @openai/codex",
                    self.cli_command
                ))
            } else {
                ProviderError::RequestFailed(format!("failed to spawn codex CLI: {e}"))
            }
        })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(ProviderError::RequestFailed(format!(
                "codex CLI exited with {}: {}{}",
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
        let (content, input_tokens, output_tokens) = parse_jsonl_output(&stdout);

        if content.is_empty() {
            return Err(ProviderError::InvalidResponse(
                "codex CLI returned no agent_message in JSONL output".to_string(),
            ));
        }

        Ok(CompletionResponse {
            content,
            model: self.model.clone().unwrap_or_else(|| "codex".to_string()),
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
        let provider = CodexCompletionProvider::new("codex".into(), None);
        assert_eq!(provider.name(), "codex");
        assert_eq!(provider.provider_class(), ProviderClass::Cloud);
        assert_eq!(provider.model_name(), "codex-default");
        assert!(provider.capabilities().has(ProviderCapability::Completion));
    }

    #[test]
    fn provider_metadata_with_model() {
        let provider = CodexCompletionProvider::new("codex".into(), Some("o3".into()));
        assert_eq!(provider.model_name(), "o3");
    }

    #[test]
    fn build_prompt_single_user_message() {
        let messages = vec![ChatMessage {
            role: Role::User,
            content: "Hello".into(),
        }];
        assert_eq!(build_prompt(&messages), "Hello");
    }

    #[test]
    fn build_prompt_with_system() {
        let messages = vec![
            ChatMessage {
                role: Role::System,
                content: "Be concise.".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "Hello".into(),
            },
        ];
        let prompt = build_prompt(&messages);
        assert!(prompt.contains("Be concise."));
        assert!(prompt.contains("User: Hello"));
    }

    #[test]
    fn build_prompt_multi_turn() {
        let messages = vec![
            ChatMessage {
                role: Role::User,
                content: "Hi".into(),
            },
            ChatMessage {
                role: Role::Assistant,
                content: "Hello!".into(),
            },
            ChatMessage {
                role: Role::User,
                content: "How are you?".into(),
            },
        ];
        let prompt = build_prompt(&messages);
        assert!(prompt.contains("User: Hi"));
        assert!(prompt.contains("Assistant: Hello!"));
        assert!(prompt.contains("User: How are you?"));
    }

    #[test]
    fn parse_jsonl_agent_message() {
        let jsonl = r#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.started"}
{"type":"item.started","item":{"id":"msg1","type":"agent_message"}}
{"type":"item.completed","item":{"id":"msg1","type":"agent_message","text":"Hello, world!"}}
{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":25}}"#;

        let (text, input, output) = parse_jsonl_output(jsonl);
        assert_eq!(text, "Hello, world!");
        assert_eq!(input, Some(100));
        assert_eq!(output, Some(25));
    }

    #[test]
    fn parse_jsonl_multiple_messages() {
        let jsonl = r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"Part 1"}}
{"type":"item.completed","item":{"id":"m2","type":"agent_message","text":"Part 2"}}
{"type":"turn.completed","usage":{"input_tokens":50,"output_tokens":10}}"#;

        let (text, input, output) = parse_jsonl_output(jsonl);
        assert_eq!(text, "Part 1\nPart 2");
        assert_eq!(input, Some(50));
        assert_eq!(output, Some(10));
    }

    #[test]
    fn parse_jsonl_no_message() {
        let jsonl = r#"{"type":"thread.started","thread_id":"abc"}
{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":0}}"#;

        let (text, _, _) = parse_jsonl_output(jsonl);
        assert!(text.is_empty());
    }

    #[test]
    fn parse_jsonl_no_usage() {
        let jsonl =
            r#"{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"ok"}}"#;

        let (text, input, output) = parse_jsonl_output(jsonl);
        assert_eq!(text, "ok");
        assert_eq!(input, None);
        assert_eq!(output, None);
    }

    #[test]
    fn parse_jsonl_skips_non_message_items() {
        let jsonl = r#"{"type":"item.completed","item":{"id":"cmd1","type":"command_execution","command":"ls"}}
{"type":"item.completed","item":{"id":"m1","type":"agent_message","text":"Done"}}
{"type":"turn.completed","usage":{"input_tokens":200,"output_tokens":50}}"#;

        let (text, _, _) = parse_jsonl_output(jsonl);
        assert_eq!(text, "Done");
    }

    #[tokio::test]
    async fn missing_binary_returns_unavailable() {
        let provider = CodexCompletionProvider::new("nonexistent-codex-binary-xyz".into(), None);
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
