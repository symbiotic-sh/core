//! Structural sanitization primitives used by Stage A.
//!
//! These helpers are deterministic, side-effect free, and allocate only when
//! they rewrite content. Each returns the cleaned payload plus a list of
//! [`StageFinding`][crate::types::StageFinding]s describing what was changed.
//!
//! Stage A (`crate::stages::stage_a`) orchestrates these into a single pass.

pub mod html;
pub mod markdown;

pub use html::{sanitize_html, HtmlSanitizeReport};
pub use markdown::{sanitize_markdown, MarkdownSanitizeReport};
