//! Shared declarative types for Symbiotic subsystems.
//!
//! Types living here are wire-level data carriers — no behaviour. They are
//! designed to be consumed by the daemon, the control plane, and the client
//! without any of them taking a dependency on the others.

pub mod question_group;

pub use question_group::*;
