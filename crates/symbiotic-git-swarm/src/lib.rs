//! Internal Git Swarm: PR-based agent collaboration with branch protection.
//!
//! This crate implements Symbiotic's internal GitHub — a secure git collaboration
//! layer for agent swarms running in Docker/Sysbox sandboxes.
//!
//! # Architecture
//!
//! - **Git server**: A persistent Alpine+git container managed by the daemon via
//!   bollard. Serves bare repos over HTTP. The daemon never runs `git` directly.
//! - **PR system**: GitHub-style pull requests with reviews, CI checks, and
//!   configurable merge rules. Agents interact via JSON-RPC tools.
//! - **Branch protection**: `pre-receive` hooks in the git container call back to
//!   the daemon for authorization, integrating with `CapabilityToken` scopes.
//!
//! # Modules
//!
//! - [`types`] — Core data types (repos, PRs, reviews, checks, rules)
//! - [`server`] — Git server container lifecycle (bollard)
//! - [`pr`] — PR lifecycle management
//! - [`merge_rules`] — Branch protection + merge rule evaluation

pub mod pr;
pub mod server;
pub mod types;

// Re-export commonly used types
pub use pr::PRManager;
pub use server::GitServerManager;
pub use types::*;
