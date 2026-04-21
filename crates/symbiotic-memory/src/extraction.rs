//! LLM-based memory extraction via Ollama.
//!
//! Implements the `MemoryExtractor` trait using Ollama's chat API with
//! structured JSON output. The extraction prompt is defined in
//! `docs/design/memory-extraction.md`.

use crate::grounding::GroundingChecker;
use crate::types::{
    ExtractedFact, ExtractionError, ExtractionInput, ExtractionResult, GroundingResult,
};

/// Trait for extracting facts from text.
#[async_trait::async_trait]
pub trait MemoryExtractor: Send + Sync {
    /// Extract facts from input text using LLM.
    async fn extract(&self, input: &ExtractionInput) -> Result<ExtractionResult, ExtractionError>;

    /// Validate extracted facts against source text (hallucination check).
    fn validate_grounding(
        &self,
        facts: &[ExtractedFact],
        source_text: &str,
    ) -> Vec<GroundingResult>;
}

/// LLM client trait for extraction, enabling test mocking.
///
/// Mirrors the interface from `symbiotic-agents::llm::LlmClient` but is defined
/// here to avoid a cross-crate dependency. Production code injects the real client.
#[async_trait::async_trait]
pub trait ExtractionLlmClient: Send + Sync {
    /// Send a chat message and return the assistant's response.
    /// When `json_mode` is true, the model should return valid JSON.
    async fn chat(
        &self,
        system_prompt: &str,
        user_message: &str,
        json_mode: bool,
    ) -> Result<String, ExtractionError>;
}

/// Ollama-based implementation of `MemoryExtractor`.
pub struct OllamaExtractor {
    client: Box<dyn ExtractionLlmClient>,
    grounding: GroundingChecker,
}

impl OllamaExtractor {
    pub fn new(client: Box<dyn ExtractionLlmClient>) -> Self {
        Self {
            client,
            grounding: GroundingChecker::default(),
        }
    }

    pub fn with_grounding(mut self, grounding: GroundingChecker) -> Self {
        self.grounding = grounding;
        self
    }

    /// Build the extraction prompt for the given input.
    fn build_prompt(input: &ExtractionInput) -> (String, String) {
        let system = EXTRACTION_SYSTEM_PROMPT.to_string();
        let user = format!("Text:\n{}\n\nSource ID: {}", input.text, input.source_id);
        (system, user)
    }

    /// Parse the LLM JSON response into an ExtractionResult.
    fn parse_response(raw: &str) -> Result<ExtractionResult, ExtractionError> {
        serde_json::from_str::<ExtractionResult>(raw)
            .map_err(|e| ExtractionError::InvalidOutput(format!("{e}: {raw}")))
    }
}

#[async_trait::async_trait]
impl MemoryExtractor for OllamaExtractor {
    async fn extract(&self, input: &ExtractionInput) -> Result<ExtractionResult, ExtractionError> {
        if input.text.is_empty() {
            return Ok(ExtractionResult {
                facts: vec![],
                input_tokens: None,
                output_tokens: None,
            });
        }

        let (system, user) = Self::build_prompt(input);
        let raw_response = self.client.chat(&system, &user, true).await?;
        Self::parse_response(&raw_response)
    }

    fn validate_grounding(
        &self,
        facts: &[ExtractedFact],
        source_text: &str,
    ) -> Vec<GroundingResult> {
        self.grounding.validate(facts, source_text)
    }
}

/// System prompt for the extraction LLM call.
const EXTRACTION_SYSTEM_PROMPT: &str = r#"You are an entity and fact extraction system. Extract stable, user-specific facts from the following text.

For each fact, provide:
- entity_type: one of [person, project, org, tool, preference, concept, task]
- entity_name: the name of the entity
- fact: a concise statement about the entity
- confidence: your confidence in this fact from 0.0 to 1.0
- evidence_quote: the exact text span that supports this fact
- temporal_hint: if the fact has a time scope (e.g., "since 2024", "currently"), include it; otherwise null

Rules:
- Only extract facts that are stable (likely true for weeks/months, not transient).
- Do not extract greetings, filler, or meta-conversation.
- Do not invent facts not supported by the text.
- If a fact contradicts something you might expect, still extract it with the evidence.
- Prefer specific facts over vague ones.

