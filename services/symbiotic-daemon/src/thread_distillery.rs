//! Thread Distillery Handler — processes thread conversations through the
//! Distillery pipeline to extract structured knowledge.
//!
//! This is T109 Phase 1: the daemon-side handler that:
//! 1. Receives thread messages (from Matrix transport or queue job)
//! 2. Converts them to `RawInput` via `ThreadConversationAdapter`
//! 3. Runs the Distillery pipeline (Reduce -> Reflect -> Verify -> Reweave -> Archive)
//! 4. Returns a `DistilleryReport` with extraction results
//!
//! The handler is wired into the daemon's job dispatch as `thread.distillery`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use symbiotic_intake::distillery::{
    build_graph_context_all_spaces, run_pipeline, DistilleryReport, DistilleryStageConfig,
};
use symbiotic_intake::thread_adapter::ThreadConversationAdapter;
use symbiotic_memory::staleness::StalenessChecker;
use symbiotic_memory::types::Memory;
use symbiotic_providers::ProviderRouter;
use symbiotic_queue::QueueJob;

use crate::events::{DaemonEvent, EventType};
use crate::memory_docs::format_staleness_warnings;
use crate::SymbioticDaemon;

/// Default extraction cooldown: 4 hours in seconds.
const DEFAULT_EXTRACTION_COOLDOWN_SECS: u64 = 4 * 60 * 60;

/// Token threshold: if new content since last extraction exceeds this, extract
/// regardless of the time cooldown. ~4 chars per token heuristic.
/// 8000 tokens ≈ 60 messages — a substantive conversation, not casual chatter.
const DEFAULT_NEW_TOKEN_THRESHOLD: usize = 8000;

// ---------------------------------------------------------------------------
// Payload codec
// ---------------------------------------------------------------------------

/// Payload for a `thread.distillery` queue job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ThreadDistilleryPayload {
    pub thread_id: String,
    pub thread_title: String,
    /// Chronologically ordered messages: `(sender, body, timestamp)`.
    pub messages: Vec<(String, String, String)>,
    /// If `true`, skip cooldown checks and force extraction.
    #[serde(default)]
    pub force: bool,
}

/// Encode a thread distillery payload to a JSON string for queue storage.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn encode_thread_distillery_payload(payload: &ThreadDistilleryPayload) -> String {
    serde_json::to_string(payload).expect("ThreadDistilleryPayload must be serializable")
}

