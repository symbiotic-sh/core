//! T128 §16 — Matrix operator notifications for Source Archeology.
//!
//! Posts operator-facing Matrix messages from the daemon when a Source
//! Archeology run produces decision-needed events:
//!
//! - **Handoff** (`Diagnose.verdict == NeedsOperatorInput`): one parent
//!   message carrying `HandoffReport.markdown` + a question summary block.
//! - **Escalate-disposition findings** (Triage / Verify downgraded a
//!   finding to `Escalate`): one threaded reply per finding off the
//!   Handoff parent (or, if no Handoff, off a synthesized parent
//!   run-header). Capped at `NotificationPolicy::max_escalate_messages`
//!   with a `…and N more` rollup.
//! - **Run summary** (gated by `NotificationPolicy::post_run_summary`,
//!   default false): one short top-level summary message per run.
//!
//! All messages are tagged `source: archeology` (D1) so UI / mobile
//! routers can filter them distinctly from `source: scheduler`.
//!
//! Bundle paths are rendered relative to `archive_root` for Matrix
//! display (D2 — `render_bundle_path` falls back to absolute when the
//! strip fails).
//!
//! De-duplication is the caller's responsibility (D3 — this module is
//! stateless, mirroring §14's `emit_bundle_v3` design).
//!
//! Threading uses §16a's `MatrixPoster` extensions
//! (`post_text_with_source_returning_id` for the parent,
//! `post_threaded_with_source` for the replies — D4).
//!
//! See `tasks/128-source-archeology/16-matrix-notifications.md` for the
//! full ratified design (5 decisions D1-D5).

use std::path::Path;

use anyhow::Result;
use symbiotic_agents::source_archeology::{
    ArcheologyTarget, FindingDisposition, PipelineOutcome, PipelineRun,
};

use crate::archeology_bundle::BundleSummary;
use crate::matrix_poster::{EventId, MatrixPoster};

// ── Public API ─────────────────────────────────────────────────────────

/// Policy governing how many archeology messages land in the operator's
/// goal room per run.
///
/// Matches the resolved shape that `ArcheologyPolicy::apply_to` (in
/// `symbiotic-control-plane`) produces: `notify_post_run_summary` and
/// `notify_max_escalate_messages` from the manifest map straight into
/// `post_run_summary` and `max_escalate_messages` here.
#[derive(Debug, Clone, Copy)]
pub struct NotificationPolicy {
    /// Post a top-level summary message after every completed run.
    /// Default: `false`. Operators opt in for high-traffic projects.
    pub post_run_summary: bool,
    /// Max number of Escalate findings surfaced as individual messages
    /// before collapsing the rest into `…and N more`. Default: `5`.
    pub max_escalate_messages: usize,
}

impl Default for NotificationPolicy {
    fn default() -> Self {
        Self {
            post_run_summary: false,
            max_escalate_messages: 5,
        }
    }
}

/// Audit summary returned by [`notify_operator`] so callers can log /
/// assert what landed in Matrix without scraping the room.
#[derive(Debug, Clone, Default)]
pub struct NotificationSummary {
    /// `true` when a Handoff parent message was posted.
    pub handoff_posted: bool,
    /// Number of per-finding Escalate threaded replies posted (excludes
    /// the synthesized parent header and the rollup).
    pub escalate_messages_posted: usize,
    /// `true` when the Escalate count exceeded `max_escalate_messages`
    /// and the trailing `…and N more` rollup was appended.
    pub escalate_rollup_used: bool,
    /// `true` when the optional top-level run summary was posted.
    pub run_summary_posted: bool,
    /// Captured event_id of whichever message acted as the thread parent
    /// for Escalate replies (Handoff parent or synthesized header).
    /// `None` when no thread was started (Noop run, or Handoff-only
    /// outcome with zero Escalate findings).
    pub thread_parent_event_id: Option<EventId>,
}

