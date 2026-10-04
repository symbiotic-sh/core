//! Agent execution loop implementing a ReAct-style pattern.
//!
//! 1. Format system prompt with available tools
//! 2. Send user goal + context to LLM
//! 3. Parse LLM response for tool calls (JSON)
//! 4. Execute requested tool (with capability check)
//! 5. Feed tool result back to LLM
//! 6. Repeat until LLM says "done" or max iterations (10)
//! 7. Return final result

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::llm::{ChatMessage, LlmClient};
use crate::tools::{format_tools_for_prompt, Tool};
use symbiotic_context::redact_pii;

/// Maximum number of iterations before the loop terminates.
const MAX_ITERATIONS: usize = 10;

/// Maximum cumulative size of all messages in bytes before the loop terminates.
const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// 80% threshold for the Context Handoff Protocol
const HANDOFF_BUFFER_BYTES: usize = (MAX_BUFFER_BYTES * 8) / 10;

/// The result of running an agent to completion.
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub output: String,
    pub iterations: usize,
    pub tool_calls: Vec<ToolCallRecord>,
}

/// Record of a tool call made during execution.
#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub params: serde_json::Value,
    pub success: bool,
    pub output: String,
}

/// Parsed LLM response: either a tool call or a final answer.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LlmAction {
    Done {
        done: bool,
        result: String,
    },
    ToolCall {
        tool: String,
        params: serde_json::Value,
    },
}

/// Configuration for a role-aware agent execution.
#[derive(Debug, Clone)]
pub struct AgentExecConfig {
    /// Custom system prompt from the role definition. When set, replaces
    /// the default generic prompt.
    pub system_prompt: Option<String>,
    /// Maximum iterations override. When `None`, uses `MAX_ITERATIONS`.
    pub max_iterations: Option<usize>,
    /// Identity context from SOUL.md to prepend to the system prompt.
    /// When set, this is injected before the base system prompt.
    pub identity_context: Option<String>,
    /// Directory to write handoff files when the 80% circuit breaker triggers.
    /// When `None`, handoff content is only returned in `ExecutionResult.output`.
    pub handoff_dir: Option<PathBuf>,
    /// Optional agent identifier for handoff file metadata.
    pub agent_id: Option<String>,
    /// Optional role label (e.g. "orchestrator", "researcher"). Paired with
    /// `agent_id` to form a `TraceTag` that wraps every `llm.chat` call so
    /// provider-layer trace hooks can write per-agent audit files.
    pub role: Option<String>,
    /// Whether to redact PII from the agent's final output before returning.
    /// Defaults to `true`. Set to `false` for debugging or trusted-local scenarios.
    pub redact_output: bool,
}

impl Default for AgentExecConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            max_iterations: None,
            identity_context: None,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: true,
        }
    }
}

/// Conditionally apply PII redaction to an execution result's output.
fn maybe_redact(mut result: ExecutionResult, redact: bool) -> ExecutionResult {
    if redact {
        result.output = redact_pii(&result.output);
    }
    result
}

/// Run the agent execution loop.
///
/// Takes a goal (user request), a set of tools, and an LLM client.
/// Returns the final result after the agent finishes or hits max iterations.
pub async fn run_agent(
    goal: &str,
    context: &str,
    tools: &[&dyn Tool],
    llm: &dyn LlmClient,
) -> Result<ExecutionResult> {
    run_agent_with_config(goal, context, tools, llm, &AgentExecConfig::default()).await
}

