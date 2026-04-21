//! LLM-backed code generation for skill synthesis.
//!
//! The `CodeGenerator` trait abstracts how source code is produced from a
//! problem description. The `LlmCodeGenerator` implementation uses a
//! `CompletionProvider` (from `symbiotic-providers`) to generate Rust source
//! code for new skills.
//!
//! The generated code follows the `stdio-json-rpc` protocol: read JSON
//! requests from stdin, write JSON responses to stdout.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::synthesis::{GeneratedSource, SynthesisError, SynthesisRequest};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Trait for generating skill source code from a problem description.
#[async_trait::async_trait]
pub trait CodeGenerator: Send + Sync {
    /// Generate `Cargo.toml` + `src/main.rs` from a synthesis request.
    async fn generate(&self, request: &SynthesisRequest)
        -> Result<GeneratedSource, SynthesisError>;

    /// Re-generate code after a previous attempt failed testing.
    ///
    /// The `previous_error` contains the stderr from the failed test run,
    /// allowing the generator to fix issues. The default implementation
    /// ignores the error and falls back to [`generate`](Self::generate).
    async fn generate_with_feedback(
        &self,
        request: &SynthesisRequest,
        _previous_error: &str,
    ) -> Result<GeneratedSource, SynthesisError> {
        self.generate(request).await
    }
}

// ---------------------------------------------------------------------------
// LLM-backed implementation
// ---------------------------------------------------------------------------

/// Configuration for the LLM code generator.
#[derive(Debug, Clone)]
pub struct LlmCodeGenConfig {
    /// Maximum tokens for the LLM response.
    pub max_tokens: u32,
    /// Temperature (lower = more deterministic).
    pub temperature: f32,
}

impl Default for LlmCodeGenConfig {
    fn default() -> Self {
        Self {
            max_tokens: 4096,
            temperature: 0.2,
        }
    }
}

/// LLM response containing generated source code.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CodeGenResponse {
    cargo_toml: String,
    main_rs: String,
}

/// An LLM-backed code generator.
///
/// Uses any type implementing the `LlmChat` trait to generate Rust code.
/// The LLM is given a system prompt defining the skill contract (stdio-json-rpc
/// protocol, serde types, test structure) and the user's problem description.
///
/// This struct does NOT depend on `symbiotic-providers` directly -- it uses
/// the `LlmChat` trait which the daemon implements as an adapter over
/// `CompletionProvider` / `ProviderRouter`. This keeps the dependency graph clean.
pub struct LlmCodeGenerator {
    llm: Arc<dyn LlmChat>,
    config: LlmCodeGenConfig,
}

/// Minimal LLM chat interface for code generation.
///
/// This trait is intentionally simple so that it can be implemented by
/// adapting `CompletionProvider` from `symbiotic-providers` without
/// introducing a direct dependency on that crate.
#[async_trait::async_trait]
pub trait LlmChat: Send + Sync {
    /// Send a system + user message and get a response string.
    async fn chat(&self, system: &str, user: &str) -> Result<String, anyhow::Error>;
}

impl LlmCodeGenerator {
    /// Create a new LLM code generator.
    pub fn new(llm: Arc<dyn LlmChat>, config: LlmCodeGenConfig) -> Self {
        Self { llm, config }
    }

    /// Returns the code generation config (used by daemon adapter to set
    /// max_tokens and temperature on the underlying `CompletionRequest`).
    pub fn config(&self) -> &LlmCodeGenConfig {
        &self.config
    }

    /// Build the system prompt for code generation.
    fn system_prompt() -> String {
        r#"You are the Symbiotic Coder Agent. Your ONLY job is to generate Rust source code for a new skill.

A skill is a standalone Rust binary that:
1. Reads JSON requests from stdin (one per line)
2. Processes them
3. Writes JSON responses to stdout (one per line)

Request format:
```json
{"method": "execute", "params": {...}}
```

Response format:
```json
{"result": {...}, "error": null}
```

You MUST output valid JSON with exactly two fields:
- "cargo_toml": The full contents of Cargo.toml
- "main_rs": The full contents of src/main.rs

Requirements for the generated code:
- Use serde and serde_json for JSON handling
- Include at least one unit test in a #[cfg(test)] module
- Handle errors gracefully (never panic on bad input)
- Use the stdio-json-rpc protocol described above
- Keep dependencies minimal (serde, serde_json, plus domain-specific crates)
- Edition 2021
- The binary name in Cargo.toml must match the skill name

Output ONLY the JSON object. No markdown fences, no explanation."#.to_string()
    }