/// Post archeology operator notifications to `goal_room` for the given
/// pipeline outcome. Stateless — caller tracks notify state.
///
/// `bundle` is `None` when there is no on-disk bundle (e.g. Noop runs
/// short-circuit before bundling). Bundle path is rendered relative to
/// `archive_root` when possible; falls back to absolute on strip failure.
pub async fn notify_operator(
    outcome: &PipelineOutcome,
    target: &ArcheologyTarget,
    bundle: Option<&BundleSummary>,
    archive_root: &Path,
    poster: &dyn MatrixPoster,
    goal_room: &str,
    policy: NotificationPolicy,
    now: u64,
) -> Result<NotificationSummary> {
    let mut summary = NotificationSummary::default();

    // Noop outcomes never produce operator notifications — the caller
    // would have to opt into a "ran-and-was-noop" announcement, and per
    // chunk Tests #2 we explicitly post nothing here.
    let Some(run) = outcome.as_full() else {
        return Ok(summary);
    };

    let bundle_path = bundle
        .map(|b| render_bundle_path(&b.run_dir, archive_root))
        .unwrap_or_else(|| "(no bundle)".to_string());

    // ── 1. Handoff parent (captures thread_parent_event_id) ────────────
    let mut thread_parent: Option<EventId> = None;
    if let Some(handoff) = run.handoff.as_ref() {
        let body = render_handoff_body(target, handoff, &bundle_path);
        let event_id = poster
            .post_text_with_source_returning_id(goal_room, &body, "archeology", now)
            .await?;
        summary.handoff_posted = true;
        thread_parent = Some(event_id);
    }

    // ── 2. Escalate findings (threaded replies) ────────────────────────
    let escalate_findings: Vec<_> = run
        .decisions
        .iter()
        .filter(|d| d.disposition == FindingDisposition::Escalate)
        .filter_map(|d| {
            run.findings
                .iter()
                .find(|f| f.id == d.finding_id)
                .map(|f| (f, d))
        })
        .collect();

    if !escalate_findings.is_empty() {
        // Synthesize a parent header if Handoff didn't already create one.
        if thread_parent.is_none() {
            let header = render_escalate_synth_header(target, escalate_findings.len());
            let event_id = poster
                .post_text_with_source_returning_id(goal_room, &header, "archeology", now)
                .await?;
            thread_parent = Some(event_id);
        }

        let parent = thread_parent
            .clone()
            .expect("thread parent set above when escalates exist");

        let cap = policy.max_escalate_messages;
        let visible = escalate_findings.iter().take(cap);
        for (finding, decision) in visible {
            let body = render_escalate_reply(finding, decision);
            poster
                .post_threaded_with_source(goal_room, &body, "archeology", &parent, now)
                .await?;
            summary.escalate_messages_posted += 1;
        }

        if escalate_findings.len() > cap {
            let overflow = escalate_findings.len() - cap;
            let rollup = render_escalate_rollup(overflow, &bundle_path);
            poster
                .post_threaded_with_source(goal_room, &rollup, "archeology", &parent, now)
                .await?;
            summary.escalate_rollup_used = true;
        }
    }

    summary.thread_parent_event_id = thread_parent;

    // ── 3. Optional top-level run summary ──────────────────────────────
    if policy.post_run_summary {
        let body = render_run_summary(target, run, &bundle_path);
        poster
            .post_text_with_source(goal_room, &body, "archeology", now)
            .await?;
        summary.run_summary_posted = true;
    }

    Ok(summary)
}

// ── Bundle path rendering (D2) ─────────────────────────────────────────

/// Render an absolute bundle directory as a path string suitable for
/// Matrix display: relative to `archive_root` when possible, else the
/// full absolute path (defensive — strip_prefix can fail on cross-volume
/// or differently-canonicalized inputs).
fn render_bundle_path(bundle_run_dir: &Path, archive_root: &Path) -> String {
    bundle_run_dir
        .strip_prefix(archive_root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| bundle_run_dir.display().to_string())
}

// ── Message templates ──────────────────────────────────────────────────

fn render_handoff_body(
    target: &ArcheologyTarget,
    handoff: &symbiotic_agents::source_archeology::HandoffReport,
    bundle_path: &str,
) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "🧭 Source Archeology — operator input needed for {}\n\n",
        target.repo_id
    ));
    body.push_str(&handoff.markdown);
    if !handoff.markdown.ends_with('\n') {
        body.push('\n');
    }
    if !handoff.questions.is_empty() {
        body.push_str("\nOpen questions:\n");
        for q in &handoff.questions {
            body.push_str(&format!("  • {}: {} — {}\n", q.id, q.text, q.reason));
        }
    }
    body.push_str(
        "\nReply with the question id + your answer \
         (or use /archeology answer {id} {value}).\n",
    );
    body.push_str(&format!("\nBundle: {bundle_path}"));
    body
}