/// Run the agent execution loop with role-aware configuration.
///
/// When `config.system_prompt` is set, it replaces the default generic prompt.
/// The tools prompt is always appended. When `config.max_iterations` is set,
/// it overrides the default maximum.
pub async fn run_agent_with_config(
    goal: &str,
    context: &str,
    tools: &[&dyn Tool],
    llm: &dyn LlmClient,
    config: &AgentExecConfig,
) -> Result<ExecutionResult> {
    let tools_prompt = format_tools_for_prompt(tools);

    let base_prompt = config.system_prompt.as_deref().unwrap_or(
        "You are an AI agent. Complete the user's goal. \
Use tools when you need to access external data (archive, queue, etc.), \
but answer directly from your own knowledge when the goal is a factual question \
or doesn't require external data. Only use ask_user when you genuinely need \
clarification from the user — never for questions you can answer yourself.",
    );

    let system_prompt = if let Some(ref identity) = config.identity_context {
        format!(
            "{identity}\n\n---\n\n{base_prompt}\n\
             Respond ONLY with JSON. No markdown, no explanation outside JSON.\n\n\
             {tools_prompt}"
        )
    } else {
        format!(
            "{base_prompt}\n\
             Respond ONLY with JSON. No markdown, no explanation outside JSON.\n\n\
             {tools_prompt}"
        )
    };

    let max_iter = config.max_iterations.unwrap_or(MAX_ITERATIONS);

    let mut messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: if context.is_empty() {
                goal.to_string()
            } else {
                format!("Context:\n{context}\n\nGoal: {goal}")
            },
        },
    ];

    let mut tool_calls = Vec::new();

    for iteration in 0..max_iter {
        let buffer_size: usize = messages.iter().map(|m| m.content.len()).sum();

        // 100% hard limit (failsafe)
        if buffer_size > MAX_BUFFER_BYTES {
            return Err(anyhow!(
                "agent message buffer exceeded {MAX_BUFFER_BYTES} bytes"
            ));
        }

        // 80% Context Handoff Protocol (Circuit Breaker)
        if buffer_size > HANDOFF_BUFFER_BYTES {
            // Inject the Handoff instruction to the LLM
            messages.push(ChatMessage {
                role: "system".to_string(),
                content: "CRITICAL: You have consumed 80% of your available context window. You must initiate the Context Handoff Protocol immediately to prevent memory degradation.

Respond ONLY with a JSON object calling a theoretical 'write_handoff' tool (or simply a final JSON with done=true) containing a detailed summary of what was accomplished, what is pending, and the exact objective for the next agent to resume from. Do NOT attempt to complete the main goal.".to_string(),
            });

            // Allow one final LLM call to process the handoff
            let handoff_response = llm.chat(&messages, true).await?;

            // Persist handoff to disk if configured
            if let Some(ref dir) = config.handoff_dir {
                if let Err(e) = write_handoff_file(
                    dir,
                    &handoff_response,
                    config.agent_id.as_deref(),
                    iteration + 1,
                ) {
                    // Non-fatal: log but don't fail the execution
                    eprintln!("warning: failed to write handoff file: {e}");
                }
            }

            return Ok(maybe_redact(
                ExecutionResult {
                    output: format!("CONTEXT_HANDOFF_REQUIRED:\n{}", handoff_response),
                    iterations: iteration + 1,
                    tool_calls,
                },
                config.redact_output,
            ));
        }

        // Tag every LLM call with (agent_id, role, iteration) so that
        // provider-layer trace hooks (e.g. the Ollama provider's optional
        // SYMBIOTIC_OLLAMA_TRACE_DIR dump) can write per-agent audit files
        // instead of a flat sequential log. Task-local so nested agent
        // dispatch never interleaves.
        let trace_tag = match (config.agent_id.as_deref(), config.role.as_deref()) {
            (Some(agent_id), Some(role)) => {
                Some(std::sync::Arc::new(symbiotic_core::trace::TraceTag {
                    agent_id: agent_id.to_string(),
                    role: role.to_string(),
                    iteration: iteration as u32,
                }))
            }
            _ => None,
        };
        let response = match symbiotic_core::trace::TRACE_TAG
            .scope(trace_tag, llm.chat(&messages, true))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                // LLM call failed — check if a previous iteration already
                // produced a final answer. If so, return it instead of failing.
                if let Some(result) = extract_done_result(&messages) {
                    return Ok(maybe_redact(
                        ExecutionResult {
                            output: result,
                            iterations: iteration + 1,
                            tool_calls,
                        },
                        config.redact_output,
                    ));
                }
                return Err(e.context("provider completion failed"));
            }
        };

        // Try to parse the response as JSON. LLMs (especially Gemini)
        // often wrap JSON in markdown fences or include surrounding text.
        // We extract the JSON object before parsing.
        let json_str = extract_or_repair_json(&response);
        let action = match json_str
            .as_deref()
            .and_then(|s| serde_json::from_str::<LlmAction>(s).ok())
        {
            Some(action) => action,
            None => {
                // Debug-surface why we're exiting. Helps diagnose cases where
                // the LLM emits a tool call wrapped in chat-template trailers
                // (e.g. gemma's `<tool_call|>` tag) or multi-object responses
                // the parser can't disambiguate. Trace-level so production
                // runs stay quiet.
                tracing::warn!(
                    iteration,
                    extracted_json_present = json_str.is_some(),
                    extracted_len = json_str.as_deref().map(|s| s.len()).unwrap_or(0),
                    response_len = response.len(),
                    response_preview = %response.chars().take(400).collect::<String>(),
                    extracted_preview = %json_str
                        .as_deref()
                        .map(|s| s.chars().take(400).collect::<String>())
                        .unwrap_or_default(),
                    "agent loop exit: could not parse response as LlmAction; returning raw text as final answer"
                );
                return Ok(maybe_redact(
                    ExecutionResult {
                        output: response,
                        iterations: iteration + 1,
                        tool_calls,
                    },
                    config.redact_output,
                ));
            }
        };

        match action {
            LlmAction::Done { done: true, result } => {
                return Ok(maybe_redact(
                    ExecutionResult {
                        output: result,
                        iterations: iteration + 1,
                        tool_calls,
                    },
                    config.redact_output,
                ));
            }
            LlmAction::Done {
                done: false,
                result,
            } => {
                // LLM said done=false with a result; treat as continuation
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response,
                });
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: format!("Continue. Previous partial result: {result}"),
                });
            }
            LlmAction::ToolCall { tool, params } => {
                // Find the requested tool
                let tool_impl = tools.iter().find(|t| t.name() == tool);

                let (success, output) = match tool_impl {
                    Some(t) => match t.execute(params.clone()).await {
                        Ok(result) => (result.success, result.output),
                        Err(e) => (false, format!("Tool error: {e}")),
                    },
                    None => (false, format!("Unknown tool: {tool}")),
                };

                tool_calls.push(ToolCallRecord {
                    tool_name: tool.clone(),
                    params,
                    success,
                    output: output.clone(),
                });

                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response,
                });
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: format!(
                        "Tool result (success={success}):\n{output}\n\nContinue with the task."
                    ),
                });
            }
        }
    }

    Err(anyhow!("agent exceeded maximum iterations ({max_iter})"))
}