    /// Build the user prompt for a specific synthesis request.
    fn user_prompt(request: &SynthesisRequest) -> String {
        format!(
            "Generate a Rust skill binary named \"{}\".\n\nDescription: {}\n\nProtocol: {}\n\nThe skill should accept an \"execute\" method with relevant parameters and return a meaningful result.",
            request.skill_name, request.description, request.protocol
        )
    }

    /// Parse and validate the LLM response into a `GeneratedSource`.
    fn validate_and_parse(raw: &str) -> Result<GeneratedSource, SynthesisError> {
        let parsed = Self::parse_response(raw)?;

        if !parsed.cargo_toml.contains("[package]") {
            return Err(SynthesisError::CodeGenFailed(
                "generated Cargo.toml missing [package] section".to_string(),
            ));
        }
        if !parsed.main_rs.contains("fn main()") {
            return Err(SynthesisError::CodeGenFailed(
                "generated main.rs missing fn main()".to_string(),
            ));
        }

        Ok(GeneratedSource {
            cargo_toml: parsed.cargo_toml,
            main_rs: parsed.main_rs,
        })
    }

    /// Parse the LLM response into a `CodeGenResponse`.
    fn parse_response(raw: &str) -> Result<CodeGenResponse, SynthesisError> {
        // Try direct JSON parse first
        if let Ok(resp) = serde_json::from_str::<CodeGenResponse>(raw) {
            return Ok(resp);
        }

        // Try extracting JSON from markdown code fences
        let trimmed = raw.trim();
        let json_str = if trimmed.starts_with("```json") {
            trimmed
                .strip_prefix("```json")
                .and_then(|s| s.strip_suffix("```"))
                .unwrap_or(trimmed)
                .trim()
        } else if trimmed.starts_with("```") {
            trimmed
                .strip_prefix("```")
                .and_then(|s| s.strip_suffix("```"))
                .unwrap_or(trimmed)
                .trim()
        } else {
            trimmed
        };

        serde_json::from_str::<CodeGenResponse>(json_str).map_err(|e| {
            SynthesisError::CodeGenFailed(format!("failed to parse LLM response as JSON: {e}"))
        })
    }
}

#[async_trait::async_trait]
impl CodeGenerator for LlmCodeGenerator {
    async fn generate(
        &self,
        request: &SynthesisRequest,
    ) -> Result<GeneratedSource, SynthesisError> {
        let system = Self::system_prompt();
        let user = Self::user_prompt(request);

        let response = self
            .llm
            .chat(&system, &user)
            .await
            .map_err(|e| SynthesisError::CodeGenFailed(format!("LLM call failed: {e}")))?;

        Self::validate_and_parse(&response)
    }

    async fn generate_with_feedback(
        &self,
        request: &SynthesisRequest,
        previous_error: &str,
    ) -> Result<GeneratedSource, SynthesisError> {
        let system = Self::system_prompt();
        let user = format!(
            "{}\n\nPrevious attempt failed with:\n```\n{}\n```\n\n\
             Fix the issues and regenerate.",
            Self::user_prompt(request),
            previous_error,
        );

        let response = self
            .llm
            .chat(&system, &user)
            .await
            .map_err(|e| SynthesisError::CodeGenFailed(format!("LLM call failed: {e}")))?;

        Self::validate_and_parse(&response)
    }
}

// ---------------------------------------------------------------------------
// Stub code generator (for testing / fallback)
// ---------------------------------------------------------------------------

/// Stub code generator that produces template code without an LLM.
///
/// This is the same logic as the existing `generate_stub_source` but wrapped
/// in the `CodeGenerator` trait for consistency.
pub struct StubCodeGenerator;

