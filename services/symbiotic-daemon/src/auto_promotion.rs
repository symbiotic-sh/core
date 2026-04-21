//! Auto-promotion analyzer — detects when a thread conversation qualifies
//! for promotion to a structured goal.
//!
//! Uses continuous AI analysis of thread content (NOT message count). The
//! flow is:
//!
//! 1. **Rule-based pre-filter** (cheap, no LLM):
//!    - Skip if thread is on cooldown (dismissed within 24h)
//!    - Skip if < 5 messages
//!    - Skip if no Decision or Methodology facts were extracted
//!    - Skip if thread already has an active goal
//! 2. **LLM analysis** (only when pre-filter passes):
//!    - Analyze recent messages + extracted fact summaries
//!    - Return structured JSON with confidence, title, template
//! 3. **Proposal event** — emitted as `thread.promotion.proposed` (Question/Awaiting)
//!    with quick-reply choices for the user to approve or dismiss
//! 4. **Approval/dismissal** — handled by the command router

use std::collections::HashMap;

use anyhow::{Context, Result};
use symbiotic_core::memory_space::MemorySpace;
use symbiotic_intake::distillery::DistilleryReport;

use crate::events::{DaemonEvent, EventType};

// ---------------------------------------------------------------------------
// Pre-filter
// ---------------------------------------------------------------------------

/// Result of the cheap rule-based pre-filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreFilterResult {
    /// Pre-filter passed — eligible for LLM analysis.
    Pass,
    /// Too few messages to analyze.
    TooFewMessages,
    /// No actionable facts extracted (no Decision or Methodology signals).
    NoActionableFacts,
    /// Thread already has an active goal — skip promotion.
    AlreadyHasGoal,
    /// Thread was recently dismissed — on cooldown (default 24h).
    OnCooldown,
}

// ---------------------------------------------------------------------------
// Cooldown tracker
// ---------------------------------------------------------------------------

/// Default cooldown duration: 24 hours in seconds.
const DEFAULT_COOLDOWN_SECS: u64 = 24 * 60 * 60;

/// Tracks promotion dismissals per thread to enforce a cooldown period
/// before re-proposing promotion for the same thread.
///
/// After a user dismisses a promotion proposal, the thread is placed on
/// cooldown for `cooldown_secs` (default 24h). During this period,
/// `pre_filter_with_cooldown` will short-circuit with `OnCooldown`.
///
/// The caller in `commands.rs` should call `record_dismissal()` when
/// `handle_promotion_command(approved=false)` is invoked.
pub struct PromotionCooldownTracker {
    /// thread_id -> unix timestamp (seconds) of the dismissal.
    dismissals: HashMap<String, u64>,
    /// How long a dismissal keeps a thread on cooldown.
    cooldown_secs: u64,
}

impl Default for PromotionCooldownTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl PromotionCooldownTracker {
    /// Create a new tracker with the default 24-hour cooldown.
    pub fn new() -> Self {
        Self {
            dismissals: HashMap::new(),
            cooldown_secs: DEFAULT_COOLDOWN_SECS,
        }
    }

    /// Create a new tracker with a custom cooldown duration.
    pub fn with_cooldown(cooldown_secs: u64) -> Self {
        Self {
            dismissals: HashMap::new(),
            cooldown_secs,
        }
    }

    /// Record that a user dismissed a promotion proposal for this thread.
    pub fn record_dismissal(&mut self, thread_id: &str, now: u64) {
        self.dismissals.insert(thread_id.to_string(), now);
    }

    /// Check whether a thread is currently on cooldown.
    pub fn is_on_cooldown(&self, thread_id: &str, now: u64) -> bool {
        match self.dismissals.get(thread_id) {
            Some(&dismissed_at) => now.saturating_sub(dismissed_at) < self.cooldown_secs,
            None => false,
        }
    }

    /// Remove expired cooldown entries (housekeeping).
    ///
    /// Call periodically to prevent the map from growing unbounded.
    pub fn clear_expired(&mut self, now: u64) {
        self.dismissals
            .retain(|_, dismissed_at| now.saturating_sub(*dismissed_at) < self.cooldown_secs);
    }
}