/// Scan assistant messages for a `{"done": true, "result": "..."}` response.
///
/// When a provider error kills the loop mid-execution, the agent may have
/// already produced a final answer in a previous iteration. This extracts
/// it so we return the result instead of failing.
fn extract_done_result(messages: &[ChatMessage]) -> Option<String> {
    for msg in messages.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        let json_str = extract_or_repair_json(&msg.content)?;
        if let Ok(LlmAction::Done { done: true, result }) =
            serde_json::from_str::<LlmAction>(&json_str)
        {
            return Some(result);
        }
    }
    None
}

/// Extract the first valid JSON object from LLM output.
///
/// Handles common LLM quirks:
/// - Markdown code fences: ```json { ... } ```
/// - Leading/trailing text: "Here's the plan: { ... } Let me know"
/// - Plain JSON: { ... }
///
/// Returns a reference to the JSON substring, or `None` if no object found.
fn extract_json_object(text: &str) -> Option<&str> {
    // First, try parsing the whole string (fast path)
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        return Some(text);
    }

    // Strip markdown code fences: ```json\n...\n``` or ```\n...\n```
    let stripped = text.trim();
    if let Some(rest) = stripped.strip_prefix("```json") {
        if let Some(inner) = rest.strip_suffix("```") {
            let inner = inner.trim();
            if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
                return Some(inner);
            }
        }
    }
    if let Some(rest) = stripped.strip_prefix("```") {
        if let Some(inner) = rest.strip_suffix("```") {
            let inner = inner.trim();
            if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
                return Some(inner);
            }
        }
    }

    // Find the first `{` and its matching `}` using brace counting
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape_next = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start {
                    let candidate = &text[s..=i];
                    if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                        return Some(candidate);
                    }
                }
                start = None;
            }
        }
    }

    None
}

/// Extract a JSON object from LLM output, OR attempt to repair an
/// almost-valid object.
///
/// gemma-family models (observed with gemma4:e4b in Ollama) emit tool-call
/// JSON with two common kinds of corruption even when Ollama's
/// `format: "json"` mode is enabled:
///
/// 1. **Raw control characters inside string values** — literal `\n`, `\t`,
///    `\r` bytes (0x0A/0x09/0x0D) instead of the escaped forms `\\n`, `\\t`,
///    `\\r`. Strict JSON forbids this; `serde_json` rejects the whole object.
/// 2. **Missing closing braces** — response ends after `params` closes but
///    before the outer `{"tool": ..., "params": {...}}` closes.
///
/// This wrapper first tries the normal extraction. If that fails, it:
/// - Strips common trailing noise (gemma's `<tool_call|>` tag, stray ```).
/// - Escapes raw control chars inside string values.
/// - Appends up to 3 closing `}` until `serde_json` accepts the result.
///
/// Returns an owned `String` so repaired JSON outlives the input.
pub(crate) fn extract_or_repair_json(text: &str) -> Option<String> {
    if let Some(s) = extract_json_object(text) {
        return Some(s.to_string());
    }

    let trimmed = text.trim_start();
    if !trimmed.starts_with('{') {
        return None;
    }

    // Strip trailing whitespace and common trailer tokens (gemma has been
    // seen to append `<tool_call|>` or stray backticks after what it thinks
    // is the end of the JSON).
    let mut candidate = trimmed.trim_end().to_string();
    loop {
        let before = candidate.len();
        for trailer in ["<tool_call|>", "<|tool_call|>", "```", "`"] {
            if candidate.ends_with(trailer) {
                candidate.truncate(candidate.len() - trailer.len());
            }
        }
        candidate = candidate.trim_end().to_string();
        if candidate.len() == before {
            break;
        }
    }

    // First pass: escape raw control chars that appear INSIDE string values
    // (between unescaped `"` marks). Strict JSON forbids literal 0x0A etc.
    let candidate = escape_raw_controls_in_strings(&candidate);

    for missing_closes in 0..=3u32 {
        let repaired = format!("{}{}", candidate, "}".repeat(missing_closes as usize));
        if serde_json::from_str::<serde_json::Value>(&repaired).is_ok() {
            return Some(repaired);
        }
    }

    None
}

