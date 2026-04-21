//! Agent role definitions, versioned prompts, and role registry.
//!
//! This crate provides the configuration layer that sits between the workflow/goal
//! system and the AI provider layer (T101). It manages:
//!
//! - **Agent role definitions** — TOML-based personas (researcher, coder, reviewer, etc.)
//! - **Versioned prompts** — System prompt history with activation and rollback
//! - **Role registry** — Centralized lookup and resolution of roles at runtime
//! - **Default roles** — Built-in role definitions for common agent types
//!
//! # Architecture
//!
//! ```text
//! Workflows / Goals
//!     ↓ "use researcher agent with v3 prompt"
//! Agent Config (this crate)      ← role definitions, versioned prompts
//!     ↓ "here's the system prompt + context"
//! Provider Layer (T101)          ← execution infrastructure
//!     ↓ "call Claude with these messages"
//! Cloud/Local API
//! ```
//!
//! # Example
//!
//! ```rust
//! use symbiotic_agent_config::{RoleRegistry, defaults};
//!
//! let mut registry = RoleRegistry::new();
//! defaults::register_defaults(&mut registry).unwrap();
//!
//! let resolved = registry.resolve("researcher").unwrap();
//! assert_eq!(resolved.name, "researcher");
//! assert!(!resolved.system_prompt.is_empty());
//! ```

pub mod defaults;
pub mod error;
pub mod registry;
pub mod types;
pub mod versioning;

pub use error::AgentConfigError;
pub use registry::RoleRegistry;
pub use types::{AgentRole, PromptVersion, ResolvedRole};
pub use versioning::{
    activate_version, add_version, load_role, rollback, save_role, version_history,
};