/// Run the cheap rule-based pre-filter before calling the LLM.
///
/// # Arguments
///
/// * `message_count` - Number of messages in the thread
/// * `claims_by_space` - Fact claims grouped by memory space (from DistilleryReport)
/// * `has_active_goal` - Whether the thread already has an active goal
pub fn pre_filter(
    message_count: usize,
    claims_by_space: &HashMap<MemorySpace, usize>,
    has_active_goal: bool,
) -> PreFilterResult {
    if has_active_goal {
        return PreFilterResult::AlreadyHasGoal;
    }

    if message_count < 5 {
        return PreFilterResult::TooFewMessages;
    }

    // Check for actionable fact signals: Methodology space (procedural knowledge
    // like decisions, methods) or Knowledge space with significant claims
    // (which includes Decisions, Findings, Preferences).
    let methodology_claims = claims_by_space
        .get(&MemorySpace::Operations)
        .copied()
        .unwrap_or(0);
    let knowledge_claims = claims_by_space
        .get(&MemorySpace::Knowledge)
        .copied()
        .unwrap_or(0);

    // Need at least one Methodology claim OR at least 2 Knowledge claims
    // (a single Knowledge claim is too thin to suggest goal-worthy intent).
    if methodology_claims == 0 && knowledge_claims < 2 {
        return PreFilterResult::NoActionableFacts;
    }

    PreFilterResult::Pass
}

/// Run the pre-filter with an optional cooldown check.
///
/// If a `PromotionCooldownTracker` is provided, the cooldown check runs
/// first (before the other cheap checks). If the thread was recently
/// dismissed, returns `PreFilterResult::OnCooldown` without evaluating
/// the other criteria.
///
/// When no tracker is provided, behaves identically to [`pre_filter`].
pub fn pre_filter_with_cooldown(
    thread_id: &str,
    message_count: usize,
    claims_by_space: &HashMap<MemorySpace, usize>,
    has_active_goal: bool,
    cooldown_tracker: Option<&PromotionCooldownTracker>,
    now: u64,
) -> PreFilterResult {
    // Cooldown check first — cheapest of all (just a HashMap lookup).
    if let Some(tracker) = cooldown_tracker {
        if tracker.is_on_cooldown(thread_id, now) {
            return PreFilterResult::OnCooldown;
        }
    }

    pre_filter(message_count, claims_by_space, has_active_goal)
}

// ---------------------------------------------------------------------------
// LLM analysis types
// ---------------------------------------------------------------------------

/// Result of the LLM-based promotion analysis.
#[derive(Debug, Clone)]
pub(crate) struct PromotionAnalysis {
    /// Whether the thread should be promoted to a goal.
    pub should_promote: bool,
    /// Confidence level (0.0-1.0).
    pub confidence: f64,
    /// Suggested goal title.
    pub suggested_title: String,
    /// Suggested workflow template name.
    pub suggested_template: String,
    /// Reasoning for the promotion suggestion.
    pub reasoning: String,
}

/// Default minimum confidence threshold for proposing promotion.
pub(crate) const DEFAULT_MIN_CONFIDENCE: f64 = 0.7;

/// System prompt for the auto-promotion LLM analysis.
const PROMOTION_SYSTEM_PROMPT: &str = "\
You are analyzing a thread conversation and its extracted knowledge facts. \
Determine if there is a coherent goal or project that would benefit from \
structured execution as a Symbiotic goal.

A thread should be promoted when:
- The conversation reveals a clear objective or project the user wants to accomplish
- There are concrete decisions or methodologies discussed
- The thread has moved beyond casual conversation into actionable territory

A thread should NOT be promoted when:
- It is a casual conversation or Q&A without a clear project outcome
- The user is just brainstorming without commitment
- The discussion is about past events without future action items

Return ONLY valid JSON (no markdown, no code fences):
{
  \"should_promote\": true/false,
  \"confidence\": 0.0-1.0,
  \"suggested_title\": \"short goal title\",
  \"suggested_template\": \"general\",
  \"reasoning\": \"brief explanation\"
}

For suggested_template, use \"general\" unless the conversation clearly matches \
a specific workflow pattern.";

// ---------------------------------------------------------------------------
// LLM analysis
// ---------------------------------------------------------------------------

