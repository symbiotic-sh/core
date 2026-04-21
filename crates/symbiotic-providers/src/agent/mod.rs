//! Agent task execution providers.
//!
//! - [`ClaudeCodeProvider`] — Claude Code agent provider (subprocess-based).
//! - [`CodexProvider`] — Codex agent provider (subprocess-based).
//!
//! Both providers execute their respective CLI tools as subprocesses, with
//! support for timeout, cancellation, and retry classification.

pub mod claude_code;
pub mod codex;

pub use claude_code::ClaudeCodeProvider;
pub use codex::CodexProvider;

/// Re-export retry classification helpers.
pub use claude_code::{is_retryable, is_terminal};