#[async_trait::async_trait]
impl CodeGenerator for StubCodeGenerator {
    async fn generate(
        &self,
        request: &SynthesisRequest,
    ) -> Result<GeneratedSource, SynthesisError> {
        Ok(crate::synthesis::generate_stub_source(
            &request.skill_name,
            &request.description,
        ))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Mock LLM --

    struct MockLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmChat for MockLlm {
        async fn chat(&self, _system: &str, _user: &str) -> Result<String, anyhow::Error> {
            Ok(self.response.clone())
        }
    }

    /// Mock LLM that captures the user prompt for verification.
    struct CapturingLlm {
        response: String,
        captured_user: std::sync::Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl LlmChat for CapturingLlm {
        async fn chat(&self, _system: &str, user: &str) -> Result<String, anyhow::Error> {
            *self.captured_user.lock().unwrap() = Some(user.to_string());
            Ok(self.response.clone())
        }
    }

    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmChat for FailingLlm {
        async fn chat(&self, _system: &str, _user: &str) -> Result<String, anyhow::Error> {
            Err(anyhow::anyhow!("LLM service unavailable"))
        }
    }

    fn valid_json_response() -> String {
        serde_json::json!({
            "cargo_toml": "[package]\nname = \"test-skill\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = { version = \"1.0\", features = [\"derive\"] }\nserde_json = \"1.0\"\n",
            "main_rs": "use std::io::{self, BufRead, Write};\nuse serde::{Deserialize, Serialize};\n\n#[derive(Deserialize)]\nstruct Request { method: String, params: serde_json::Value }\n\n#[derive(Serialize)]\nstruct Response { result: serde_json::Value, error: Option<String> }\n\nfn main() {\n    let stdin = io::stdin();\n    let stdout = io::stdout();\n    let mut stdout = stdout.lock();\n    for line in stdin.lock().lines() {\n        let line = match line { Ok(l) => l, Err(_) => break };\n        if line.trim().is_empty() { continue; }\n        let request: Request = match serde_json::from_str(&line) {\n            Ok(r) => r,\n            Err(e) => {\n                let resp = Response { result: serde_json::Value::Null, error: Some(format!(\"parse error: {e}\")) };\n                let _ = writeln!(stdout, \"{}\", serde_json::to_string(&resp).unwrap());\n                continue;\n            }\n        };\n        let response = match request.method.as_str() {\n            \"execute\" => Response { result: serde_json::json!({\"status\": \"ok\"}), error: None },\n            _ => Response { result: serde_json::Value::Null, error: Some(format!(\"unknown method: {}\", request.method)) },\n        };\n        let _ = writeln!(stdout, \"{}\", serde_json::to_string(&response).unwrap());\n    }\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn test_basic() { assert!(true); }\n}\n"
        })
        .to_string()
    }

    // -- parse_response --

    #[test]
    fn parse_valid_json() {
        let json = valid_json_response();
        let resp = LlmCodeGenerator::parse_response(&json).unwrap();
        assert!(resp.cargo_toml.contains("[package]"));
        assert!(resp.main_rs.contains("fn main()"));
    }

    #[test]
    fn parse_json_in_markdown_fences() {
        let json = valid_json_response();
        let wrapped = format!("```json\n{}\n```", json);
        let resp = LlmCodeGenerator::parse_response(&wrapped).unwrap();
        assert!(resp.cargo_toml.contains("[package]"));
    }

    #[test]
    fn parse_json_in_plain_fences() {
        let json = valid_json_response();
        let wrapped = format!("```\n{}\n```", json);
        let resp = LlmCodeGenerator::parse_response(&wrapped).unwrap();
        assert!(resp.cargo_toml.contains("[package]"));
    }

    #[test]
    fn parse_invalid_json_fails() {
        let err = LlmCodeGenerator::parse_response("not json at all").unwrap_err();
        assert!(matches!(err, SynthesisError::CodeGenFailed(_)));
        assert!(err.to_string().contains("failed to parse"));
    }

    // -- system_prompt --

    #[test]
    fn system_prompt_mentions_protocol() {
        let prompt = LlmCodeGenerator::system_prompt();
        assert!(prompt.contains("stdio-json-rpc"));
        assert!(prompt.contains("serde"));
        assert!(prompt.contains("JSON"));
    }

    // -- user_prompt --

    #[test]
    fn user_prompt_includes_request_details() {
        let request = SynthesisRequest::new(
            "url-fetcher".to_string(),
            "Fetches URLs safely".to_string(),
            "agent-1".to_string(),
        );
        let prompt = LlmCodeGenerator::user_prompt(&request);
        assert!(prompt.contains("url-fetcher"));
        assert!(prompt.contains("Fetches URLs safely"));
        assert!(prompt.contains("stdio-json-rpc"));
    }

    // -- LlmCodeGenerator::generate --

    #[tokio::test]
    async fn generate_with_valid_llm_response() {
        let llm = Arc::new(MockLlm {
            response: valid_json_response(),
        });
        let gen = LlmCodeGenerator::new(llm, LlmCodeGenConfig::default());

        let request = SynthesisRequest::new(
            "test-skill".to_string(),
            "A test skill".to_string(),
            "agent-1".to_string(),
        );

        let source = gen.generate(&request).await.unwrap();
        assert!(source.cargo_toml.contains("[package]"));
        assert!(source.main_rs.contains("fn main()"));
    }

    #[tokio::test]
    async fn generate_fails_on_llm_error() {
        let llm = Arc::new(FailingLlm);
        let gen = LlmCodeGenerator::new(llm, LlmCodeGenConfig::default());

        let request = SynthesisRequest::new(
            "test-skill".to_string(),
            "A test skill".to_string(),
            "agent-1".to_string(),
        );

        let err = gen.generate(&request).await.unwrap_err();
        assert!(matches!(err, SynthesisError::CodeGenFailed(_)));
        assert!(err.to_string().contains("LLM service unavailable"));
    }

    #[tokio::test]
    async fn generate_fails_on_missing_package_section() {
        let llm = Arc::new(MockLlm {
            response: serde_json::json!({
                "cargo_toml": "name = \"bad\"",
                "main_rs": "fn main() {}"
            })
            .to_string(),
        });
        let gen = LlmCodeGenerator::new(llm, LlmCodeGenConfig::default());

        let request = SynthesisRequest::new(
            "bad-skill".to_string(),
            "Bad skill".to_string(),
            "agent-1".to_string(),
        );

        let err = gen.generate(&request).await.unwrap_err();
        assert!(err.to_string().contains("[package]"));
    }

    #[tokio::test]
    async fn generate_fails_on_missing_main() {
        let llm = Arc::new(MockLlm {
            response: serde_json::json!({
                "cargo_toml": "[package]\nname = \"test\"\nversion = \"0.1.0\"\nedition = \"2021\"",
                "main_rs": "// no main function here"
            })
            .to_string(),
        });
        let gen = LlmCodeGenerator::new(llm, LlmCodeGenConfig::default());

        let request = SynthesisRequest::new(
            "no-main".to_string(),
            "Missing main".to_string(),
            "agent-1".to_string(),
        );

        let err = gen.generate(&request).await.unwrap_err();
        assert!(err.to_string().contains("fn main()"));
    }

    // -- StubCodeGenerator --

    #[tokio::test]
    async fn stub_generator_produces_valid_source() {
        let gen = StubCodeGenerator;
        let request = SynthesisRequest::new(
            "stub-skill".to_string(),
            "A stub skill".to_string(),
            "agent-1".to_string(),
        );

        let source = gen.generate(&request).await.unwrap();
        assert!(source.cargo_toml.contains("stub-skill"));
        assert!(source.main_rs.contains("fn main()"));
        assert!(source.main_rs.contains("stub-skill"));
    }

    // -- generate_with_feedback --

    #[tokio::test]
    async fn generate_with_feedback_includes_error_in_prompt() {
        let llm = Arc::new(CapturingLlm {
            response: valid_json_response(),
            captured_user: std::sync::Mutex::new(None),
        });
        let gen = LlmCodeGenerator::new(llm.clone(), LlmCodeGenConfig::default());

        let request = SynthesisRequest::new(
            "test-skill".to_string(),
            "A test skill".to_string(),
            "agent-1".to_string(),
        );

        let source = gen
            .generate_with_feedback(&request, "error[E0308]: mismatched types")
            .await
            .unwrap();
        assert!(source.cargo_toml.contains("[package]"));

        let captured = llm.captured_user.lock().unwrap().clone().unwrap();
        assert!(captured.contains("error[E0308]: mismatched types"));
        assert!(captured.contains("Previous attempt failed"));
        assert!(captured.contains("Fix the issues"));
    }

    #[tokio::test]
    async fn stub_generator_ignores_feedback() {
        let gen = StubCodeGenerator;
        let request = SynthesisRequest::new(
            "stub-skill".to_string(),
            "A stub skill".to_string(),
            "agent-1".to_string(),
        );

        // generate_with_feedback uses the default impl which ignores the error
        let source = gen
            .generate_with_feedback(&request, "some error")
            .await
            .unwrap();
        assert!(source.main_rs.contains("fn main()"));
    }

    // -- LlmCodeGenConfig --

    #[test]
    fn default_config_values() {
        let config = LlmCodeGenConfig::default();
        assert_eq!(config.max_tokens, 4096);
        assert!((config.temperature - 0.2).abs() < f32::EPSILON);
    }
}
