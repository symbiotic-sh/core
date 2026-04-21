//! Image generation providers.
//!
//! - [`OpenAiImageProvider`] — DALL-E 3 via the OpenAI API (fully implemented).
//! - [`StubImageProvider`] — placeholder for Flux, Stable Diffusion, nanobanana, Midjourney.

pub mod openai;
pub mod stub;

pub use openai::OpenAiImageProvider;
pub use stub::StubImageProvider;
