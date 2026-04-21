//! Provider traits defining the interface for each AI capability.
//!
//! All providers implement [`ModelProvider`] for basic metadata. Specific
//! capabilities are expressed through additional traits:
//!
//! - [`CompletionProvider`] — text completion / chat
//! - [`EmbeddingProvider`] — text embedding generation
//! - [`ImageProvider`] — image generation
//! - [`VideoProvider`] — video generation
//! - [`AgentProvider`] — autonomous agent task execution

use async_trait::async_trait;

use crate::{
    CapabilitySet, CompletionRequest, CompletionResponse, EmbedResult, ImageRequest, ImageResponse,
    PricingInfo, ProviderClass, ProviderError, TaskRequest, TaskResult, TaskSession, TaskStatus,
    VideoRequest, VideoResponse,
};

/// Base trait for all AI model providers.
///
/// Every provider must declare its name, class, model, capabilities,
/// and optionally its pricing information.
pub trait ModelProvider: Send + Sync {
    /// Human-readable name of this provider (e.g. "ollama", "openai").
    fn name(&self) -> &str;

    /// Whether this provider runs locally, in the cloud, or is an aggregator.
    fn provider_class(&self) -> ProviderClass;

    /// The specific model identifier (e.g. "gpt-4o", "llama3.1:8b").
    fn model_name(&self) -> &str;

    /// The set of capabilities this provider supports.
    fn capabilities(&self) -> &CapabilitySet;

    /// Pricing information, if available.
    fn pricing(&self) -> Option<&PricingInfo>;
}

/// Provider that can generate text completions from a conversation.
#[async_trait]
pub trait CompletionProvider: ModelProvider {
    /// Generate a completion for the given conversation.
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;
}

/// Provider that can generate vector embeddings from text.
#[async_trait]
pub trait EmbeddingProvider: ModelProvider {
    /// Embed a single text input.
    async fn embed(&self, text: &str) -> Result<EmbedResult, ProviderError>;

    /// Embed multiple texts. Default implementation calls [`Self::embed`] sequentially.
    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbedResult>, ProviderError> {
        let mut results = Vec::with_capacity(texts.len());
        for text in texts {
            results.push(self.embed(text).await?);
        }
        Ok(results)
    }
}

/// Provider that can generate images from text prompts.
#[async_trait]
pub trait ImageProvider: ModelProvider {
    /// Generate one or more images from the request.
    async fn generate_image(&self, request: &ImageRequest) -> Result<ImageResponse, ProviderError>;
}

/// Provider that can generate videos from text prompts.
#[async_trait]
pub trait VideoProvider: ModelProvider {
    /// Generate a video from the request.
    async fn generate_video(&self, request: &VideoRequest) -> Result<VideoResponse, ProviderError>;
}

/// Provider that can execute autonomous agent tasks.
#[async_trait]
pub trait AgentProvider: ModelProvider {
    /// Submit a new task for execution.
    async fn submit_task(&self, request: &TaskRequest) -> Result<TaskSession, ProviderError>;

    /// Poll the current status of a running task.
    async fn poll_status(&self, session_id: &str) -> Result<TaskStatus, ProviderError>;

    /// Retrieve the full result of a completed task.
    async fn get_result(&self, session_id: &str) -> Result<TaskResult, ProviderError>;

    /// Cancel a running task.
    async fn cancel(&self, session_id: &str) -> Result<(), ProviderError>;
}