/// Analyze a thread for auto-promotion using an LLM.
///
/// This is async because it makes an LLM completion call. The caller is
/// responsible for bridging to sync context if needed (see the scoped thread
/// pattern in `thread_distillery.rs`).
///
/// # Arguments
///
/// * `llm` - LLM client for completion
/// * `thread_title` - Human-readable thread title
/// * `recent_messages` - Last N messages as `(sender, body, timestamp)`
/// * `fact_summaries` - Text summaries of extracted facts
pub(crate) async fn analyze_for_promotion(
    llm: &dyn symbiotic_agents::llm::LlmClient,
    thread_title: &str,
    recent_messages: &[(String, String, String)],
    fact_summaries: &[String],
) -> Result<PromotionAnalysis> {
    // Build the user prompt from thread context
    let mut user_prompt = format!("Thread: \"{thread_title}\"\n\n");

    user_prompt.push_str("Recent messages:\n");
    // Use at most the last 10 messages
    let start = recent_messages.len().saturating_sub(10);
    for (sender, body, ts) in &recent_messages[start..] {
        user_prompt.push_str(&format!("[{ts}] {sender}: {body}\n"));
    }

    if !fact_summaries.is_empty() {
        user_prompt.push_str("\nExtracted facts:\n");
        for fact in fact_summaries {
            user_prompt.push_str(&format!("- {fact}\n"));
        }
    }

    let messages = vec![
        symbiotic_agents::llm::ChatMessage {
            role: "system".to_string(),
            content: PROMOTION_SYSTEM_PROMPT.to_string(),
        },
        symbiotic_agents::llm::ChatMessage {
            role: "user".to_string(),
            content: user_prompt,
        },
    ];

    let response = llm
        .chat(&messages, true)
        .await
        .context("auto-promotion LLM call failed")?;

    parse_promotion_response(&response)
}

/// Parse the LLM's JSON response into a `PromotionAnalysis`.
pub(crate) fn parse_promotion_response(response: &str) -> Result<PromotionAnalysis> {
    // Try to extract JSON from the response (handle markdown code fences)
    let json_str = extract_json_from_response(response);

    let value: serde_json::Value =
        serde_json::from_str(json_str).context("failed to parse promotion LLM response as JSON")?;

    let should_promote = value
        .get("should_promote")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let confidence = value
        .get("confidence")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);

    let suggested_title = value
        .get("suggested_title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled Goal")
        .to_string();

    let suggested_template = value
        .get("suggested_template")
        .and_then(|v| v.as_str())
        .unwrap_or("general")
        .to_string();

    let reasoning = value
        .get("reasoning")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok(PromotionAnalysis {
        should_promote,
        confidence,
        suggested_title,
        suggested_template,
        reasoning,
    })
}

/// Extract JSON from a response that may include markdown code fences.
fn extract_json_from_response(response: &str) -> &str {
    let trimmed = response.trim();

    // Try to strip ```json ... ``` or ``` ... ```
    if let Some(rest) = trimmed.strip_prefix("```json") {
        if let Some(json) = rest.strip_suffix("```") {
            return json.trim();
        }
    }
    if let Some(rest) = trimmed.strip_prefix("```") {
        if let Some(json) = rest.strip_suffix("```") {
            return json.trim();
        }
    }

    trimmed
}

// ---------------------------------------------------------------------------
// Promotion proposal event
// ---------------------------------------------------------------------------

