//! Base types shared across all AI providers.
//!
//! Contains request/response types for completions, embeddings, image generation,
//! video generation, and agent task execution. Also includes provider metadata
//! types like [`ProviderClass`], [`ProviderCapability`], and [`PricingInfo`].

use serde::{Deserialize, Serialize};

/// Classification of where a provider runs.
///
/// Used by the sensitivity router to enforce data locality rules:
/// private/restricted content must stay on [`ProviderClass::Local`] providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderClass {
    /// Runs on local hardware (e.g. Ollama, llama.cpp).
    Local,
    /// Runs on a remote cloud API (e.g. OpenAI, Anthropic).
    Cloud,
    /// Proxies to multiple upstream providers (e.g. OpenRouter, Venice).
    Aggregator,
}

/// A specific capability that a provider may support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCapability {
    /// Text completion / chat.
    Completion,
    /// Text embedding generation.
    Embedding,
    /// Function / tool calling.
    FunctionCall,
    /// Image understanding (multimodal input).
    Vision,
    /// Image generation from text prompts.
    ImageGeneration,
    /// Video generation from text prompts.
    VideoGeneration,
    /// Autonomous agent task execution.
    AgentExecution,
}

/// A set of capabilities advertised by a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilitySet(pub Vec<ProviderCapability>);

impl CapabilitySet {
    /// Create a new capability set from the given capabilities.
    pub fn new(capabilities: Vec<ProviderCapability>) -> Self {
        Self(capabilities)
    }

    /// Check whether the set contains a specific capability.
    pub fn has(&self, capability: ProviderCapability) -> bool {
        self.0.contains(&capability)
    }

    /// Add a capability to the set (no-op if already present).
    pub fn add(&mut self, capability: ProviderCapability) {
        if !self.has(capability) {
            self.0.push(capability);
        }
    }
}

/// Pricing information for a provider's model.
///
/// All costs are in USD. Fields are optional because not every provider
/// charges for every dimension (e.g. a text-only model has no `per_image`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PricingInfo {
    /// Cost per 1,000 input tokens.
    pub input_per_1k_tokens: Option<f64>,
    /// Cost per 1,000 output tokens.
    pub output_per_1k_tokens: Option<f64>,
    /// Cost per generated image.
    pub per_image: Option<f64>,
    /// Cost per second of generated video.
    pub per_video_second: Option<f64>,
    /// Cost per 1,000 embedding tokens.
    pub embedding_per_1k_tokens: Option<f64>,
    /// Cost per agent task submission.
    pub per_agent_task: Option<f64>,
}

// ---------------------------------------------------------------------------
// Model hint types
// ---------------------------------------------------------------------------

/// Hint for model selection — lets callers express cost/quality preference
/// without hardcoding specific model names.
///
/// The router uses this hint to re-order candidates after all filtering
/// (capability, sensitivity, health, budget) is complete. If no candidate
/// matches the hint, the router falls back to the default ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ModelHint {
    /// Use the default model (no preference).
    #[default]
    Default,
    /// Prefer the cheapest/fastest model available (e.g., Haiku, Flash, GPT-4o-mini).
    CheapFast,
    /// Prefer the most capable model available (e.g., Sonnet, Opus, GPT-4o).
    MostCapable,
}

