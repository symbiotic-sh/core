//! Lightweight ResearchOnly sub-goal agent (T130 §05).
//!
//! The researcher is the cheapest / lowest-risk sub-goal backend in the
//! design (§4.1, §8.1). It is strictly read-only:
//!
//! - consumes the question carried by [`UnblockKey::ResearchOnly`] plus the
//!   operator answers delivered on `goal.unblocked`;
//! - queries a [`ResearchRecall`] source for relevant Archive snippets;
//! - asks a `fast`-tier LLM (the one handed in via the dispatcher — we avoid
//!   reaching into T83's LLM Runtime Manager directly here to keep this
//!   chunk's dependency graph flat) to synthesise an Archive note draft;
//! - returns an [`ArchiveNoteDraft`] — **the draft is not written to the
//!   Archive in this chunk**. The write path + thread-message pill are part
//!   of the three-channel merge-back (§5) and land in §08.
//!
//! By wiring everything through two small traits (`ResearchRecall`,
//! `ResearchLlm`) the full daemon composition continues to work with the
//! real `RecallGateway` and `ProviderRouter` (each can implement the trait
//! behind the scenes in a later chunk) without forcing tests to spin up
//! heavy infrastructure. Unit tests substitute trivial stubs.
//!
//! # Scope
//!
//! This module stays deliberately minimal — the design doc's three-channel
//! merge-back (§5) requires Archive writes + thread pills, which are §08's
//! job. Here we produce the *draft* only; the dispatcher emits the event
//! channel signal and defers the rest.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};
use thiserror::Error;

/// One Archive snippet surfaced by Recall — kept intentionally narrow so
/// stub implementations in tests don't need to replicate the full
/// [`symbiotic_context::RecallGateway`] surface.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSnippet {
    /// The Archive path / identifier for provenance.
    pub source: String,
    /// The retrieved text (may be a chunk).
    pub content: String,
    /// Retrieval confidence (0.0–1.0). `1.0` means "exact match".
    pub score: f32,
}

/// Minimal Recall shape the researcher needs. Real composition wraps a
/// `RecallGateway` behind this trait; tests provide fixture snippets.
#[async_trait]
pub trait ResearchRecall: Send + Sync {
    async fn query(&self, topic: &str, top_k: usize) -> anyhow::Result<Vec<RecallSnippet>>;
}

/// A thin wrapper around [`LlmClient`] so the dispatcher can decide which
/// tier (fast / balanced / deep) to pass in. The researcher always runs on
/// the `fast` tier per design §13.5 / §5.2 cost notes, but we carry the
/// trait separately to make that choice explicit at the call site.
pub type ResearchLlm = Arc<dyn LlmClient>;

/// The shape of the note produced by [`ResearcherAgent::execute`].
///
/// The draft is **not** persisted here — §08's merge-back lands the write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveNoteDraft {
    /// Stable id inherited from the sub-goal. Used as the Archive filename
    /// stem by §08's write path.
    pub sub_goal_id: String,
    /// The research question that seeded the note.
    pub question: String,
    /// One-line summary suitable for the thread pill (§5.4).
    pub summary: String,
    /// Full markdown body — the durable Archive artifact (§5.3).
    pub markdown: String,
    /// Source attributions (Archive paths / URLs). Passed through from
    /// `RecallSnippet::source`.
    pub sources: Vec<String>,
}