/// Decode a thread distillery payload from a queue job's payload string.
pub(crate) fn decode_thread_distillery_payload(raw: &str) -> Result<ThreadDistilleryPayload> {
    serde_json::from_str(raw).context("invalid thread.distillery payload JSON")
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// Handles thread-to-Distillery processing.
///
/// Called when a `thread.distillery` job is dequeued or when the daemon
/// detects a thread is due for knowledge extraction.
pub struct ThreadDistilleryHandler;

impl ThreadDistilleryHandler {
    /// Process a thread through the Distillery pipeline.
    ///
    /// Takes only the `Send+Sync` fields needed from the daemon to avoid
    /// threading issues (SymbioticDaemon is not `Send`).
    ///
    /// # Arguments
    ///
    /// * `provider_router` - Shared provider router for LLM completion
    /// * `archive_root` - Path to the archive/knowledge-base directory
    /// * `thread_id` - The thread to process
    /// * `thread_title` - Human-readable title of the thread
    /// * `messages` - Chronologically ordered `(sender, body, timestamp)` tuples
    ///
    /// # Returns
    ///
    /// A `DistilleryReport` summarizing what was extracted, or an error if the
    /// pipeline failed (e.g. no LLM provider available).
    pub async fn process_thread(
        provider_router: Arc<ProviderRouter>,
        archive_root: PathBuf,
        thread_id: &str,
        thread_title: &str,
        messages: &[(String, String, String)],
    ) -> Result<DistilleryReport> {
        if messages.is_empty() {
            anyhow::bail!("thread {thread_id} has no messages to distill");
        }

        // Step 1: Convert thread messages to RawInput
        let raw_input =
            ThreadConversationAdapter::messages_to_raw_input(thread_id, thread_title, messages);

        // Step 2: Build graph context from the Archive (all memory spaces)
        let config = DistilleryStageConfig {
            kb_root: archive_root,
            redact_llm_prompts: true,
        };
        let graph_context = build_graph_context_all_spaces(&config.kb_root)
            .context("failed to build graph context for thread distillery")?;

        // Step 3: Get an LLM client via the ProviderRouter.
        // Use CheapFast hint — extraction is high-volume, low-stakes work
        // that Haiku/Flash/Mini models handle well at a fraction of the cost.
        use crate::agents::ProviderRouterLlmClient;
        use symbiotic_core::Sensitivity;
        use symbiotic_providers::ModelHint;

        let llm = ProviderRouterLlmClient::with_hint(
            provider_router,
            Sensitivity::Shareable,
            format!("thread_distillery:{thread_id}"),
            ModelHint::CheapFast,
        );

        // Step 4: Run the Distillery pipeline
        let report = run_pipeline(raw_input, &graph_context, &config, &llm)
            .await
            .context("distillery pipeline failed for thread")?;

        log::info!(
            "thread_distillery: thread={thread_id} claims_extracted={} claims_verified={} \
             links_proposed={} notes_rewritten={}",
            report.claims_extracted,
            report.claims_verified,
            report.links_proposed,
            report.notes_rewritten,
        );

        Ok(report)
    }

    /// Check if a thread is due for extraction.
    ///
    /// Returns `true` if any of:
    /// - `force` is `true` (manual trigger)
    /// - `last_extracted` is `None` (never extracted before)
    /// - More than 4 hours have elapsed since `last_extracted`
    /// - New content since last extraction exceeds the token threshold
    ///
    /// # Arguments
    ///
    /// * `last_extracted` - Unix timestamp of the last extraction, or `None` if never extracted
    /// * `now` - Current unix timestamp
    /// * `force` - If `true`, always returns `true` (manual trigger or goal completion)
    /// * `new_content_chars` - Total characters of new messages since last extraction
    pub fn should_extract(
        last_extracted: Option<u64>,
        now: u64,
        force: bool,
        new_content_chars: usize,
    ) -> bool {
        if force {
            return true;
        }

        // Token threshold: ~4 chars per token. If enough new content
        // accumulated, extract regardless of time cooldown.
        let estimated_tokens = new_content_chars / 4;
        if estimated_tokens >= DEFAULT_NEW_TOKEN_THRESHOLD {
            return true;
        }

        match last_extracted {
            None => true,
            Some(ts) => now.saturating_sub(ts) >= DEFAULT_EXTRACTION_COOLDOWN_SECS,
        }
    }
}

// ---------------------------------------------------------------------------
// Staleness scan after extraction
// ---------------------------------------------------------------------------

/// Run a staleness scan on the given facts and return a `DaemonEvent` if
/// any stale or suspect facts are found.
///
/// Returns `None` if all facts are fresh.
#[allow(dead_code)]
pub(crate) fn staleness_event_for_thread(thread_id: &str, facts: &[Memory]) -> Option<DaemonEvent> {
    if facts.is_empty() {
        return None;
    }

    let checker = StalenessChecker::default();
    let warnings = format_staleness_warnings(facts, &checker)?;

    // Count the number of warning bullets (lines starting with "- **")
    let count = warnings.lines().filter(|l| l.starts_with("- **")).count();

    Some(DaemonEvent {
        event_type: EventType::MemoryStaleness,
        status: "completed".to_string(),
        job_id: None,
        detail: format!("{count} stale/suspect fact(s) detected in thread {thread_id}"),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: Some(thread_id.to_string()),
    })
}

// ---------------------------------------------------------------------------
// Daemon integration: execute_thread_distillery_job
// ---------------------------------------------------------------------------

impl SymbioticDaemon {
    /// Execute a `thread.distillery` queue job.
    ///
    /// Decodes the payload, runs the async Distillery pipeline via a scoped
    /// thread (matching the existing pattern for async-from-sync in the daemon),
    /// and returns:
    /// - The primary `DaemonEvent` (distillery result)
    /// - A `Vec<DaemonEvent>` of additional events (staleness warnings,
    ///   auto-promotion proposals)
    pub(crate) fn execute_thread_distillery_job(
        &self,
        job: QueueJob,
        now: u64,
    ) -> Result<(DaemonEvent, Vec<DaemonEvent>)> {
        let payload = decode_thread_distillery_payload(&job.payload)?;

        // Extract Send+Sync fields from the daemon before spawning a scoped thread.
        // SymbioticDaemon is !Send due to non-Send fields (Box<dyn ValidateCredential>,
        // TrustStore with RefCell), so we clone only the Arc fields needed by the pipeline.
        let provider_router = Arc::clone(&self.provider_router);
        let archive_root = self.config.archive_root.clone();
        let thread_id = payload.thread_id.clone();
        let thread_title = payload.thread_title.clone();
        let messages = payload.messages.clone();

        // Bridge async Distillery pipeline from sync job context via scoped thread.
        let report_result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(move || {
                    handle.block_on(ThreadDistilleryHandler::process_thread(
                        provider_router,
                        archive_root,
                        &thread_id,
                        &thread_title,
                        &messages,
                    ))
                })
                .join()
                .expect("thread distillery thread should not panic")
            }),
            Err(_) => {
                anyhow::bail!("thread.distillery requires a tokio runtime");
            }
        };

        match report_result {
            Ok(report) => {
                self.queue.ack(&job.job_id, &self.config.worker_id, now)?;

                let mut extra_events = Vec::new();

                // --- Auto-promotion analysis ---
                // Check if the thread qualifies for goal promotion based on
                // the distillery report. This runs a cheap pre-filter first;
                // only calls the LLM if the pre-filter passes.
                // Check if this thread already has an active goal by scanning
                // the goal state file for a matching goal_room with an active
                // status.  Goal states are keyed by goal_room (which may equal
                // the thread_id when the thread IS the goal room).  If the file
                // cannot be read we default to false (allow promotion).
                let has_active_goal =
                    crate::goal_state::load_goal_states(&self.config.goal_state_file)
                        .map(|states| {
                            states.iter().any(|gs| {
                                gs.goal_room == payload.thread_id
                                    && matches!(
                                        gs.status.as_str(),
                                        "starting" | "running" | "paused"
                                    )
                            })
                        })
                        .unwrap_or(false);
                let pre_filter_result = {
                    let tracker = self
                        .promotion_cooldown_tracker
                        .lock()
                        .expect("cooldown tracker lock poisoned");
                    crate::auto_promotion::pre_filter_with_cooldown(
                        &payload.thread_id,
                        payload.messages.len(),
                        &report.claims_by_space,
                        has_active_goal,
                        Some(&tracker),
                        now,
                    )
                };
                if pre_filter_result != crate::auto_promotion::PreFilterResult::Pass {
                    log::debug!(
                        "auto_promotion: pre-filter rejected (reason={:?}, thread={})",
                        pre_filter_result,
                        payload.thread_id,
                    );
                }
                if pre_filter_result == crate::auto_promotion::PreFilterResult::Pass {
                    // Pre-filter passed — run async LLM analysis via scoped thread
                    let promo_router = Arc::clone(&self.provider_router);
                    let promo_thread_id = payload.thread_id.clone();
                    let promo_thread_title = payload.thread_title.clone();
                    let promo_messages = payload.messages.clone();

                    // Build fact summaries from the report for the LLM prompt.
                    // We don't have individual facts, so summarize from counts.
                    let fact_summaries: Vec<String> = report
                        .claims_by_space
                        .iter()
                        .map(|(space, count)| format!("{count} {space:?} claim(s) extracted"))
                        .collect();

                    let promo_result = match tokio::runtime::Handle::try_current() {
                        Ok(handle) => std::thread::scope(|s| {
                            s.spawn(move || {
                                use crate::agents::ProviderRouterLlmClient;
                                use symbiotic_core::Sensitivity;

                                let llm = ProviderRouterLlmClient::new(
                                    promo_router,
                                    Sensitivity::Shareable,
                                    format!("auto_promotion:{promo_thread_id}"),
                                );
                                handle.block_on(crate::auto_promotion::analyze_for_promotion(
                                    &llm,
                                    &promo_thread_title,
                                    &promo_messages,
                                    &fact_summaries,
                                ))
                            })
                            .join()
                            .expect("auto-promotion thread should not panic")
                        }),
                        Err(_) => {
                            log::warn!("auto_promotion: no tokio runtime, skipping LLM analysis");
                            Err(anyhow::anyhow!("no tokio runtime for auto-promotion"))
                        }
                    };

                    match promo_result {
                        Ok(analysis) => {
                            if analysis.should_promote
                                && analysis.confidence
                                    >= crate::auto_promotion::DEFAULT_MIN_CONFIDENCE
                            {
                                let proposal = crate::auto_promotion::build_promotion_proposal(
                                    &payload.thread_id,
                                    &analysis,
                                );
                                log::info!(
                                    "auto_promotion: proposing promotion for thread={} \
                                     title={:?} confidence={:.2}",
                                    payload.thread_id,
                                    analysis.suggested_title,
                                    analysis.confidence,
                                );
                                extra_events.push(proposal);
                            } else {
                                log::debug!(
                                    "auto_promotion: LLM declined (should_promote={}, confidence={:.2})",
                                    analysis.should_promote,
                                    analysis.confidence,
                                );
                            }
                        }
                        Err(err) => {
                            log::warn!(
                                "auto_promotion: LLM analysis failed for thread={}: {err}",
                                payload.thread_id,
                            );
                        }
                    }
                }

                let primary_event = DaemonEvent {
                    event_type: EventType::ThreadDistillery,
                    status: "completed".to_string(),
                    job_id: Some(job.job_id),
                    detail: format!(
                        "thread={} claims_extracted={} claims_verified={} notes_rewritten={}",
                        payload.thread_id,
                        report.claims_extracted,
                        report.claims_verified,
                        report.notes_rewritten,
                    ),
                    goal_room: None,
                    goal_template: None,
                    goal_run_id: None,
                    goal_id: None,
                    intake_run_id: None,
                    url: None,
                    title: Some(payload.thread_title),
                    sensitivity: None,
                    quick_replies: None,
                    thread_id: Some(payload.thread_id),
                };

                Ok((primary_event, extra_events))
            }
            Err(err) => Err(err.context(format!(
                "thread.distillery failed for thread {}",
                payload.thread_id
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- should_extract tests ---

    #[test]
    fn should_extract_when_forced() {
        assert!(ThreadDistilleryHandler::should_extract(
            Some(1000),
            1001,
            true,
            0,
        ));
    }

    #[test]
    fn should_extract_when_never_extracted() {
        assert!(ThreadDistilleryHandler::should_extract(
            None, 1000, false, 0
        ));
    }

    #[test]
    fn should_not_extract_when_recently_extracted() {
        let now = 50_000;
        let last = now - 3600; // 1 hour ago — within 4h cooldown
        assert!(!ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            0,
        ));
    }

    #[test]
    fn should_extract_when_cooldown_elapsed() {
        let now = 100_000;
        let last = now - DEFAULT_EXTRACTION_COOLDOWN_SECS; // Exactly 4 hours ago
        assert!(ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            0,
        ));
    }

    #[test]
    fn should_extract_when_cooldown_exceeded() {
        let now = 100_000;
        let last = now - DEFAULT_EXTRACTION_COOLDOWN_SECS - 1; // 4h + 1s ago
        assert!(ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            0,
        ));
    }

    #[test]
    fn should_not_extract_one_second_before_cooldown() {
        let now = 100_000;
        let last = now - DEFAULT_EXTRACTION_COOLDOWN_SECS + 1; // 1s short of 4h
        assert!(!ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            0,
        ));
    }

    #[test]
    fn should_extract_force_overrides_recent() {
        let now = 1000;
        let last = now - 10; // 10 seconds ago — well within cooldown
        assert!(ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            true,
            0,
        ));
    }

    #[test]
    fn should_extract_force_with_none() {
        assert!(ThreadDistilleryHandler::should_extract(None, 1000, true, 0));
    }

    #[test]
    fn should_extract_when_token_threshold_exceeded() {
        let now = 1000;
        let last = now - 10; // 10 seconds ago — well within time cooldown
                             // 8000 tokens * 4 chars = 32000 chars
        assert!(ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            32_000,
        ));
    }

    #[test]
    fn should_not_extract_below_token_threshold() {
        let now = 1000;
        let last = now - 10;
        // 7999 tokens * 4 = 31996 chars — just under threshold
        assert!(!ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            31_996,
        ));
    }

    #[test]
    fn should_extract_at_exact_token_threshold() {
        let now = 1000;
        let last = now - 10;
        // Exactly 8000 tokens * 4 = 32000 chars
        assert!(ThreadDistilleryHandler::should_extract(
            Some(last),
            now,
            false,
            32_000,
        ));
    }

    // --- Payload codec tests ---

    #[test]
    fn payload_round_trip() {
        let payload = ThreadDistilleryPayload {
            thread_id: "thread-abc".to_string(),
            thread_title: "Test Thread".to_string(),
            messages: vec![
                (
                    "User".to_string(),
                    "Hello".to_string(),
                    "2026-03-16 14:30".to_string(),
                ),
                (
                    "Symbiotic".to_string(),
                    "Hi there!".to_string(),
                    "2026-03-16 14:31".to_string(),
                ),
            ],
            force: false,
        };

        let encoded = encode_thread_distillery_payload(&payload);
        let decoded = decode_thread_distillery_payload(&encoded).unwrap();

        assert_eq!(decoded.thread_id, "thread-abc");
        assert_eq!(decoded.thread_title, "Test Thread");
        assert_eq!(decoded.messages.len(), 2);
        assert_eq!(decoded.messages[0].0, "User");
        assert_eq!(decoded.messages[1].1, "Hi there!");
        assert!(!decoded.force);
    }

    #[test]
    fn payload_with_force() {
        let payload = ThreadDistilleryPayload {
            thread_id: "thread-xyz".to_string(),
            thread_title: "Forced".to_string(),
            messages: vec![],
            force: true,
        };

        let encoded = encode_thread_distillery_payload(&payload);
        let decoded = decode_thread_distillery_payload(&encoded).unwrap();
        assert!(decoded.force);
    }

    #[test]
    fn payload_decode_invalid_json_fails() {
        let result = decode_thread_distillery_payload("not json");
        assert!(result.is_err());
    }

    #[test]
    fn payload_decode_missing_force_defaults_false() {
        let json = r#"{"thread_id":"t1","thread_title":"T","messages":[]}"#;
        let decoded = decode_thread_distillery_payload(json).unwrap();
        assert!(!decoded.force);
    }

    // --- staleness_event_for_thread tests ---

    use symbiotic_memory::types::{FactDisposition, FactType, MemoryStatus, Sensitivity};

    fn make_test_memory(id: &str, fact: &str, date: &str, fact_type: Option<FactType>) -> Memory {
        Memory {
            id: id.to_string(),
            entity_id: "ent-1".to_string(),
            fact: fact.to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: Sensitivity::Shareable,
            valid_from: date.to_string(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: date.to_string(),
            updated_at: date.to_string(),
            fact_type,
            authored_by: None,
            supersedes: None,
            depends_on: vec![],
            fsrs: None,
        }
    }

    #[test]
    fn staleness_event_none_for_empty_facts() {
        let result = staleness_event_for_thread("thread-t1", &[]);
        assert!(result.is_none());
    }

    #[test]
    fn staleness_event_none_for_fresh_facts() {
        // Recent facts — should be fresh.
        let facts = vec![make_test_memory(
            "f1",
            "Recent finding",
            "2026-03-22T00:00:00Z",
            Some(FactType::Finding),
        )];
        let result = staleness_event_for_thread("thread-t2", &facts);
        // This depends on "now" from system clock; since the date is recent,
        // this should be None unless the test is run far in the future.
        // We accept that this test may need updating if run after 2026-06-22.
        assert!(
            result.is_none(),
            "expected no staleness event for recent facts"
        );
    }

    #[test]
    fn staleness_event_some_for_old_facts() {
        // Very old fact — should trigger staleness.
        let facts = vec![make_test_memory(
            "f1",
            "Ancient finding",
            "2020-01-01T00:00:00Z",
            Some(FactType::Finding),
        )];
        let result = staleness_event_for_thread("thread-t3", &facts);
        assert!(result.is_some(), "expected staleness event for old facts");
        let event = result.unwrap();
        assert_eq!(event.event_type, EventType::MemoryStaleness);
        assert_eq!(event.thread_id.as_deref(), Some("thread-t3"));
        assert!(event.detail.contains("stale/suspect fact(s)"));
    }
}
