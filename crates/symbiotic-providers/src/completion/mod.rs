//! Completion provider implementations.
//!
//! Includes:
//!
//! - [`OllamaCompletionProvider`] — local inference via Ollama HTTP API
//! - [`OpenAiCompletionProvider`] — OpenAI chat completions API
//! - [`AnthropicProvider`] — Anthropic Claude Messages API
//! - [`GenericOpenAiCompatProvider`] — covers any provider that speaks the
//!   OpenAI `/chat/completions` wire format (OpenRouter, Venice, Together,
//!   Groq, self-hosted vLLM, etc.).
//! - [`ClaudeCodeCompletionProvider`] — Claude Code CLI (`claude -p`) for
//!   subscription-based completions (**local dev only** — TOS prohibits VPS use).
//! - [`CodexCompletionProvider`] — OpenAI Codex CLI (`codex exec`) for
//!   subscription-based completions (Apache 2.0, daemon-safe).
//!
//! The [`GenericOpenAiCompatProvider`] also has factory methods for Google
//! Gemini (`gemini()`) and other providers with OpenAI-compatible endpoints.

pub mod anthropic;
pub mod claude_code;
pub mod codex;
pub mod ollama;
pub mod openai;
pub mod openai_compat;

pub use anthropic::AnthropicProvider;
pub use claude_code::ClaudeCodeCompletionProvider;
pub use codex::CodexCompletionProvider;
pub use ollama::OllamaCompletionProvider;
pub use openai::OpenAiCompletionProvider;
pub use openai_compat::GenericOpenAiCompatProvider;