#[derive(Debug, Error)]
pub enum ResearcherError {
    #[error("recall query failed: {0}")]
    Recall(#[source] anyhow::Error),
    #[error("llm call failed: {0}")]
    Llm(#[source] anyhow::Error),
    #[error("llm returned empty response")]
    EmptyLlmResponse,
}

/// Inputs for a single research run.
#[derive(Debug, Clone)]
pub struct ResearchRequest {
    /// The sub-goal id the dispatcher minted when routing this run.
    pub sub_goal_id: String,
    /// The question from `UnblockKey::ResearchOnly { question }`.
    pub question: String,
    /// Answers carried on the `goal.unblocked` event (question_index → text).
    /// Folded into the LLM prompt as operator context.
    pub operator_answers: Vec<(usize, String)>,
    /// How many Recall snippets to retrieve. Defaults to 5.
    pub top_k: usize,
}

impl ResearchRequest {
    pub fn new(sub_goal_id: impl Into<String>, question: impl Into<String>) -> Self {
        Self {
            sub_goal_id: sub_goal_id.into(),
            question: question.into(),
            operator_answers: Vec::new(),
            top_k: 5,
        }
    }

    pub fn with_answers(mut self, answers: Vec<(usize, String)>) -> Self {
        self.operator_answers = answers;
        self
    }

    pub fn with_top_k(mut self, top_k: usize) -> Self {
        self.top_k = top_k;
        self
    }
}

/// Lightweight agent composition. Holds only the two collaborators it
/// needs; no swarm, no git, no capability elevation (design §8.1 —
/// ResearchOnly requires only `recall_query` + `archive_write_semantic`,
/// both default-granted).
pub struct ResearcherAgent {
    recall: Arc<dyn ResearchRecall>,
    llm: ResearchLlm,
}

impl ResearcherAgent {
    pub fn new(recall: Arc<dyn ResearchRecall>, llm: ResearchLlm) -> Self {
        Self { recall, llm }
    }

    /// Execute the research run. Returns an [`ArchiveNoteDraft`] ready for
    /// §08's merge-back to persist.
    pub async fn execute(
        &self,
        request: ResearchRequest,
    ) -> Result<ArchiveNoteDraft, ResearcherError> {
        let snippets = self
            .recall
            .query(&request.question, request.top_k)
            .await
            .map_err(ResearcherError::Recall)?;

        let prompt = build_research_prompt(&request, &snippets);
        let response = self
            .llm
            .chat(&prompt, /* json_mode */ false)
            .await
            .map_err(ResearcherError::Llm)?;

        let trimmed = response.trim();
        if trimmed.is_empty() {
            return Err(ResearcherError::EmptyLlmResponse);
        }

        let summary = first_nonempty_line(trimmed)
            .unwrap_or_else(|| format!("Research note for: {}", request.question));
        let markdown = render_markdown(&request, &snippets, trimmed);
        let sources = snippets.into_iter().map(|s| s.source).collect();

        Ok(ArchiveNoteDraft {
            sub_goal_id: request.sub_goal_id,
            question: request.question,
            summary,
            markdown,
            sources,
        })
    }
}

fn build_research_prompt(
    request: &ResearchRequest,
    snippets: &[RecallSnippet],
) -> Vec<ChatMessage> {
    let mut system = String::from(
        "You are a research sub-agent in the Symbiotic system. \
         Produce a concise, well-cited markdown note answering the question. \
         Start with a single-line TL;DR summary, then supporting analysis, \
         then a 'Sources' section enumerating the provided references. \
         Do not speculate beyond the snippets unless explicitly flagged.",
    );
    if !request.operator_answers.is_empty() {
        system.push_str("\n\nOperator context (answers to clarifying questions):\n");
        for (idx, ans) in &request.operator_answers {
            system.push_str(&format!("- Q{idx}: {ans}\n"));
        }
    }

    let mut user = format!("Research question:\n{}\n\n", request.question);
    if snippets.is_empty() {
        user.push_str(
            "No Archive snippets were found. Answer from first principles and flag the gap.\n",
        );
    } else {
        user.push_str("Archive snippets (ranked by relevance):\n");
        for (i, snippet) in snippets.iter().enumerate() {
            user.push_str(&format!(
                "\n[{i}] ({score:.2}) {src}\n{body}\n",
                i = i + 1,
                score = snippet.score,
                src = snippet.source,
                body = snippet.content
            ));
        }
    }

    vec![
        ChatMessage {
            role: "system".to_string(),
            content: system,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ]
}

fn render_markdown(
    request: &ResearchRequest,
    snippets: &[RecallSnippet],
    llm_body: &str,
) -> String {
    let mut md = String::new();
    md.push_str(&format!("# Research: {}\n\n", request.question));
    md.push_str(&format!("- **sub_goal_id**: `{}`\n", request.sub_goal_id));
    if !request.operator_answers.is_empty() {
        md.push_str("- **operator_answers**:\n");
        for (idx, ans) in &request.operator_answers {
            md.push_str(&format!("  - Q{idx}: {ans}\n"));
        }
    }
    md.push_str("\n---\n\n");
    md.push_str(llm_body);
    md.push('\n');

    if !snippets.is_empty() {
        md.push_str("\n## Evidence\n\n");
        for (i, s) in snippets.iter().enumerate() {
            md.push_str(&format!(
                "- [{}] `{}` (score {:.2})\n",
                i + 1,
                s.source,
                s.score
            ));
        }
    }
    md
}

fn first_nonempty_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| {
            let without_marker = l.trim_start_matches(|c: char| c == '#' || c.is_whitespace());
            without_marker.to_string()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubRecall {
        snippets: Vec<RecallSnippet>,
    }

    #[async_trait]
    impl ResearchRecall for StubRecall {
        async fn query(&self, _topic: &str, _top_k: usize) -> anyhow::Result<Vec<RecallSnippet>> {
            Ok(self.snippets.clone())
        }
    }

    struct FailingRecall;

    #[async_trait]
    impl ResearchRecall for FailingRecall {
        async fn query(&self, _topic: &str, _top_k: usize) -> anyhow::Result<Vec<RecallSnippet>> {
            Err(anyhow::anyhow!("recall down"))
        }
    }

    struct StubLlm {
        reply: String,
    }

    #[async_trait]
    impl LlmClient for StubLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Ok(self.reply.clone())
        }
    }

    struct FailingLlm;

    #[async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("llm down"))
        }
    }

    #[tokio::test]
    async fn execute_produces_draft_with_snippets_and_sources() {
        let recall = Arc::new(StubRecall {
            snippets: vec![
                RecallSnippet {
                    source: "archive://semantic/oauth-rfc.md".into(),
                    content: "OAuth 2.1 folds PKCE into the core.".into(),
                    score: 0.91,
                },
                RecallSnippet {
                    source: "archive://semantic/oauth-survey.md".into(),
                    content: "The `oauth2` crate is idiomatic in Rust.".into(),
                    score: 0.85,
                },
            ],
        });
        let llm = Arc::new(StubLlm {
            reply: "TL;DR: use the `oauth2` crate with PKCE.\n\n\
                    Rationale: the RFC folds PKCE into OAuth 2.1, and the `oauth2` \
                    crate is idiomatic.\n"
                .to_string(),
        }) as Arc<dyn LlmClient>;

        let agent = ResearcherAgent::new(recall, llm);
        let draft = agent
            .execute(
                ResearchRequest::new("sg-auth-research", "Which OAuth crate for Rust?")
                    .with_answers(vec![(0, "PKCE required".into())]),
            )
            .await
            .expect("research should succeed");

        assert_eq!(draft.sub_goal_id, "sg-auth-research");
        assert!(draft.summary.contains("TL;DR") || draft.summary.contains("oauth2"));
        assert!(draft.markdown.contains("Which OAuth crate"));
        assert!(draft.markdown.contains("operator_answers"));
        assert!(draft.markdown.contains("Evidence"));
        assert_eq!(draft.sources.len(), 2);
        assert!(draft
            .sources
            .contains(&"archive://semantic/oauth-rfc.md".to_string()));
    }

    #[tokio::test]
    async fn execute_handles_empty_recall() {
        let recall = Arc::new(StubRecall { snippets: vec![] });
        let llm = Arc::new(StubLlm {
            reply: "No prior evidence; flagged.".into(),
        }) as Arc<dyn LlmClient>;
        let agent = ResearcherAgent::new(recall, llm);
        let draft = agent
            .execute(ResearchRequest::new("sg-x", "obscure question"))
            .await
            .expect("should still produce a draft");
        assert!(draft.sources.is_empty());
        assert!(draft.markdown.contains("obscure question"));
    }

    #[tokio::test]
    async fn recall_failure_surfaces_as_error() {
        let recall = Arc::new(FailingRecall);
        let llm = Arc::new(StubLlm {
            reply: "unreachable".into(),
        }) as Arc<dyn LlmClient>;
        let agent = ResearcherAgent::new(recall, llm);
        let err = agent
            .execute(ResearchRequest::new("sg-x", "q"))
            .await
            .unwrap_err();
        assert!(matches!(err, ResearcherError::Recall(_)));
    }

    #[tokio::test]
    async fn llm_failure_surfaces_as_error() {
        let recall = Arc::new(StubRecall { snippets: vec![] });
        let llm = Arc::new(FailingLlm) as Arc<dyn LlmClient>;
        let agent = ResearcherAgent::new(recall, llm);
        let err = agent
            .execute(ResearchRequest::new("sg-x", "q"))
            .await
            .unwrap_err();
        assert!(matches!(err, ResearcherError::Llm(_)));
    }

    #[tokio::test]
    async fn empty_llm_response_is_rejected() {
        let recall = Arc::new(StubRecall { snippets: vec![] });
        let llm = Arc::new(StubLlm {
            reply: "   \n".into(),
        }) as Arc<dyn LlmClient>;
        let agent = ResearcherAgent::new(recall, llm);
        let err = agent
            .execute(ResearchRequest::new("sg-x", "q"))
            .await
            .unwrap_err();
        assert!(matches!(err, ResearcherError::EmptyLlmResponse));
    }

    #[test]
    fn first_nonempty_line_strips_markdown_markers() {
        assert_eq!(
            first_nonempty_line("# Title\nbody").as_deref(),
            Some("Title")
        );
        assert_eq!(
            first_nonempty_line("\n\n  TL;DR: use X").as_deref(),
            Some("TL;DR: use X")
        );
        assert_eq!(first_nonempty_line("   \n\n").as_deref(), None);
    }
}