Respond with valid JSON matching this schema:
{
  "facts": [
    {
      "entity_type": "person",
      "entity_name": "Jane Doe",
      "fact": "Works as a designer at Acme Corp",
      "confidence": 0.85,
      "evidence_quote": "Jane mentioned she's been designing at Acme for two years",
      "temporal_hint": "since 2024"
    }
  ]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::EntityType;
    use std::sync::Mutex;

    /// Mock LLM client for testing extraction without a real Ollama instance.
    struct MockLlmClient {
        response: Mutex<Option<Result<String, ExtractionError>>>,
    }

    impl MockLlmClient {
        fn with_response(response: Result<String, ExtractionError>) -> Self {
            Self {
                response: Mutex::new(Some(response)),
            }
        }
    }

    #[async_trait::async_trait]
    impl ExtractionLlmClient for MockLlmClient {
        async fn chat(
            &self,
            _system_prompt: &str,
            _user_message: &str,
            _json_mode: bool,
        ) -> Result<String, ExtractionError> {
            self.response
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(ExtractionError::OllamaUnavailable(
                    "no mock response".to_string(),
                )))
        }
    }

    fn sample_llm_response() -> String {
        serde_json::json!({
            "facts": [
                {
                    "entity_type": "person",
                    "entity_name": "Alice",
                    "fact": "Works as a backend engineer at Acme Corp",
                    "confidence": 0.9,
                    "evidence_quote": "Alice has been working as a backend engineer at Acme Corp",
                    "temporal_hint": "since 2023"
                },
                {
                    "entity_type": "tool",
                    "entity_name": "Rust",
                    "fact": "Primary programming language used by Alice",
                    "confidence": 0.85,
                    "evidence_quote": "She primarily uses Rust for all her projects",
                    "temporal_hint": null
                }
            ]
        })
        .to_string()
    }

    fn sample_input() -> ExtractionInput {
        ExtractionInput {
            text: "Alice has been working as a backend engineer at Acme Corp since 2023. \
                   She primarily uses Rust for all her projects and is passionate about \
                   systems programming."
                .to_string(),
            source_id: "doc-001".to_string(),
            source_url: Some("https://example.com/notes".to_string()),
        }
    }

    #[tokio::test]
    async fn extract_parses_valid_llm_response() {
        let client = MockLlmClient::with_response(Ok(sample_llm_response()));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let result = extractor.extract(&input).await.expect("extraction");
        assert_eq!(result.facts.len(), 2);
        assert_eq!(result.facts[0].entity_name, "Alice");
        assert_eq!(result.facts[0].entity_type, EntityType::Person);
        assert_eq!(result.facts[0].confidence, 0.9);
        assert_eq!(
            result.facts[0].temporal_hint,
            Some("since 2023".to_string())
        );
        assert_eq!(result.facts[1].entity_name, "Rust");
        assert_eq!(result.facts[1].entity_type, EntityType::Tool);
        assert!(result.facts[1].temporal_hint.is_none());
    }

    #[tokio::test]
    async fn extract_empty_text_returns_empty() {
        let client = MockLlmClient::with_response(Ok("should not be called".to_string()));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = ExtractionInput {
            text: "".to_string(),
            source_id: "empty".to_string(),
            source_url: None,
        };

        let result = extractor.extract(&input).await.expect("extraction");
        assert!(result.facts.is_empty());
    }

    #[tokio::test]
    async fn extract_invalid_json_returns_error() {
        let client = MockLlmClient::with_response(Ok("not valid json {{{".to_string()));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let err = extractor.extract(&input).await.unwrap_err();
        match err {
            ExtractionError::InvalidOutput(msg) => {
                assert!(msg.contains("not valid json"), "got: {msg}");
            }
            other => panic!("expected InvalidOutput, got: {other}"),
        }
    }

    #[tokio::test]
    async fn extract_ollama_unavailable_propagates() {
        let client = MockLlmClient::with_response(Err(ExtractionError::OllamaUnavailable(
            "connection refused".to_string(),
        )));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let err = extractor.extract(&input).await.unwrap_err();
        match err {
            ExtractionError::OllamaUnavailable(msg) => {
                assert!(msg.contains("connection refused"));
            }
            other => panic!("expected OllamaUnavailable, got: {other}"),
        }
    }

    #[tokio::test]
    async fn extract_empty_facts_array() {
        let response = serde_json::json!({"facts": []}).to_string();
        let client = MockLlmClient::with_response(Ok(response));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let result = extractor.extract(&input).await.expect("extraction");
        assert!(result.facts.is_empty());
    }

    #[tokio::test]
    async fn extract_missing_optional_temporal_hint() {
        let response = serde_json::json!({
            "facts": [{
                "entity_type": "project",
                "entity_name": "Symbiotic",
                "fact": "An AI assistant project",
                "confidence": 0.75,
                "evidence_quote": "Working on Symbiotic, the AI assistant",
                "temporal_hint": null
            }]
        })
        .to_string();
        let client = MockLlmClient::with_response(Ok(response));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let result = extractor.extract(&input).await.expect("extraction");
        assert_eq!(result.facts.len(), 1);
        assert!(result.facts[0].temporal_hint.is_none());
    }

    #[tokio::test]
    async fn validate_grounding_with_matching_quotes() {
        let client = MockLlmClient::with_response(Ok(sample_llm_response()));
        let extractor = OllamaExtractor::new(Box::new(client));
        let input = sample_input();

        let result = extractor.extract(&input).await.expect("extraction");
        let grounding = extractor.validate_grounding(&result.facts, &input.text);

        assert_eq!(grounding.len(), 2);
        // "Alice has been working as a backend engineer at Acme Corp" is in the source
        assert!(grounding[0].grounded, "Alice fact should be grounded");
        // "She primarily uses Rust for all her projects" is in the source
        assert!(grounding[1].grounded, "Rust fact should be grounded");
    }

    #[tokio::test]
    async fn validate_grounding_detects_hallucinated_quote() {
        let client = MockLlmClient::with_response(Ok("unused".to_string()));
        let extractor = OllamaExtractor::new(Box::new(client));

        let fabricated = ExtractedFact {
            entity_type: EntityType::Person,
            entity_name: "Bob".to_string(),
            fact: "CEO of TechCorp".to_string(),
            confidence: 0.9,
            evidence_quote: "Bob is the CEO of TechCorp and runs everything".to_string(),
            temporal_hint: None,
            fact_type: None,
        };

        let source = "Alice works at Acme. She uses Rust.";
        let grounding = extractor.validate_grounding(&[fabricated], source);

        assert_eq!(grounding.len(), 1);
        assert!(
            !grounding[0].grounded,
            "fabricated fact should NOT be grounded"
        );
    }

    #[test]
    fn build_prompt_includes_source_id() {
        let input = ExtractionInput {
            text: "Test content".to_string(),
            source_id: "src-42".to_string(),
            source_url: None,
        };
        let (system, user) = OllamaExtractor::build_prompt(&input);
        assert!(system.contains("entity and fact extraction"));
        assert!(user.contains("Test content"));
        assert!(user.contains("src-42"));
    }

    #[test]
    fn parse_response_valid_json() {
        let json = sample_llm_response();
        let result = OllamaExtractor::parse_response(&json).expect("parse");
        assert_eq!(result.facts.len(), 2);
    }

    #[test]
    fn parse_response_malformed_json() {
        let err = OllamaExtractor::parse_response("not json").unwrap_err();
        match err {
            ExtractionError::InvalidOutput(_) => {}
            other => panic!("expected InvalidOutput, got: {other}"),
        }
    }

    #[test]
    fn parse_response_wrong_schema() {
        // Valid JSON but wrong schema (missing required fields)
        let json = r#"{"items": [{"name": "test"}]}"#;
        let err = OllamaExtractor::parse_response(json).unwrap_err();
        match err {
            ExtractionError::InvalidOutput(_) => {}
            other => panic!("expected InvalidOutput, got: {other}"),
        }
    }

    #[test]
    fn parse_response_partial_fact_missing_field() {
        // A fact missing the required `evidence_quote` field
        let json = serde_json::json!({
            "facts": [{
                "entity_type": "person",
                "entity_name": "Jane",
                "fact": "Works at Acme",
                "confidence": 0.8
            }]
        })
        .to_string();
        let err = OllamaExtractor::parse_response(&json).unwrap_err();
        match err {
            ExtractionError::InvalidOutput(msg) => {
                assert!(msg.contains("evidence_quote"), "got: {msg}");
            }
            other => panic!("expected InvalidOutput, got: {other}"),
        }
    }
}