/// Build a `thread.promotion.proposed` DaemonEvent for the user to approve/reject.
pub(crate) fn build_promotion_proposal(
    thread_id: &str,
    analysis: &PromotionAnalysis,
) -> DaemonEvent {
    let detail = format!(
        "{}\n\nSuggested goal: {}\nConfidence: {:.0}%",
        analysis.reasoning,
        analysis.suggested_title,
        analysis.confidence * 100.0,
    );

    let choices = vec!["Promote to Goal".to_string(), "Dismiss".to_string()];
    let quick_replies =
        serde_json::to_string(&choices).expect("choices must be serializable to JSON");

    DaemonEvent {
        event_type: EventType::ThreadPromotionProposed,
        status: "awaiting".to_string(),
        job_id: None,
        detail,
        goal_room: None,
        goal_template: Some(analysis.suggested_template.clone()),
        goal_run_id: None,
        goal_id: None,
        intake_run_id: None,
        url: None,
        title: Some(analysis.suggested_title.clone()),
        sensitivity: None,
        quick_replies: Some(quick_replies),
        thread_id: Some(thread_id.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Promotion command handling
// ---------------------------------------------------------------------------

/// Action to take after promotion approval.
#[derive(Debug, Clone)]
pub(crate) struct PromotionAction {
    pub template: String,
    #[allow(dead_code)]
    pub goal_title: String,
    #[allow(dead_code)]
    pub thread_id: String,
}

/// Handle the user's response to a promotion proposal.
///
/// Returns `Some(PromotionAction)` if approved, `None` if dismissed.
/// When dismissed, builds a `thread.promotion.dismissed` event via the
/// returned `Option<DaemonEvent>` in the second tuple element.
pub(crate) fn handle_promotion_command(
    thread_id: &str,
    approved: bool,
    suggested_title: &str,
    suggested_template: &str,
) -> (Option<PromotionAction>, Option<DaemonEvent>) {
    if approved {
        let action = PromotionAction {
            template: suggested_template.to_string(),
            goal_title: suggested_title.to_string(),
            thread_id: thread_id.to_string(),
        };

        let event = DaemonEvent {
            event_type: EventType::ThreadPromotionAccepted,
            status: "completed".to_string(),
            job_id: None,
            detail: format!("Promoted to goal: {suggested_title}"),
            goal_room: None,
            goal_template: Some(suggested_template.to_string()),
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: Some(suggested_title.to_string()),
            sensitivity: None,
            quick_replies: None,
            thread_id: Some(thread_id.to_string()),
        };

        (Some(action), Some(event))
    } else {
        let event = DaemonEvent {
            event_type: EventType::ThreadPromotionDismissed,
            status: "completed".to_string(),
            job_id: None,
            detail: "Promotion dismissed by user".to_string(),
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
        };

        (None, Some(event))
    }
}

// ---------------------------------------------------------------------------
// Distillery integration helper
// ---------------------------------------------------------------------------

/// Check whether a distillery report warrants auto-promotion analysis,
/// and if so, return fact summaries suitable for the LLM prompt.
///
/// This is the bridge between `execute_thread_distillery_job` and the
/// auto-promotion flow. It runs the pre-filter and, if it passes,
/// returns the data needed for the async LLM analysis.
#[allow(dead_code)]
pub(crate) fn check_promotion_eligibility(
    report: &DistilleryReport,
    message_count: usize,
    has_active_goal: bool,
) -> Option<PreFilterResult> {
    let result = pre_filter(message_count, &report.claims_by_space, has_active_goal);
    if result == PreFilterResult::Pass {
        Some(result)
    } else {
        log::debug!(
            "auto_promotion: pre-filter rejected (reason={:?}, messages={}, claims={:?})",
            result,
            message_count,
            report.claims_by_space,
        );
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- pre_filter tests ---

    #[test]
    fn pre_filter_rejects_fewer_than_5_messages() {
        let claims = HashMap::from([(MemorySpace::Operations, 1)]);
        assert_eq!(
            pre_filter(0, &claims, false),
            PreFilterResult::TooFewMessages
        );
        assert_eq!(
            pre_filter(1, &claims, false),
            PreFilterResult::TooFewMessages
        );
        assert_eq!(
            pre_filter(2, &claims, false),
            PreFilterResult::TooFewMessages
        );
        assert_eq!(
            pre_filter(3, &claims, false),
            PreFilterResult::TooFewMessages
        );
        assert_eq!(
            pre_filter(4, &claims, false),
            PreFilterResult::TooFewMessages
        );
    }

    #[test]
    fn pre_filter_rejects_no_actionable_facts() {
        // No claims at all
        let empty: HashMap<MemorySpace, usize> = HashMap::new();
        assert_eq!(
            pre_filter(5, &empty, false),
            PreFilterResult::NoActionableFacts
        );

        // Only 1 knowledge claim (not enough without methodology)
        let one_knowledge = HashMap::from([(MemorySpace::Knowledge, 1)]);
        assert_eq!(
            pre_filter(5, &one_knowledge, false),
            PreFilterResult::NoActionableFacts
        );

        // Only SelfSpace claims (no decision/methodology signal)
        let self_only = HashMap::from([(MemorySpace::Identity, 3)]);
        assert_eq!(
            pre_filter(5, &self_only, false),
            PreFilterResult::NoActionableFacts
        );
    }

    #[test]
    fn pre_filter_rejects_active_goal() {
        let claims = HashMap::from([(MemorySpace::Operations, 2)]);
        assert_eq!(
            pre_filter(10, &claims, true),
            PreFilterResult::AlreadyHasGoal
        );
    }

    #[test]
    fn pre_filter_passes_with_methodology_claims() {
        let claims = HashMap::from([(MemorySpace::Operations, 1)]);
        assert_eq!(pre_filter(5, &claims, false), PreFilterResult::Pass);
    }

    #[test]
    fn pre_filter_passes_with_sufficient_knowledge_claims() {
        let claims = HashMap::from([(MemorySpace::Knowledge, 2)]);
        assert_eq!(pre_filter(5, &claims, false), PreFilterResult::Pass);
    }

    #[test]
    fn pre_filter_passes_with_mixed_claims() {
        let claims = HashMap::from([(MemorySpace::Knowledge, 1), (MemorySpace::Operations, 1)]);
        assert_eq!(pre_filter(6, &claims, false), PreFilterResult::Pass);
    }

    // --- build_promotion_proposal tests ---

    #[test]
    fn build_proposal_has_correct_event_fields() {
        let analysis = PromotionAnalysis {
            should_promote: true,
            confidence: 0.85,
            suggested_title: "Build a website".to_string(),
            suggested_template: "general".to_string(),
            reasoning: "The conversation shows clear intent to build a website".to_string(),
        };

        let event = build_promotion_proposal("thread-abc", &analysis);

        assert_eq!(event.event_type, EventType::ThreadPromotionProposed);
        assert_eq!(event.status, "awaiting");
        assert_eq!(event.thread_id.as_deref(), Some("thread-abc"));
        assert_eq!(event.title.as_deref(), Some("Build a website"));
        assert_eq!(event.goal_template.as_deref(), Some("general"));
        assert!(event.detail.contains("Build a website"));
        assert!(event.detail.contains("85%"));
        assert!(event.quick_replies.is_some());

        // Verify quick_replies is valid JSON with expected choices
        let choices: Vec<String> =
            serde_json::from_str(event.quick_replies.as_ref().unwrap()).unwrap();
        assert_eq!(choices, vec!["Promote to Goal", "Dismiss"]);
    }

    // --- handle_promotion_command tests ---

    #[test]
    fn handle_promotion_approved_returns_action() {
        let (action, event) = handle_promotion_command("thread-xyz", true, "My Goal", "general");

        assert!(action.is_some());
        let action = action.unwrap();
        assert_eq!(action.template, "general");
        assert_eq!(action.goal_title, "My Goal");
        assert_eq!(action.thread_id, "thread-xyz");

        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.event_type, EventType::ThreadPromotionAccepted);
        assert_eq!(event.thread_id.as_deref(), Some("thread-xyz"));
        assert!(event.detail.contains("My Goal"));
    }

    #[test]
    fn handle_promotion_dismissed_returns_none() {
        let (action, event) = handle_promotion_command("thread-xyz", false, "My Goal", "general");

        assert!(action.is_none());

        assert!(event.is_some());
        let event = event.unwrap();
        assert_eq!(event.event_type, EventType::ThreadPromotionDismissed);
        assert_eq!(event.thread_id.as_deref(), Some("thread-xyz"));
        assert!(event.detail.contains("dismissed"));
    }

    // --- parse_promotion_response tests ---

    #[test]
    fn parse_valid_json_response() {
        let json = r#"{
            "should_promote": true,
            "confidence": 0.85,
            "suggested_title": "Website Redesign",
            "suggested_template": "general",
            "reasoning": "Clear project intent detected"
        }"#;

        let analysis = parse_promotion_response(json).unwrap();
        assert!(analysis.should_promote);
        assert!((analysis.confidence - 0.85).abs() < 0.001);
        assert_eq!(analysis.suggested_title, "Website Redesign");
        assert_eq!(analysis.suggested_template, "general");
        assert_eq!(analysis.reasoning, "Clear project intent detected");
    }

    #[test]
    fn parse_json_with_code_fences() {
        let response = "```json\n{\"should_promote\": false, \"confidence\": 0.3, \"suggested_title\": \"Nope\", \"suggested_template\": \"general\", \"reasoning\": \"Just chatting\"}\n```";

        let analysis = parse_promotion_response(response).unwrap();
        assert!(!analysis.should_promote);
        assert!((analysis.confidence - 0.3).abs() < 0.001);
    }

    #[test]
    fn parse_json_with_bare_code_fences() {
        let response = "```\n{\"should_promote\": true, \"confidence\": 0.9, \"suggested_title\": \"Do it\", \"suggested_template\": \"general\", \"reasoning\": \"Go\"}\n```";

        let analysis = parse_promotion_response(response).unwrap();
        assert!(analysis.should_promote);
    }

    #[test]
    fn parse_missing_fields_use_defaults() {
        let json = r#"{"should_promote": true}"#;

        let analysis = parse_promotion_response(json).unwrap();
        assert!(analysis.should_promote);
        assert!((analysis.confidence - 0.0).abs() < 0.001);
        assert_eq!(analysis.suggested_title, "Untitled Goal");
        assert_eq!(analysis.suggested_template, "general");
        assert_eq!(analysis.reasoning, "");
    }

    #[test]
    fn parse_confidence_clamped_to_range() {
        let json = r#"{"should_promote": true, "confidence": 1.5}"#;
        let analysis = parse_promotion_response(json).unwrap();
        assert!((analysis.confidence - 1.0).abs() < 0.001);

        let json2 = r#"{"should_promote": true, "confidence": -0.5}"#;
        let analysis2 = parse_promotion_response(json2).unwrap();
        assert!((analysis2.confidence - 0.0).abs() < 0.001);
    }

    #[test]
    fn parse_invalid_json_fails() {
        let result = parse_promotion_response("not json at all");
        assert!(result.is_err());
    }

    // --- classify integration tests (tested via events.rs, but verify here too) ---

    #[test]
    fn promotion_proposed_event_classifies_correctly() {
        use symbiotic_core::protocol::{Kind, Status};

        let analysis = PromotionAnalysis {
            should_promote: true,
            confidence: 0.8,
            suggested_title: "Test Goal".to_string(),
            suggested_template: "general".to_string(),
            reasoning: "Test reasoning".to_string(),
        };
        let event = build_promotion_proposal("thread-t1", &analysis);
        let (kind, status, body) = event.classify();

        assert_eq!(kind, Kind::Question);
        assert_eq!(status, Status::Awaiting);
        assert!(body.contains("Test Goal"), "body={body}");
    }

    #[test]
    fn promotion_accepted_event_classifies_correctly() {
        use symbiotic_core::protocol::{Kind, Status};

        let (_, event) = handle_promotion_command("thread-t1", true, "My Goal", "general");
        let event = event.unwrap();
        let (kind, status, body) = event.classify();

        assert_eq!(kind, Kind::State);
        assert_eq!(status, Status::Success);
        assert!(body.contains("promoted to goal"), "body={body}");
    }

    #[test]
    fn promotion_dismissed_event_classifies_correctly() {
        use symbiotic_core::protocol::{Kind, Status};

        let (_, event) = handle_promotion_command("thread-t1", false, "My Goal", "general");
        let event = event.unwrap();
        let (kind, status, body) = event.classify();

        assert_eq!(kind, Kind::State);
        assert_eq!(status, Status::Success);
        assert!(body.contains("promotion dismissed"), "body={body}");
    }

    // --- check_promotion_eligibility tests ---

    #[test]
    fn eligibility_returns_none_when_prefilter_fails() {
        let report = DistilleryReport {
            claims_extracted: 0,
            claims_verified: 0,
            links_proposed: 0,
            links_verified: 0,
            notes_rewritten: 0,
            archive_path: std::path::PathBuf::new(),
            claims_by_space: HashMap::new(),
        };

        // Too few messages
        assert!(check_promotion_eligibility(&report, 2, false).is_none());

        // Already has goal
        let report_with_claims = DistilleryReport {
            claims_extracted: 0,
            claims_verified: 0,
            links_proposed: 0,
            links_verified: 0,
            notes_rewritten: 0,
            archive_path: std::path::PathBuf::new(),
            claims_by_space: HashMap::from([(MemorySpace::Operations, 2)]),
        };
        assert!(check_promotion_eligibility(&report_with_claims, 5, true).is_none());
    }

    #[test]
    fn eligibility_returns_some_when_prefilter_passes() {
        let report = DistilleryReport {
            claims_extracted: 3,
            claims_verified: 2,
            links_proposed: 0,
            links_verified: 0,
            notes_rewritten: 0,
            archive_path: std::path::PathBuf::new(),
            claims_by_space: HashMap::from([(MemorySpace::Operations, 2)]),
        };

        assert!(check_promotion_eligibility(&report, 5, false).is_some());
    }

    // --- cooldown tracker tests ---

    #[test]
    fn fresh_thread_is_not_on_cooldown() {
        let tracker = PromotionCooldownTracker::new();
        let now = 1_000_000;
        assert!(!tracker.is_on_cooldown("thread-new", now));
    }

    #[test]
    fn dismissed_thread_is_on_cooldown() {
        let mut tracker = PromotionCooldownTracker::new();
        let now = 1_000_000;
        tracker.record_dismissal("thread-abc", now);

        // Immediately after dismissal — on cooldown
        assert!(tracker.is_on_cooldown("thread-abc", now));

        // 1 hour later — still on cooldown
        assert!(tracker.is_on_cooldown("thread-abc", now + 3600));

        // 23 hours later — still on cooldown
        assert!(tracker.is_on_cooldown("thread-abc", now + 23 * 3600));
    }

    #[test]
    fn cooldown_expires_after_24h() {
        let mut tracker = PromotionCooldownTracker::new();
        let now = 1_000_000;
        tracker.record_dismissal("thread-abc", now);

        // Exactly at 24h boundary — no longer on cooldown (>= cooldown_secs)
        let after_24h = now + 24 * 3600;
        assert!(!tracker.is_on_cooldown("thread-abc", after_24h));

        // Well past 24h — definitely not on cooldown
        assert!(!tracker.is_on_cooldown("thread-abc", now + 48 * 3600));
    }

    #[test]
    fn clear_expired_removes_old_entries() {
        let mut tracker = PromotionCooldownTracker::with_cooldown(100);
        let now = 1_000;

        tracker.record_dismissal("thread-old", now);
        tracker.record_dismissal("thread-recent", now + 80);

        // At now + 110: thread-old expired (110 >= 100), thread-recent still active (30 < 100)
        tracker.clear_expired(now + 110);

        assert!(!tracker.is_on_cooldown("thread-old", now + 110));
        assert!(tracker.is_on_cooldown("thread-recent", now + 110));
    }

    #[test]
    fn custom_cooldown_duration() {
        let mut tracker = PromotionCooldownTracker::with_cooldown(60); // 60 seconds
        let now = 500;
        tracker.record_dismissal("thread-x", now);

        assert!(tracker.is_on_cooldown("thread-x", now + 30)); // 30s < 60s
        assert!(!tracker.is_on_cooldown("thread-x", now + 60)); // 60s >= 60s
    }

    // --- pre_filter_with_cooldown tests ---

    #[test]
    fn pre_filter_with_cooldown_returns_on_cooldown() {
        let mut tracker = PromotionCooldownTracker::with_cooldown(3600);
        let now = 10_000;
        tracker.record_dismissal("thread-cool", now);

        let claims = HashMap::from([(MemorySpace::Operations, 2)]);
        // Thread would otherwise pass, but cooldown blocks it
        let result =
            pre_filter_with_cooldown("thread-cool", 5, &claims, false, Some(&tracker), now + 100);
        assert_eq!(result, PreFilterResult::OnCooldown);
    }

    #[test]
    fn pre_filter_with_cooldown_passes_after_expiry() {
        let mut tracker = PromotionCooldownTracker::with_cooldown(3600);
        let now = 10_000;
        tracker.record_dismissal("thread-cool", now);

        let claims = HashMap::from([(MemorySpace::Operations, 2)]);
        // After cooldown expires, normal pre-filter logic applies
        let result = pre_filter_with_cooldown(
            "thread-cool",
            5,
            &claims,
            false,
            Some(&tracker),
            now + 3600, // exactly at expiry
        );
        assert_eq!(result, PreFilterResult::Pass);
    }

    #[test]
    fn pre_filter_with_cooldown_no_tracker_skips_cooldown_check() {
        let claims = HashMap::from([(MemorySpace::Operations, 1)]);
        // No tracker provided — should behave like plain pre_filter
        let result = pre_filter_with_cooldown("any-thread", 5, &claims, false, None, 0);
        assert_eq!(result, PreFilterResult::Pass);
    }

    #[test]
    fn pre_filter_with_cooldown_still_checks_other_criteria() {
        let tracker = PromotionCooldownTracker::new(); // no dismissals
        let claims = HashMap::new(); // no actionable facts

        // Not on cooldown, but fails on NoActionableFacts
        let result =
            pre_filter_with_cooldown("thread-new", 5, &claims, false, Some(&tracker), 1000);
        assert_eq!(result, PreFilterResult::NoActionableFacts);
    }
}
