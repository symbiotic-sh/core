//! Runtime configuration helpers for the daemon.
//!
//! Currently exports [`feature_flags`] — the env-backed flag reader used by
//! goal-pipeline wiring. Expect this module to grow into the home for all
//! cross-subsystem runtime configuration.

pub mod feature_flags;