fn render_escalate_synth_header(target: &ArcheologyTarget, n: usize) -> String {
    format!(
        "⚠️ Archeology run produced {n} escalation{plural} for {repo}",
        plural = if n == 1 { "" } else { "s" },
        repo = target.repo_id,
    )
}

fn render_escalate_reply(
    finding: &symbiotic_agents::source_archeology::Finding,
    decision: &symbiotic_agents::source_archeology::TriageDecision,
) -> String {
    format!(
        "⚠️ Escalated finding — {evidence} ({severity:?})\n\n\
         Category: {category}\n\
         {description}\n\n\
         Triage rationale: {rationale}\n\n\
         /archeology approve {id}  |  /archeology dismiss {id}",
        evidence = finding.evidence_path,
        severity = finding.severity,
        category = finding.category,
        description = finding.description,
        rationale = decision.rationale,
        id = finding.id,
    )
}

fn render_escalate_rollup(overflow_n: usize, bundle_path: &str) -> String {
    format!("…and {overflow_n} more escalated findings — see bundle {bundle_path}")
}

fn render_run_summary(target: &ArcheologyTarget, run: &PipelineRun, bundle_path: &str) -> String {
    let summary = &run.checkpoint.triage_summary;
    format!(
        "✓ Archeology — {repo}\n\n\
         Verdict: {verdict:?} (confidence {confidence:.2})\n\
         Findings: {r}R / {d}D / {e}E\n\
         Bundle: {bundle_path}",
        repo = target.repo_id,
        verdict = run.diagnosis.verdict,
        confidence = run.diagnosis.confidence,
        r = summary.resolve,
        d = summary.defer,
        e = summary.escalate,
    )
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};
    use symbiotic_agents::source_archeology::{
        ArcheologyCheckpoint, ArcheologyMode, Diagnosis, DiagnosisVerdict, ExcavationReport,
        Finding, FindingAction, FindingDisposition, FindingSeverity, FindingSourceStage,
        GoalAlignment, HandoffReport, OperatorQuestion, StalenessReport, TriageDecision,
        TriageSummary, CHECKPOINT_SCHEMA_VERSION,
    };

    use crate::matrix_poster::EventId;

    // ── Mock MatrixPoster — captures the post-call sequence ────────────

    /// Captured message kinds, in the order the mock saw them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum CapturedPost {
        TopLevel {
            room: String,
            body: String,
            source: String,
        },
        TopLevelReturningId {
            room: String,
            body: String,
            source: String,
            event_id: EventId,
        },
        Threaded {
            room: String,
            body: String,
            source: String,
            parent: EventId,
            event_id: EventId,
        },
    }

    struct CapturingPoster {
        calls: Mutex<Vec<CapturedPost>>,
        /// Counter for synthesizing fake event_ids (`$mock-N`).
        next_id: Mutex<u64>,
    }

    impl CapturingPoster {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                next_id: Mutex::new(1),
            }
        }

        fn calls(&self) -> Vec<CapturedPost> {
            self.calls.lock().unwrap().clone()
        }

        fn mint_id(&self) -> EventId {
            let mut n = self.next_id.lock().unwrap();
            let id = format!("$mock-{}", *n);
            *n += 1;
            id
        }
    }

    #[async_trait]
    impl MatrixPoster for CapturingPoster {
        async fn post_text(&self, _room_id: &str, _body: &str, _now: u64) -> Result<()> {
            // Not used in §16 — the module always passes a source tag.
            // Defensive: panic so a regression to plain `post_text` is loud.
            panic!("CapturingPoster: post_text should not be called by archeology_notify");
        }

        async fn post_text_with_source(
            &self,
            room_id: &str,
            body: &str,
            source: &str,
            _now: u64,
        ) -> Result<()> {
            self.calls.lock().unwrap().push(CapturedPost::TopLevel {
                room: room_id.to_string(),
                body: body.to_string(),
                source: source.to_string(),
            });
            Ok(())
        }

        async fn post_text_with_source_returning_id(
            &self,
            room_id: &str,
            body: &str,
            source: &str,
            _now: u64,
        ) -> Result<EventId> {
            let event_id = self.mint_id();
            self.calls
                .lock()
                .unwrap()
                .push(CapturedPost::TopLevelReturningId {
                    room: room_id.to_string(),
                    body: body.to_string(),
                    source: source.to_string(),
                    event_id: event_id.clone(),
                });
            Ok(event_id)
        }

        async fn post_threaded_with_source(
            &self,
            room_id: &str,
            body: &str,
            source: &str,
            parent_event_id: &EventId,
            _now: u64,
        ) -> Result<EventId> {
            let event_id = self.mint_id();
            self.calls.lock().unwrap().push(CapturedPost::Threaded {
                room: room_id.to_string(),
                body: body.to_string(),
                source: source.to_string(),
                parent: parent_event_id.clone(),
                event_id: event_id.clone(),
            });
            Ok(event_id)
        }
    }

    // ── Fixtures ───────────────────────────────────────────────────────

    fn target() -> ArcheologyTarget {
        ArcheologyTarget {
            repo_id: "repo:flux".to_string(),
            base_branch: "main".to_string(),
            goal_id: "onboard".to_string(),
            allowed_paths: Vec::new(),
            mode: ArcheologyMode::Full,
        }
    }

    fn empty_excavation() -> ExcavationReport {
        ExcavationReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeef".to_string(),
            observations: Vec::new(),
        }
    }

    fn empty_staleness() -> StalenessReport {
        StalenessReport {
            repo_id: "repo:flux".to_string(),
            observed_head: "deadbeef".to_string(),
            rows: Vec::new(),
        }
    }

    fn checkpoint(verdict: DiagnosisVerdict, summary: TriageSummary) -> ArcheologyCheckpoint {
        ArcheologyCheckpoint {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            repo_id: "repo:flux".to_string(),
            run_timestamp: Utc.with_ymd_and_hms(2026, 4, 19, 12, 0, 0).unwrap(),
            observed_head: "deadbeef".to_string(),
            previous_observed_head: None,
            diagnosis_verdict: verdict,
            diagnosis_confidence: 0.9,
            staleness_by_file: Default::default(),
            discrepancy_files: Vec::new(),
            aspirational_claims: Vec::new(),
            open_issues: Vec::new(),
            files_analyzed: Vec::new(),
            no_drift_files: Vec::new(),
            findings_count: 0,
            triage_summary: summary,
        }
    }

    fn make_run(
        verdict: DiagnosisVerdict,
        findings: Vec<Finding>,
        decisions: Vec<TriageDecision>,
        handoff: Option<HandoffReport>,
    ) -> PipelineRun {
        let summary = {
            let mut s = TriageSummary::default();
            for d in &decisions {
                match d.disposition {
                    FindingDisposition::Resolve => s.resolve += 1,
                    FindingDisposition::Defer => s.defer += 1,
                    FindingDisposition::Escalate => s.escalate += 1,
                }
            }
            s
        };
        PipelineRun {
            excavation: empty_excavation(),
            staleness: empty_staleness(),
            diagnosis: Diagnosis {
                verdict,
                confidence: 0.9,
                reasoning: "test".to_string(),
            },
            findings,
            handoff,
            decisions,
            checkpoint: checkpoint(verdict, summary),
            checkpoint_path: PathBuf::from("/dev/null"),
        }
    }

    fn handoff_finding(id: &str, evidence: &str) -> Finding {
        Finding {
            id: id.to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity: FindingSeverity::High,
            category: "drift".to_string(),
            evidence_path: evidence.to_string(),
            description: "test escalate".to_string(),
            proposed_action: FindingAction::Patch {
                diff: "diff".to_string(),
            },
            goal_alignment: GoalAlignment::InScope,
        }
    }

    fn escalate_decision(finding_id: &str) -> TriageDecision {
        TriageDecision {
            finding_id: finding_id.to_string(),
            disposition: FindingDisposition::Escalate,
            rationale: "operator decision needed".to_string(),
        }
    }

    fn handoff_report() -> HandoffReport {
        HandoffReport {
            markdown: "# Handoff\nNeeds operator clarification on branch.".to_string(),
            questions: vec![
                OperatorQuestion {
                    id: "q1".to_string(),
                    text: "Salvage or rescaffold?".to_string(),
                    reason: "ambiguous freshness".to_string(),
                    choices: vec!["salvage".to_string(), "rescaffold".to_string()],
                },
                OperatorQuestion {
                    id: "q2".to_string(),
                    text: "Drop legacy section?".to_string(),
                    reason: "section references deleted module".to_string(),
                    choices: Vec::new(),
                },
            ],
        }
    }

    fn fake_bundle(run_dir: PathBuf) -> BundleSummary {
        BundleSummary {
            run_dir,
            checkpoint_path: PathBuf::from("/dev/null"),
            artifact_count: 0,
            scaffold_files_written: 0,
            patches_written: 0,
            handoff_report_written: false,
            findings_dropped_for_path_safety: 0,
        }
    }

    // ── Tests ──────────────────────────────────────────────────────────

    /// Test 1 — Handoff outcome posts exactly one parent message and
    /// captures the thread_parent_event_id. No threaded replies because
    /// no Escalate findings.
    #[tokio::test]
    async fn test_handoff_only_posts_one_parent_with_thread_id() {
        let run = make_run(
            DiagnosisVerdict::NeedsOperatorInput,
            Vec::new(),
            Vec::new(),
            Some(handoff_report()),
        );
        let outcome = PipelineOutcome::Full(Box::new(run));
        let archive_root = PathBuf::from("/archive");
        let bundle = fake_bundle(archive_root.join("ops/projects/flux/run/ts"));
        let poster = CapturingPoster::new();

        let summary = notify_operator(
            &outcome,
            &target(),
            Some(&bundle),
            &archive_root,
            &poster,
            "!goal-room:matrix.org",
            NotificationPolicy::default(),
            1000,
        )
        .await
        .unwrap();

        assert!(summary.handoff_posted);
        assert_eq!(summary.escalate_messages_posted, 0);
        assert!(!summary.escalate_rollup_used);
        assert!(!summary.run_summary_posted);
        assert_eq!(summary.thread_parent_event_id.as_deref(), Some("$mock-1"));

        let calls = poster.calls();
        assert_eq!(calls.len(), 1, "exactly one Matrix post expected");
        match &calls[0] {
            CapturedPost::TopLevelReturningId {
                body,
                source,
                event_id,
                ..
            } => {
                assert_eq!(source, "archeology");
                assert!(body.contains("🧭 Source Archeology"));
                assert!(body.contains("repo:flux"));
                assert!(body.contains("# Handoff"));
                assert!(body.contains("q1: Salvage or rescaffold?"));
                assert!(body.contains("q2: Drop legacy section?"));
                assert!(body.contains("Bundle: ops/projects/flux/run/ts"));
                assert_eq!(event_id, "$mock-1");
            }
            other => panic!("expected TopLevelReturningId, got {other:?}"),
        }
    }

    /// Test 2 — Noop outcome posts nothing and surfaces a default-empty
    /// summary.
    #[tokio::test]
    async fn test_noop_outcome_posts_nothing() {
        let cp = checkpoint(DiagnosisVerdict::NoDocs, TriageSummary::default());
        let outcome = PipelineOutcome::Noop {
            reason: "head unchanged".to_string(),
            current_head: "deadbeef".to_string(),
            prior_checkpoint: Box::new(cp),
        };
        let poster = CapturingPoster::new();

        let summary = notify_operator(
            &outcome,
            &target(),
            None,
            &PathBuf::from("/archive"),
            &poster,
            "!goal-room",
            NotificationPolicy::default(),
            1000,
        )
        .await
        .unwrap();

        assert!(!summary.handoff_posted);
        assert_eq!(summary.escalate_messages_posted, 0);
        assert!(!summary.escalate_rollup_used);
        assert!(!summary.run_summary_posted);
        assert!(summary.thread_parent_event_id.is_none());
        assert!(poster.calls().is_empty(), "Noop must post nothing");
    }

    /// Test 3 — Handoff + 3 Escalate findings with cap=5 → 1 parent + 3
    /// threaded replies, no rollup.
    #[tokio::test]
    async fn test_handoff_with_three_escalates_threads_under_handoff() {
        let findings = vec![
            handoff_finding("f-1", "docs/A.md"),
            handoff_finding("f-2", "docs/B.md"),
            handoff_finding("f-3", "docs/C.md"),
        ];
        let decisions = vec![
            escalate_decision("f-1"),
            escalate_decision("f-2"),
            escalate_decision("f-3"),
        ];
        let run = make_run(
            DiagnosisVerdict::NeedsOperatorInput,
            findings,
            decisions,
            Some(handoff_report()),
        );
        let outcome = PipelineOutcome::Full(Box::new(run));
        let archive_root = PathBuf::from("/archive");
        let bundle = fake_bundle(archive_root.join("ops/projects/flux/run/ts"));
        let poster = CapturingPoster::new();

        let summary = notify_operator(
            &outcome,
            &target(),
            Some(&bundle),
            &archive_root,
            &poster,
            "!goal-room",
            NotificationPolicy::default(),
            1000,
        )
        .await
        .unwrap();

        assert!(summary.handoff_posted);
        assert_eq!(summary.escalate_messages_posted, 3);
        assert!(!summary.escalate_rollup_used);
        assert_eq!(summary.thread_parent_event_id.as_deref(), Some("$mock-1"));

        let calls = poster.calls();
        assert_eq!(
            calls.len(),
            4,
            "1 Handoff parent + 3 threaded replies = 4 posts"
        );
        // Posts 2..=4 must be threaded under $mock-1.
        for (i, call) in calls.iter().enumerate().skip(1) {
            match call {
                CapturedPost::Threaded { parent, source, .. } => {
                    assert_eq!(parent, "$mock-1", "post #{i} not threaded under handoff");
                    assert_eq!(source, "archeology");
                }
                other => panic!("post #{i} expected Threaded, got {other:?}"),
            }
        }
    }

    /// Test 4 — Handoff + 10 Escalate findings with cap=5 → 5 threaded
    /// replies + 1 threaded rollup (`…and 5 more`). Total 6 thread replies.
    #[tokio::test]
    async fn test_handoff_with_ten_escalates_caps_at_five_with_rollup() {
        let findings: Vec<Finding> = (0..10)
            .map(|i| handoff_finding(&format!("f-{i}"), &format!("docs/{i}.md")))
            .collect();
        let decisions: Vec<TriageDecision> = (0..10)
            .map(|i| escalate_decision(&format!("f-{i}")))
            .collect();
        let run = make_run(
            DiagnosisVerdict::NeedsOperatorInput,
            findings,
            decisions,
            Some(handoff_report()),
        );
        let outcome = PipelineOutcome::Full(Box::new(run));
        let archive_root = PathBuf::from("/archive");
        let bundle = fake_bundle(archive_root.join("bundle"));
        let poster = CapturingPoster::new();

        let summary = notify_operator(
            &outcome,
            &target(),
            Some(&bundle),
            &archive_root,
            &poster,
            "!goal-room",
            NotificationPolicy::default(),
            1000,
        )
        .await
        .unwrap();

        assert!(summary.handoff_posted);
        assert_eq!(summary.escalate_messages_posted, 5);
        assert!(summary.escalate_rollup_used);
        assert_eq!(summary.thread_parent_event_id.as_deref(), Some("$mock-1"));

        let calls = poster.calls();
        assert_eq!(
            calls.len(),
            7,
            "1 parent + 5 threaded replies + 1 threaded rollup = 7"
        );
        // Last call is the rollup — threaded under the same parent.
        match calls.last().unwrap() {
            CapturedPost::Threaded { body, parent, .. } => {
                assert!(body.contains("…and 5 more escalated findings"));
                assert!(body.contains("bundle"));
                assert_eq!(parent, "$mock-1");
            }
            other => panic!("expected Threaded rollup, got {other:?}"),
        }
    }

    /// Test 5 — Escalates without Handoff: synthesize a parent header
    /// message, capture its event_id, thread replies under it.
    #[tokio::test]
    async fn test_escalates_without_handoff_synthesize_parent_header() {
        let findings = vec![
            handoff_finding("f-1", "docs/A.md"),
            handoff_finding("f-2", "docs/B.md"),
        ];
        let decisions = vec![escalate_decision("f-1"), escalate_decision("f-2")];
        let run = make_run(DiagnosisVerdict::Salvageable, findings, decisions, None);
        let outcome = PipelineOutcome::Full(Box::new(run));
        let archive_root = PathBuf::from("/archive");
        let bundle = fake_bundle(archive_root.join("ts"));
        let poster = CapturingPoster::new();

        let summary = notify_operator(
            &outcome,
            &target(),
            Some(&bundle),
            &archive_root,
            &poster,
            "!goal-room",
            NotificationPolicy::default(),
            1000,
        )
        .await
        .unwrap();

        assert!(!summary.handoff_posted, "no Handoff outcome");
        assert_eq!(summary.escalate_messages_posted, 2);
        assert!(!summary.escalate_rollup_used);
        assert_eq!(
            summary.thread_parent_event_id.as_deref(),
            Some("$mock-1"),
            "synthesized header is the thread parent"
        );

        let calls = poster.calls();
        assert_eq!(
            calls.len(),
            3,
            "1 synthesized header + 2 threaded replies = 3"
        );
        match &calls[0] {
            CapturedPost::TopLevelReturningId { body, source, .. } => {
                assert_eq!(source, "archeology");
                assert!(body.contains("⚠️ Archeology run produced 2 escalations"));
                assert!(body.contains("repo:flux"));
            }
            other => panic!("expected synthesized TopLevelReturningId header, got {other:?}"),
        }
        for call in calls.iter().skip(1) {
            match call {
                CapturedPost::Threaded { parent, .. } => {
                    assert_eq!(parent, "$mock-1");
                }
                other => panic!("expected Threaded, got {other:?}"),
            }
        }
    }

    /// Test 6 — `policy.post_run_summary == true` adds one extra
    /// top-level (non-threaded) summary message at the end.
    #[tokio::test]
    async fn test_post_run_summary_appends_top_level_summary() {
        // Use a Salvageable run with no escalates so the summary is the
        // only extra post we get.
        let run = make_run(DiagnosisVerdict::Salvageable, Vec::new(), Vec::new(), None);
        let outcome = PipelineOutcome::Full(Box::new(run));
        let archive_root = PathBuf::from("/archive");
        let bundle = fake_bundle(archive_root.join("ts"));
        let poster = CapturingPoster::new();

        let policy = NotificationPolicy {
            post_run_summary: true,
            max_escalate_messages: 5,
        };
        let summary = notify_operator(
            &outcome,
            &target(),
            Some(&bundle),
            &archive_root,
            &poster,
            "!goal-room",
            policy,
            1000,
        )
        .await
        .unwrap();

        assert!(!summary.handoff_posted);
        assert_eq!(summary.escalate_messages_posted, 0);
        assert!(summary.run_summary_posted);

        let calls = poster.calls();
        assert_eq!(calls.len(), 1, "only the run summary post");
        match &calls[0] {
            CapturedPost::TopLevel { body, source, .. } => {
                assert_eq!(source, "archeology");
                assert!(body.contains("✓ Archeology — repo:flux"));
                assert!(body.contains("Verdict: Salvageable"));
                assert!(body.contains("Bundle: ts"));
            }
            other => panic!("run summary must be top-level (non-threaded), got {other:?}"),
        }
    }

    /// Test 7 — `render_bundle_path` strips the archive_root prefix when
    /// the bundle lives under it; falls back to the absolute path
    /// otherwise.
    #[test]
    fn test_render_bundle_path_strips_archive_root_else_absolute() {
        let archive_root = PathBuf::from("/archive");
        let inside = PathBuf::from("/archive/ops/projects/flux/run/ts");
        let outside = PathBuf::from("/elsewhere/bundle/ts");

        assert_eq!(
            render_bundle_path(&inside, &archive_root),
            "ops/projects/flux/run/ts",
            "path under archive_root must be rendered relative"
        );
        assert_eq!(
            render_bundle_path(&outside, &archive_root),
            "/elsewhere/bundle/ts",
            "path outside archive_root must fall back to absolute"
        );
    }
}
