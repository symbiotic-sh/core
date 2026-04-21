//! Per-agent LLM-call trace tagging.
//!
//! Every `llm.chat(...)` call inside `run_agent_with_config` is wrapped in
//! `TRACE_TAG.scope(Some(tag), ...)`. Provider implementations (Ollama,
//! etc.) read the tag at the point they serialize the request and use it to
//! name the trace files they write — `{dir}/{agent_id}/turn-{N:04}-request.json`
//! instead of a flat `{dir}/NNNN-req.json`.
//!
//! This is the at-source fix that replaces the post-processing organizer:
//! parallel agent dispatches (including the nested orchestrator → sub-agent
//! pattern) never interleave in the trace output, because each agent's
//! turns live under its own agent_id-keyed folder from the start.

use std::sync::Arc;

/// A scoped audit tag attached to every LLM call within one agent's
/// execution context. Populated by the executor; read by provider
/// implementations that write trace side-cars.
#[derive(Debug, Clone)]
pub struct TraceTag {
    pub agent_id: String,
    pub role: String,
    pub iteration: u32,
}

tokio::task_local! {
    /// Current trace tag. `None` outside an agent loop.
    ///
    /// Wrapped in `Arc` so the tag's owned fields don't get cloned on every
    /// `try_with` — agents with long chains of tool calls read this tag a
    /// lot.
    pub static TRACE_TAG: Option<Arc<TraceTag>>;
}

/// Return the current trace tag, if any. Safe to call from any async
/// context; returns `None` outside an agent execution.
pub fn current_tag() -> Option<Arc<TraceTag>> {
    TRACE_TAG.try_with(|t| t.clone()).ok().flatten()
}
