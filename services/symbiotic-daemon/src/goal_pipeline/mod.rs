//! Goal-pipeline glue: modules that bridge goal lifecycle events to agent
//! executions and the event bus.
//!
//! This module was introduced by T130 §03 (Inquisitor batch emit) to host
//! [`inquisitor_adapter`]. T130 §04 (goal DAG + question resolver) added
//! [`question_resolver`] alongside it.

pub mod inquisitor_adapter;
pub mod question_resolver;