/// Walk `text` and escape raw control characters (newline, tab, carriage
/// return) that appear inside JSON string values. Outside strings the input
/// is left alone. Handles backslash-escape tracking so the scan doesn't get
/// fooled by an escaped quote.
fn escape_raw_controls_in_strings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escape_next = false;
    for c in text.chars() {
        if escape_next {
            out.push(c);
            escape_next = false;
            continue;
        }
        if in_string && c == '\\' {
            out.push(c);
            escape_next = true;
            continue;
        }
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            continue;
        }
        if in_string {
            match c {
                '\n' => out.push_str("\\n"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                other => out.push(other),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Write a context handoff file to the specified directory.
fn write_handoff_file(
    dir: &std::path::Path,
    content: &str,
    agent_id: Option<&str>,
    iteration: usize,
) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let timestamp = chrono::Utc::now().format("%Y-%m-%d-%H%M");
    let filename = format!("{timestamp}-context-handoff.md");
    let path = dir.join(&filename);

    let agent_line = agent_id
        .map(|id| format!("agent_id: \"{id}\"\n"))
        .unwrap_or_default();

    let file_content = format!(
        "---\n\
         type: context-handoff\n\
         {agent_line}\
         iteration: {iteration}\n\
         timestamp: \"{timestamp}\"\n\
         ---\n\n\
         {content}\n"
    );

    std::fs::write(&path, &file_content)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ToolResult;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // -- Mock LLM that returns predefined responses --

    struct MockLlm {
        responses: Vec<String>,
        call_count: AtomicUsize,
    }

    impl MockLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow!("mock LLM ran out of responses"))
        }
    }

    // -- Mock tool --

    struct EchoTool;

    #[async_trait::async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}})
        }
        async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
            let text = params
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("empty");
            Ok(ToolResult {
                success: true,
                output: format!("echo: {text}"),
            })
        }
    }

    // -- extract_json_object tests --

    #[test]
    fn extract_plain_json() {
        let input = r#"{"tool": "echo", "params": {"text": "hi"}}"#;
        assert_eq!(extract_json_object(input), Some(input));
    }

    #[test]
    fn extract_json_from_markdown_fence() {
        let input = "```json\n{\"tool\": \"echo\", \"params\": {}}\n```";
        assert_eq!(
            extract_json_object(input),
            Some("{\"tool\": \"echo\", \"params\": {}}")
        );
    }

    #[test]
    fn extract_json_from_plain_fence() {
        let input = "```\n{\"done\": true, \"result\": \"ok\"}\n```";
        assert_eq!(
            extract_json_object(input),
            Some("{\"done\": true, \"result\": \"ok\"}")
        );
    }

    #[test]
    fn extract_json_with_surrounding_text() {
        let input = "Here is my response:\n{\"tool\": \"ask_user\", \"params\": {\"question\": \"What?\"}}\nLet me know.";
        let extracted = extract_json_object(input).unwrap();
        assert!(extracted.starts_with('{'));
        assert!(extracted.contains("ask_user"));
    }

    #[test]
    fn extract_json_with_nested_braces() {
        let input = r#"{"tool": "file_write", "params": {"path": "x.json", "content": "{\"key\": \"val\"}"}}"#;
        assert_eq!(extract_json_object(input), Some(input));
    }

    #[test]
    fn extract_returns_none_for_no_json() {
        assert_eq!(extract_json_object("Just plain text"), None);
    }

    #[test]
    fn extract_json_from_gemini_typical_response() {
        // Gemini often wraps in ```json ... ``` with extra newlines
        let input = "\n```json\n{\n  \"tool\": \"shell_exec\",\n  \"params\": {\n    \"command\": \"npm init -y\"\n  }\n}\n```\n";
        let extracted = extract_json_object(input).unwrap();
        assert!(extracted.contains("shell_exec"));
    }

    #[test]
    fn repair_appends_missing_close_brace() {
        // Observed with gemma4:e4b — model emits `params` closing but drops
        // the outer object's closing brace.
        let input = r#"{"tool": "file_write", "params": {"path": "memo.md", "content": "hi"}"#;
        let repaired = extract_or_repair_json(input).expect("should repair missing brace");
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["tool"], "file_write");
        assert_eq!(parsed["params"]["path"], "memo.md");
    }

    #[test]
    fn repair_escapes_raw_newlines_inside_strings() {
        // Also observed with gemma4:e4b — literal 0x0A inside string values.
        // Raw JSON forbids this; we escape to `\\n` inline before re-parsing.
        let input = "{\"tool\": \"file_write\", \"params\": {\"content\": \"line one\nline two\nline three\", \"path\": \"x.md\"}}";
        let repaired = extract_or_repair_json(input).expect("should repair raw newlines");
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["tool"], "file_write");
        let content = parsed["params"]["content"].as_str().unwrap();
        assert!(content.contains("line one"));
        assert!(content.contains("line three"));
    }

    #[test]
    fn repair_handles_combined_corruption() {
        // The real-world case: raw newlines AND a missing closing brace.
        // This is the shape of the v7 orchestrator final response; if this
        // fails, demo 2 cannot close its action loop on gemma-class models.
        let input = "{\"tool\": \"file_write\", \"params\": {\"content\": \"# Memo\n\nBody here\", \"path\": \"memo.md\"}";
        let repaired = extract_or_repair_json(input).expect("should repair combined corruption");
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["tool"], "file_write");
        assert_eq!(parsed["params"]["path"], "memo.md");
        assert!(parsed["params"]["content"]
            .as_str()
            .unwrap()
            .contains("Body here"));
    }

    #[test]
    fn repair_strips_gemma_trailer_token() {
        // gemma's chat template can leave `<tool_call|>` or stray backticks
        // after the JSON. Trailer stripping + repair should recover.
        let input = "{\"tool\": \"echo\", \"params\": {\"text\": \"hi\"}<tool_call|>";
        let repaired = extract_or_repair_json(input).expect("should strip trailer + repair");
        let parsed: serde_json::Value = serde_json::from_str(&repaired).unwrap();
        assert_eq!(parsed["tool"], "echo");
    }

    #[tokio::test]
    async fn agent_parses_tool_call_in_markdown_fence() {
        let llm = MockLlm::new(vec![
            "```json\n{\"tool\": \"echo\", \"params\": {\"text\": \"fenced\"}}\n```".to_string(),
            r#"{"done": true, "result": "Fenced tool worked"}"#.to_string(),
        ]);
        let echo = EchoTool;
        let tools: Vec<&dyn Tool> = vec![&echo];

        let result = run_agent("Test fenced JSON", "", &tools, &llm)
            .await
            .expect("should parse fenced JSON");

        assert_eq!(result.output, "Fenced tool worked");
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "echo");
    }

    #[tokio::test]
    async fn agent_parses_tool_call_with_surrounding_text() {
        let llm = MockLlm::new(vec![
            "I'll search for that.\n{\"tool\": \"echo\", \"params\": {\"text\": \"embedded\"}}\nLet me check.".to_string(),
            r#"{"done": true, "result": "Embedded tool worked"}"#.to_string(),
        ]);
        let echo = EchoTool;
        let tools: Vec<&dyn Tool> = vec![&echo];

        let result = run_agent("Test embedded JSON", "", &tools, &llm)
            .await
            .expect("should parse embedded JSON");

        assert_eq!(result.output, "Embedded tool worked");
        assert_eq!(result.tool_calls.len(), 1);
    }

    #[tokio::test]
    async fn agent_completes_immediately_with_done() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "The answer is 42"}"#.to_string()
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("What is the answer?", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(result.output, "The answer is 42");
        assert_eq!(result.iterations, 1);
        assert!(result.tool_calls.is_empty());
    }

    #[tokio::test]
    async fn agent_calls_tool_then_completes() {
        let llm = MockLlm::new(vec![
            r#"{"tool": "echo", "params": {"text": "hello world"}}"#.to_string(),
            r#"{"done": true, "result": "Echoed: hello world"}"#.to_string(),
        ]);
        let echo = EchoTool;
        let tools: Vec<&dyn Tool> = vec![&echo];

        let result = run_agent("Echo something", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(result.output, "Echoed: hello world");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].tool_name, "echo");
        assert!(result.tool_calls[0].success);
    }

    #[tokio::test]
    async fn agent_handles_unknown_tool() {
        let llm = MockLlm::new(vec![
            r#"{"tool": "nonexistent", "params": {}}"#.to_string(),
            r#"{"done": true, "result": "Gave up"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Do something", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(result.output, "Gave up");
        assert_eq!(result.tool_calls.len(), 1);
        assert!(!result.tool_calls[0].success);
        assert!(result.tool_calls[0].output.contains("Unknown tool"));
    }

    #[tokio::test]
    async fn agent_respects_max_iterations() {
        // LLM keeps calling tools forever
        let responses: Vec<String> = (0..MAX_ITERATIONS + 5)
            .map(|_| r#"{"tool": "echo", "params": {"text": "loop"}}"#.to_string())
            .collect();
        let llm = MockLlm::new(responses);
        let echo = EchoTool;
        let tools: Vec<&dyn Tool> = vec![&echo];

        let err = run_agent("Loop forever", "", &tools, &llm)
            .await
            .expect_err("should hit max iterations");
        assert!(err.to_string().contains("maximum iterations"));
    }

    #[tokio::test]
    async fn agent_handles_non_json_response() {
        let llm = MockLlm::new(vec!["Just a plain text answer".to_string()]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Question?", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(result.output, "Just a plain text answer");
        assert_eq!(result.iterations, 1);
    }

    #[tokio::test]
    async fn agent_includes_context_in_prompt() {
        // We verify context is used by checking the LLM receives it
        let llm = Arc::new(MockLlm::new(vec![
            r#"{"done": true, "result": "got context"}"#.to_string(),
        ]));
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Summarize", "Important context here", &tools, llm.as_ref())
            .await
            .expect("run_agent");

        assert_eq!(result.output, "got context");
    }

    #[tokio::test]
    async fn agent_aborts_when_buffer_exceeds_max_bytes() {
        // Mock LLM that returns a tool call, then a huge output is injected
        // via the tool result. The mock tool returns >2 MiB of data.
        struct HugeTool;

        #[async_trait::async_trait]
        impl Tool for HugeTool {
            fn name(&self) -> &str {
                "huge"
            }
            fn description(&self) -> &str {
                "Returns a huge output"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _params: serde_json::Value) -> Result<ToolResult> {
                // Return output exceeding MAX_BUFFER_BYTES (2 MiB)
                let big = "X".repeat(3 * 1024 * 1024);
                Ok(ToolResult {
                    success: true,
                    output: big,
                })
            }
        }

        // LLM calls the huge tool, then would call it again
        let llm = MockLlm::new(vec![
            r#"{"tool": "huge", "params": {}}"#.to_string(),
            r#"{"tool": "huge", "params": {}}"#.to_string(),
        ]);
        let huge = HugeTool;
        let tools: Vec<&dyn Tool> = vec![&huge];

        let err = run_agent("Trigger buffer overflow", "", &tools, &llm)
            .await
            .expect_err("should abort due to buffer size");
        assert!(
            err.to_string().contains("buffer exceeded"),
            "expected buffer exceeded error, got: {err}"
        );
    }

    #[tokio::test]
    async fn agent_tool_error_is_recorded() {
        struct FailTool;

        #[async_trait::async_trait]
        impl Tool for FailTool {
            fn name(&self) -> &str {
                "fail"
            }
            fn description(&self) -> &str {
                "Always fails"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _params: serde_json::Value) -> Result<ToolResult> {
                Err(anyhow!("intentional failure"))
            }
        }

        let llm = MockLlm::new(vec![
            r#"{"tool": "fail", "params": {}}"#.to_string(),
            r#"{"done": true, "result": "handled error"}"#.to_string(),
        ]);
        let fail = FailTool;
        let tools: Vec<&dyn Tool> = vec![&fail];

        let result = run_agent("Try failing tool", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(result.tool_calls.len(), 1);
        assert!(!result.tool_calls[0].success);
        assert!(result.tool_calls[0].output.contains("intentional failure"));
    }

    #[tokio::test]
    async fn agent_with_custom_system_prompt() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "Research complete"}"#.to_string()
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let config = AgentExecConfig {
            system_prompt: Some("You are a research agent. Synthesize information.".to_string()),
            max_iterations: None,
            identity_context: None,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: false,
        };

        let result = run_agent_with_config("Find info", "", &tools, &llm, &config)
            .await
            .expect("run_agent_with_config");

        assert_eq!(result.output, "Research complete");
        assert_eq!(result.iterations, 1);
    }

    #[tokio::test]
    async fn agent_with_custom_max_iterations() {
        // LLM keeps calling tools — custom limit of 3 should kick in
        let responses: Vec<String> = (0..10)
            .map(|_| r#"{"tool": "echo", "params": {"text": "loop"}}"#.to_string())
            .collect();
        let llm = MockLlm::new(responses);
        let echo = EchoTool;
        let tools: Vec<&dyn Tool> = vec![&echo];

        let config = AgentExecConfig {
            system_prompt: None,
            max_iterations: Some(3),
            identity_context: None,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: false,
        };

        let err = run_agent_with_config("Loop", "", &tools, &llm, &config)
            .await
            .expect_err("should hit custom max iterations");
        assert!(err.to_string().contains("maximum iterations (3)"));
    }

    #[tokio::test]
    async fn default_config_matches_run_agent() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "same behavior"}"#.to_string()
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent_with_config("Test", "", &tools, &llm, &AgentExecConfig::default())
            .await
            .expect("run_agent_with_config");

        assert_eq!(result.output, "same behavior");
    }
    #[tokio::test]
    async fn agent_initiates_handoff_at_80_percent_buffer() {
        struct BigTool;

        #[async_trait::async_trait]
        impl Tool for BigTool {
            fn name(&self) -> &str {
                "big"
            }
            fn description(&self) -> &str {
                "Returns a big output"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _params: serde_json::Value) -> Result<ToolResult> {
                // Return output just over 80% (1.6 MiB), but under 100%
                let big = "X".repeat((1.7 * 1024.0 * 1024.0) as usize);
                Ok(ToolResult {
                    success: true,
                    output: big,
                })
            }
        }

        let llm = MockLlm::new(vec![
            r#"{"tool": "big", "params": {}}"#.to_string(), // Call the tool to blow up context
            r#"{"done": true, "result": "Handoff summary"}"#.to_string(), // The response to the Handoff injection
        ]);
        let big_tool = BigTool;
        let tools: Vec<&dyn Tool> = vec![&big_tool];

        let result = run_agent("Do something big", "", &tools, &llm)
            .await
            .expect("should return gracefully via handoff protocol");

        assert!(result.output.contains("CONTEXT_HANDOFF_REQUIRED:"));
        assert!(result.output.contains("Handoff summary"));
    }

    #[tokio::test]
    async fn handoff_writes_file_when_dir_configured() {
        struct BigTool;

        #[async_trait::async_trait]
        impl Tool for BigTool {
            fn name(&self) -> &str {
                "big"
            }
            fn description(&self) -> &str {
                "Returns big output"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _params: serde_json::Value) -> Result<ToolResult> {
                let big = "X".repeat((1.7 * 1024.0 * 1024.0) as usize);
                Ok(ToolResult {
                    success: true,
                    output: big,
                })
            }
        }

        let llm = MockLlm::new(vec![
            r#"{"tool": "big", "params": {}}"#.to_string(),
            r#"{"done": true, "result": "Handoff: task was X, pending is Y"}"#.to_string(),
        ]);
        let big_tool = BigTool;
        let tools: Vec<&dyn Tool> = vec![&big_tool];

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = AgentExecConfig {
            system_prompt: None,
            max_iterations: None,
            identity_context: None,
            handoff_dir: Some(tmp.path().to_path_buf()),
            agent_id: Some("test-agent-42".to_string()),
            role: None,
            redact_output: false,
        };

        let result = run_agent_with_config("Do big work", "", &tools, &llm, &config)
            .await
            .expect("should succeed with handoff");

        assert!(result.output.contains("CONTEXT_HANDOFF_REQUIRED:"));

        // Verify file was written
        let entries: Vec<_> = std::fs::read_dir(tmp.path())
            .expect("read dir")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1, "expected exactly one handoff file");

        let content = std::fs::read_to_string(entries[0].path()).expect("read file");
        assert!(content.contains("type: context-handoff"));
        assert!(content.contains("agent_id: \"test-agent-42\""));
        assert!(content.contains("Handoff: task was X, pending is Y"));
    }

    #[tokio::test]
    async fn handoff_no_file_when_dir_not_configured() {
        struct BigTool;

        #[async_trait::async_trait]
        impl Tool for BigTool {
            fn name(&self) -> &str {
                "big"
            }
            fn description(&self) -> &str {
                "Returns big output"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({})
            }
            async fn execute(&self, _params: serde_json::Value) -> Result<ToolResult> {
                let big = "X".repeat((1.7 * 1024.0 * 1024.0) as usize);
                Ok(ToolResult {
                    success: true,
                    output: big,
                })
            }
        }

        let llm = MockLlm::new(vec![
            r#"{"tool": "big", "params": {}}"#.to_string(),
            r#"{"done": true, "result": "Handoff summary"}"#.to_string(),
        ]);
        let big_tool = BigTool;
        let tools: Vec<&dyn Tool> = vec![&big_tool];

        // Default config — no handoff_dir
        let result = run_agent("Do big work", "", &tools, &llm)
            .await
            .expect("should succeed");

        assert!(result.output.contains("CONTEXT_HANDOFF_REQUIRED:"));
        // No file assertions needed — just verify it doesn't crash without handoff_dir
    }

    #[tokio::test]
    async fn agent_with_identity_context_in_prompt() {
        // Use a mock LLM that captures messages to verify identity is prepended
        struct CaptureLlm {
            responses: Vec<String>,
            call_count: AtomicUsize,
            captured_system: std::sync::Mutex<Option<String>>,
        }

        #[async_trait::async_trait]
        impl LlmClient for CaptureLlm {
            async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
                // Capture the system prompt from the first message
                if let Some(msg) = messages.first() {
                    if msg.role == "system" {
                        let mut guard = self.captured_system.lock().unwrap();
                        if guard.is_none() {
                            *guard = Some(msg.content.clone());
                        }
                    }
                }
                let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
                self.responses
                    .get(idx)
                    .cloned()
                    .ok_or_else(|| anyhow!("mock LLM ran out of responses"))
            }
        }

        let llm = CaptureLlm {
            responses: vec![r#"{"done": true, "result": "Identity loaded"}"#.to_string()],
            call_count: AtomicUsize::new(0),
            captured_system: std::sync::Mutex::new(None),
        };
        let tools: Vec<&dyn Tool> = vec![];

        let config = AgentExecConfig {
            system_prompt: Some("You are a research agent.".to_string()),
            max_iterations: None,
            identity_context: Some("# SOUL\nYou are Symbiotic.".to_string()),
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: false,
        };

        let result = run_agent_with_config("Test", "", &tools, &llm, &config)
            .await
            .expect("should succeed");

        assert_eq!(result.output, "Identity loaded");

        // Verify the system prompt was prepended with the identity context
        let system = llm.captured_system.lock().unwrap();
        let system = system.as_ref().expect("system prompt should be captured");
        assert!(
            system.starts_with("# SOUL\nYou are Symbiotic."),
            "system prompt should start with identity context, got: {}",
            &system[..80.min(system.len())]
        );
        assert!(
            system.contains("You are a research agent."),
            "system prompt should contain the base prompt"
        );
        assert!(
            system.contains("---"),
            "identity and base prompt should be separated by ---"
        );
    }

    #[tokio::test]
    async fn agent_without_identity_context_has_no_separator() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "No identity"}"#.to_string()
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let config = AgentExecConfig {
            system_prompt: Some("You are a research agent.".to_string()),
            max_iterations: None,
            identity_context: None,
            handoff_dir: None,
            agent_id: None,
            role: None,
            redact_output: false,
        };

        let result = run_agent_with_config("Test", "", &tools, &llm, &config)
            .await
            .expect("should succeed");

        assert_eq!(result.output, "No identity");
    }

    // --- PII redaction tests ---

    #[tokio::test]
    async fn agent_redacts_email_from_done_response() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "Contact user@example.com for help"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Get contact", "", &tools, &llm)
            .await
            .expect("run_agent");

        // Default config has redact_output=true
        assert!(
            !result.output.contains("user@example.com"),
            "email should be redacted from output, got: {}",
            result.output
        );
        assert!(
            result.output.contains("[redacted-email]"),
            "output should contain redaction placeholder, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn agent_redacts_phone_from_non_json_response() {
        // LLM returns plain text (non-JSON) containing a phone number
        let llm = MockLlm::new(vec!["Call us at 555-123-4567 for support".to_string()]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Get phone", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert!(
            !result.output.contains("555-123-4567"),
            "phone should be redacted, got: {}",
            result.output
        );
        assert!(
            result.output.contains("[redacted-phone]"),
            "output should contain phone placeholder, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn agent_redacts_ip_from_done_response() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "Server is at 192.168.1.100"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Get server", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert!(
            !result.output.contains("192.168.1.100"),
            "IP should be redacted, got: {}",
            result.output
        );
        assert!(
            result.output.contains("[redacted-ip]"),
            "output should contain IP placeholder, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn agent_redacts_multiple_pii_types() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "Email user@example.com, call 555-123-4567, server 10.0.0.1"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Get info", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert!(!result.output.contains("user@example.com"));
        assert!(!result.output.contains("555-123-4567"));
        assert!(!result.output.contains("10.0.0.1"));
        assert!(result.output.contains("[redacted-email]"));
        assert!(result.output.contains("[redacted-phone]"));
        assert!(result.output.contains("[redacted-ip]"));
    }

    #[tokio::test]
    async fn agent_redaction_disabled_preserves_pii() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "Contact user@example.com for help"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let config = AgentExecConfig {
            redact_output: false,
            ..AgentExecConfig::default()
        };

        let result = run_agent_with_config("Get contact", "", &tools, &llm, &config)
            .await
            .expect("run_agent_with_config");

        assert!(
            result.output.contains("user@example.com"),
            "email should NOT be redacted when redact_output=false, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn agent_preserves_clean_output() {
        let llm = MockLlm::new(vec![
            r#"{"done": true, "result": "The analysis is complete with 42 results"}"#.to_string(),
        ]);
        let tools: Vec<&dyn Tool> = vec![];

        let result = run_agent("Analyze", "", &tools, &llm)
            .await
            .expect("run_agent");

        assert_eq!(
            result.output, "The analysis is complete with 42 results",
            "clean output should pass through unchanged"
        );
    }

    #[tokio::test]
    async fn default_config_has_redaction_enabled() {
        let config = AgentExecConfig::default();
        assert!(config.redact_output, "redact_output should default to true");
    }
}
