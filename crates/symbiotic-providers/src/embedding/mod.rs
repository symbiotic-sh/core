//! Embedding provider implementations.
//!
//! - [`OllamaEmbeddingProvider`] — local embedding generation via Ollama HTTP API.
//! - [`OpenAiEmbeddingProvider`] — cloud embedding generation via the OpenAI API.

pub mod ollama;
pub mod openai;

pub use ollama::OllamaEmbeddingProvider;
pub use openai::OpenAiEmbeddingProvider;
