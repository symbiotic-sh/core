//! Skills system for Symbiotic agents.
//!
//! Provides reusable, versioned prompt workflows with TOML manifests,
//! trust gating, auto-load detection, validation hooks, and dynamic
//! skill synthesis (code generation + sandbox compilation + archival).
//!
//! ## Modules
//!
//! - [`manifest`] — TOML manifest parsing and validation
//! - [`registry`] — Skill loading, auto-detection, trust gating
//! - [`synthesis`] — Pipeline orchestration (generate → test → compile → archive)
//! - [`codegen`] — Code generation (LLM-backed or stub)
//! - [`docker`] — Docker-backed sandbox compiler
//! - [`tool`] — Agent tool wrapper for synthesis
//! - [`validation`] — Output validation hooks

pub mod codegen;
pub mod detector;
pub mod docker;
pub mod manifest;
pub mod registry;
pub mod synthesis;
pub mod tool;
pub mod validation;
