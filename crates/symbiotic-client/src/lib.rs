//! Platform-agnostic client library for Symbiotic.
//!
//! Provides event parsing and command building for any client (mobile, desktop,
//! web/WASM) communicating with a Symbiotic daemon over Matrix transport.
//!
//! # Architecture
//!
//! ```text
//! symbiotic-core      (protocol types: Kind, Status, EventPayload, CommandPayload)
//!     └── symbiotic-client   (this crate: parse events, build commands)
//!             ├── symbiotic-mobile-core  (FFI for Flutter via flutter_rust_bridge)
//!             └── wasm module            (wasm-bindgen for web)
//! ```
//!
//! # Modules
//!
//! - [`parser`] — deserialize `sym.e` envelopes into typed events
//! - [`commands`] — build `sym.c` envelopes for daemon commands

pub mod commands;
pub mod parser;

// Re-export protocol types so consumers don't need to depend on symbiotic-core directly.
pub use symbiotic_core::protocol::{CommandPayload, EventPayload, Kind, Status};
