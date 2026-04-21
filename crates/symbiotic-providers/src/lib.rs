//! Multi-provider AI abstraction with metering, budgets, and sensitivity routing.
//!
//! This crate defines the trait hierarchy and shared types for all AI provider
//! integrations in Symbiotic. Concrete provider implementations live in the
//! submodules ([`completion`], [`embedding`], [`image`], [`video`], [`agent`]).
//!
//! # Architecture
//!
//! Every provider implements [`ModelProvider`] for basic metadata (name, class,
//! model, capabilities, pricing). Specific capabilities are expressed through
//! additional traits:
//!
//! - [`CompletionProvider`] for text completion / chat
//! - [`EmbeddingProvider`] for text embedding generation
//! - [`ImageProvider`] for image generation
//! - [`VideoProvider`] for video generation
//! - [`AgentProvider`] for autonomous agent task execution
//!
//! The [`ProviderClass`] enum drives sensitivity-aware routing: private and
//! restricted content is restricted to [`ProviderClass::Local`] providers.

pub mod auth;
pub mod budget;
pub mod config;
pub mod error;
pub mod metering;
pub mod registry;
pub mod routing;
pub mod traits;
pub mod types;

// Submodule directories for concrete provider implementations.
pub mod agent;
pub mod completion;
pub mod embedding;
pub mod image;
pub mod video;

// Re-export all public types at crate root for ergonomic imports.
pub use auth::{CredentialResolver, EnvVarResolver, ProviderAuth};
pub use budget::BudgetEnforcer;
pub use config::{
    load_providers_config, AuthMethod, BudgetConfig, ProviderBudget, ProviderConfig,
    ProvidersConfig,
};
pub use error::ProviderError;
pub use metering::{
    MeteredCompletionProvider, MeteredEmbeddingProvider, UsageAggregate, UsageFilter, UsageLog,
};
pub use registry::{ProviderRegistry, RegisteredProvider};
pub use routing::{HealthChecker, ProviderRouter, RetryConfig};
pub use traits::{
    AgentProvider, CompletionProvider, EmbeddingProvider, ImageProvider, ModelProvider,
    VideoProvider,
};
pub use types::{
    CapabilitySet, ChatMessage, CompletionRequest, CompletionResponse, EmbedResult, GeneratedImage,
    GeneratedVideo, ImageRequest, ImageResponse, ModelHint, PricingInfo, ProviderCapability,
    ProviderClass, RequestType, Role, TaskArtifact, TaskRequest, TaskResult, TaskSession,
    TaskStatus, UsageRecord, VideoRequest, VideoResponse,
};

// Re-export concrete provider implementations.
pub use agent::{ClaudeCodeProvider, CodexProvider};
pub use completion::{
    AnthropicProvider, ClaudeCodeCompletionProvider, CodexCompletionProvider,
    GenericOpenAiCompatProvider, OllamaCompletionProvider, OpenAiCompletionProvider,
};
pub use embedding::{OllamaEmbeddingProvider, OpenAiEmbeddingProvider};
pub use image::{OpenAiImageProvider, StubImageProvider};
pub use video::StubVideoProvider;