impl ModelHint {
    /// Returns `true` if the given model name matches this hint's preference.
    ///
    /// Matching is case-insensitive and looks for characteristic substrings
    /// in the model name. `Default` matches nothing (no preference).
    pub fn matches_model_name(&self, model_name: &str) -> bool {
        let lower = model_name.to_ascii_lowercase();
        match self {
            ModelHint::Default => false,
            ModelHint::CheapFast => {
                lower.contains("haiku")
                    || lower.contains("flash")
                    || lower.contains("mini")
                    || lower.contains("nano")
            }
            ModelHint::MostCapable => {
                lower.contains("opus")
                    || lower.contains("sonnet")
                    || (lower.contains("gpt-4o") && !lower.contains("mini"))
                    || lower.contains("gpt-5")
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Completion types
// ---------------------------------------------------------------------------

/// Role of a participant in a chat conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// System prompt / instruction.
    System,
    /// Human user message.
    User,
    /// AI assistant response.
    Assistant,
}

/// A single message in a chat conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    /// Who sent this message.
    pub role: Role,
    /// The text content of the message.
    pub content: String,
}

/// Request for a text completion / chat response.
#[derive(Debug, Clone)]
pub struct CompletionRequest {
    /// The conversation history to complete.
    pub messages: Vec<ChatMessage>,
    /// Maximum number of tokens to generate.
    pub max_tokens: Option<u32>,
    /// Sampling temperature (0.0 = deterministic, higher = more random).
    pub temperature: Option<f32>,
    /// Stop sequences that terminate generation.
    pub stop: Option<Vec<String>>,
    /// Hint for model selection — lets the caller express a preference for
    /// cheap/fast vs. most-capable models without hardcoding model names.
    /// Defaults to [`ModelHint::Default`] (no preference).
    pub model_hint: ModelHint,
}

/// Response from a text completion provider.
#[derive(Debug, Clone)]
pub struct CompletionResponse {
    /// The generated text content.
    pub content: String,
    /// The model that produced this response.
    pub model: String,
    /// Number of input tokens consumed.
    pub input_tokens: Option<u64>,
    /// Number of output tokens generated.
    pub output_tokens: Option<u64>,
    /// Reason the model stopped generating (e.g. "stop", "length").
    pub finish_reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Embedding types
// ---------------------------------------------------------------------------

/// Result of embedding a single text input.
#[derive(Debug, Clone)]
pub struct EmbedResult {
    /// The embedding vector.
    pub embedding: Vec<f32>,
    /// The model that produced this embedding.
    pub model_name: String,
    /// Dimensionality of the embedding vector.
    pub dimensions: usize,
}

// ---------------------------------------------------------------------------
// Image generation types
// ---------------------------------------------------------------------------

/// Request for image generation.
#[derive(Debug, Clone)]
pub struct ImageRequest {
    /// Text prompt describing the desired image.
    pub prompt: String,
    /// Desired image width in pixels.
    pub width: Option<u32>,
    /// Desired image height in pixels.
    pub height: Option<u32>,
    /// Number of images to generate.
    pub count: Option<u32>,
}

/// Response from an image generation provider.
#[derive(Debug, Clone)]
pub struct ImageResponse {
    /// The generated images.
    pub images: Vec<GeneratedImage>,
    /// The model that produced these images.
    pub model: String,
}

/// A generated image, either as raw bytes or a URL.
#[derive(Debug, Clone)]
pub enum GeneratedImage {
    /// Image data returned inline.
    Bytes {
        /// Raw image bytes.
        data: Vec<u8>,
        /// Image format (e.g. "png", "jpeg").
        format: String,
    },
    /// URL where the image can be downloaded.
    Url(String),
}

// ---------------------------------------------------------------------------
// Video generation types
// ---------------------------------------------------------------------------

/// Request for video generation.
#[derive(Debug, Clone)]
pub struct VideoRequest {
    /// Text prompt describing the desired video.
    pub prompt: String,
    /// Desired video duration in seconds.
    pub duration_seconds: Option<f32>,
    /// Desired video width in pixels.
    pub width: Option<u32>,
    /// Desired video height in pixels.
    pub height: Option<u32>,
}

/// Response from a video generation provider.
#[derive(Debug, Clone)]
pub struct VideoResponse {
    /// The generated video.
    pub video: GeneratedVideo,
    /// The model that produced this video.
    pub model: String,
    /// Actual duration of the generated video in seconds.
    pub duration_seconds: Option<f32>,
}

/// A generated video, either as raw bytes or a URL.
#[derive(Debug, Clone)]
pub enum GeneratedVideo {
    /// Video data returned inline.
    Bytes {
        /// Raw video bytes.
        data: Vec<u8>,
        /// Video format (e.g. "mp4", "webm").
        format: String,
    },
    /// URL where the video can be downloaded.
    Url(String),
}

// ---------------------------------------------------------------------------
// Agent task types
// ---------------------------------------------------------------------------

/// Request to submit an autonomous agent task.
#[derive(Debug, Clone)]
pub struct TaskRequest {
    /// Natural-language description of the task to perform.
    pub task: String,
    /// Optional system prompt to guide agent behavior.
    pub system_prompt: Option<String>,
    /// Working directory for the agent to operate in.
    pub working_directory: Option<String>,
    /// Maximum execution time before the task is cancelled.
    pub timeout_seconds: Option<u64>,
    /// Paths to files the agent should have access to.
    pub context_files: Vec<String>,
}

/// A running or completed agent task session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSession {
    /// Unique identifier for this task session.
    pub session_id: String,
    /// Name of the provider executing the task.
    pub provider: String,
    /// Current status of the task.
    pub status: TaskStatus,
    /// Unix timestamp when the task was submitted.
    pub submitted_at: u64,
}

/// Status of an agent task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is waiting in the queue.
    Queued,
    /// Task is currently executing.
    Running,
    /// Task finished successfully.
    Completed,
    /// Task failed with an error.
    Failed,
    /// Task was cancelled by the user.
    Cancelled,
    /// Task exceeded its timeout.
    TimedOut,
}

/// Result of a completed agent task.
#[derive(Debug, Clone)]
pub struct TaskResult {
    /// Session ID this result belongs to.
    pub session_id: String,
    /// Final status of the task.
    pub status: TaskStatus,
    /// Text output produced by the agent.
    pub output: String,
    /// Files created or modified during execution.
    pub artifacts: Vec<TaskArtifact>,
    /// Total input tokens consumed across all LLM calls.
    pub total_input_tokens: Option<u64>,
    /// Total output tokens generated across all LLM calls.
    pub total_output_tokens: Option<u64>,
    /// Estimated total cost in USD.
    pub cost_usd: Option<f64>,
}

/// A file artifact produced by an agent task.
#[derive(Debug, Clone)]
pub struct TaskArtifact {
    /// Path of the artifact relative to the working directory.
    pub path: String,
    /// Text content of the artifact (if available).
    pub content: Option<String>,
    /// Whether this file was created (true) or modified (false).
    pub created: bool,
}

// ---------------------------------------------------------------------------
// Usage tracking types
// ---------------------------------------------------------------------------

/// Type of request for usage tracking and billing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestType {
    /// Text completion / chat.
    Completion,
    /// Text embedding.
    Embedding,
    /// Image generation.
    ImageGeneration,
    /// Video generation.
    VideoGeneration,
    /// Agent task execution.
    AgentTask,
}

/// A single usage record for metering and billing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Name of the provider that served the request.
    pub provider: String,
    /// Model identifier used.
    pub model: String,
    /// Unix timestamp of the request.
    pub timestamp: u64,
    /// Number of input tokens consumed.
    pub input_tokens: u64,
    /// Number of output tokens generated.
    pub output_tokens: u64,
    /// Number of media units (images, video seconds, etc.).
    pub media_units: u64,
    /// Estimated cost in USD (if pricing info is available).
    pub cost_usd: Option<f64>,
    /// What kind of request this was.
    pub request_type: RequestType,
    /// Source that triggered this request (e.g. "daemon", "cli", "agent").
    pub source: String,
    /// Session ID for agent tasks (links to [`TaskSession`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}
