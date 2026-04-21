//! Stage 5 — Triage (decision tree over findings).
//!
//! For each `Finding` produced by Reconcile / Scaffold, decides
//! `Resolve | Defer | Escalate`. MVP is a declarative tree per design
//! doc §Stage 5; post-MVP will swap in an LLM-reasoning triager via
//! the same `Triager` trait.
//!
//! See `docs/design/source-archeology.md` §Stage 5 — Triage.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use symbiotic_core::protocol::{ChatMessage, LlmClient};

use super::archeology_types::{
    Autonomy, Finding, FindingAction, FindingDisposition, FindingSeverity, GoalAlignment,
    TriageDecision,
};

// ── Context + raw output ───────────────────────────────────────────────

/// Per-run context passed to the triager / reviewer. `autonomy` is the
/// dominant input; extended with thread-signal overrides post-MVP.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TriageContext {
    pub autonomy: Autonomy,
}

/// Raw triager output — disposition + rationale, before config-level
/// policy adjustments (peer-review overrides) are applied.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RawDisposition {
    pub disposition: FindingDisposition,
    pub rationale: String,
}

// ── Config (per docs/design/agent-tunables.md) ─────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TriageConfig {
    /// Out-of-scope findings with severity ≤ this are routed to `Defer`.
    /// Higher-severity out-of-scope findings proceed through the tree's
    /// remaining branches. Default: `Medium` — `High` stays eligible
    /// for `Resolve` / `Escalate` even when out of scope.
    pub out_of_scope_defer_max_severity: FindingSeverity,
    /// When a `Reviewer` is configured and its disposition differs from
    /// the triager's, escalate. Default: `true`.
    pub escalate_on_peer_disagreement: bool,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            out_of_scope_defer_max_severity: FindingSeverity::Medium,
            escalate_on_peer_disagreement: true,
        }
    }
}

// ── Traits ─────────────────────────────────────────────────────────────

/// Triager. MVP is `DeclarativeTriager` (deterministic); post-MVP swaps
/// an LLM-backed implementation behind this trait.
#[async_trait]
pub trait Triager: Send + Sync {
    async fn triage(&self, finding: &Finding, ctx: &TriageContext) -> Result<RawDisposition>;
}

/// Optional peer-review agent. Takes the triager's raw output and
/// returns its own verdict; the runner compares and (if disagreeing)
/// escalates per `TriageConfig.escalate_on_peer_disagreement`.
#[async_trait]
pub trait Reviewer: Send + Sync {
    async fn review(
        &self,
        finding: &Finding,
        ctx: &TriageContext,
        primary: &RawDisposition,
    ) -> Result<RawDisposition>;
}

// ── MVP declarative triager ────────────────────────────────────────────

/// Deterministic decision tree implementing the MVP spec from
/// `docs/design/source-archeology.md` §Stage 5. No LLM.
#[derive(Default)]
pub struct DeclarativeTriager {
    pub config: TriageConfig,
}

#[async_trait]
impl Triager for DeclarativeTriager {
    async fn triage(&self, finding: &Finding, ctx: &TriageContext) -> Result<RawDisposition> {
        // Rule 1: critical always escalates.
        if finding.severity == FindingSeverity::Critical {
            return Ok(RawDisposition {
                disposition: FindingDisposition::Escalate,
                rationale: "rule=critical_escalate".to_string(),
            });
        }
        // Rule 2: manual autonomy escalates everything.
        if ctx.autonomy == Autonomy::Manual {
            return Ok(RawDisposition {
                disposition: FindingDisposition::Escalate,
                rationale: "rule=manual_autonomy_escalate".to_string(),
            });
        }
        // Rule 3: out-of-scope + low/medium severity → defer.
        if finding.goal_alignment == GoalAlignment::OutOfScope
            && severity_rank(finding.severity)
                <= severity_rank(self.config.out_of_scope_defer_max_severity)
        {
            return Ok(RawDisposition {
                disposition: FindingDisposition::Defer,
                rationale: format!(
                    "rule=out_of_scope_defer; severity={:?}<=max={:?}",
                    finding.severity, self.config.out_of_scope_defer_max_severity
                ),
            });
        }
        // Rule 4 (peer review) is applied by the runner, not here.
        // Rule 5: default to resolve.
        Ok(RawDisposition {
            disposition: FindingDisposition::Resolve,
            rationale: format!(
                "rule=default_resolve; severity={:?}; autonomy={:?}; alignment={:?}",
                finding.severity, ctx.autonomy, finding.goal_alignment
            ),
        })
    }
}

// ── LLM-backed triager (post-MVP) ──────────────────────────────────────

/// `deep`-tier triager backed by an `LlmClient`. Post-MVP alternative
/// to `DeclarativeTriager`. Sends the Finding + TriageContext + a
/// sketch of the declarative default to the LLM, which may agree or
/// override.
///
/// Conservative fallback: if the LLM errors or returns malformed
/// output, falls back to `DeclarativeTriager` (so the pipeline still
/// makes progress rather than escalating everything on transient LLM
/// failure). That fallback is more generous than Diagnose's error
/// path because Triage runs for every Finding; escalating everything
/// on a transient error would flood the operator.
pub struct LlmTriager<'a> {
    client: &'a dyn LlmClient,
    declarative_fallback: DeclarativeTriager,
}

impl<'a> LlmTriager<'a> {
    pub fn new(client: &'a dyn LlmClient, config: TriageConfig) -> Self {
        Self {
            client,
            declarative_fallback: DeclarativeTriager { config },
        }
    }

    fn build_messages(finding: &Finding, ctx: &TriageContext) -> Result<Vec<ChatMessage>> {
        let system = "You are the Source Archeology triager. For each finding, decide \
                      one disposition: resolve (apply the action now), defer (record as \
                      an open issue for future consideration), or escalate (operator must \
                      decide before any action). Reply with exactly one JSON object: \
                      {\"disposition\": \"resolve\" | \"defer\" | \"escalate\", \
                      \"rationale\": \"<1-3 sentences>\"}. No prose outside the JSON. \
                      Be conservative — prefer defer over resolve on ambiguity, and \
                      escalate on security-sensitive or high-severity findings.";
        let action_kind = match &finding.proposed_action {
            FindingAction::Patch { .. } => "patch",
            FindingAction::NewFile { .. } => "new_file",
            FindingAction::Report { .. } => "report",
        };
        let user = serde_json::to_string_pretty(&serde_json::json!({
            "finding": {
                "id": finding.id,
                "source_stage": finding.source_stage,
                "severity": finding.severity,
                "category": finding.category,
                "evidence_path": finding.evidence_path,
                "description": finding.description,
                "action_kind": action_kind,
                "goal_alignment": finding.goal_alignment,
            },
            "context": {
                "autonomy": ctx.autonomy,
            },
        }))?;
        Ok(vec![
            ChatMessage {
                role: "system".to_string(),
                content: system.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: user,
            },
        ])
    }
}

#[async_trait]
impl<'a> Triager for LlmTriager<'a> {
    async fn triage(&self, finding: &Finding, ctx: &TriageContext) -> Result<RawDisposition> {
        let messages = match Self::build_messages(finding, ctx) {
            Ok(m) => m,
            Err(_) => return self.declarative_fallback.triage(finding, ctx).await,
        };
        let resp = match self.client.chat(&messages, true).await {
            Ok(r) => r,
            Err(_) => return self.declarative_fallback.triage(finding, ctx).await,
        };
        match parse_triage_response(&resp) {
            Ok(d) => Ok(d),
            Err(_) => self.declarative_fallback.triage(finding, ctx).await,
        }
    }
}

fn parse_triage_response(resp: &str) -> Result<RawDisposition> {
    let trimmed = resp.trim();
    let parsed: RawDisposition = serde_json::from_str(trimmed)
        .map_err(|e| anyhow::anyhow!("LLM response is not a RawDisposition: {e}; raw={resp}"))?;
    Ok(parsed)
}

// ── Runner ─────────────────────────────────────────────────────────────

pub async fn run(
    findings: &[Finding],
    ctx: &TriageContext,
    triager: &dyn Triager,
    reviewer: Option<&dyn Reviewer>,
    config: TriageConfig,
) -> Result<Vec<TriageDecision>> {
    let mut decisions: Vec<TriageDecision> = Vec::with_capacity(findings.len());
    for finding in findings {
        // Primary triage.
        let primary = match triager.triage(finding, ctx).await {
            Ok(p) => p,
            Err(err) => RawDisposition {
                disposition: FindingDisposition::Escalate,
                rationale: format!("triager_error:{err}"),
            },
        };

        // Optional peer review, with configurable disagreement policy.
        let final_raw = if let Some(r) = reviewer {
            match r.review(finding, ctx, &primary).await {
                Ok(peer) => {
                    if peer.disposition != primary.disposition
                        && config.escalate_on_peer_disagreement
                    {
                        RawDisposition {
                            disposition: FindingDisposition::Escalate,
                            rationale: format!(
                                "rule=peer_disagreement; primary={:?}; peer={:?}; primary_rationale=\"{}\"; peer_rationale=\"{}\"",
                                primary.disposition, peer.disposition, primary.rationale, peer.rationale
                            ),
                        }
                    } else {
                        primary
                    }
                }
                Err(err) => RawDisposition {
                    disposition: FindingDisposition::Escalate,
                    rationale: format!(
                        "reviewer_error:{err}; primary_was={:?}",
                        primary.disposition
                    ),
                },
            }
        } else {
            primary
        };

        decisions.push(TriageDecision {
            finding_id: finding.id.clone(),
            disposition: final_raw.disposition,
            rationale: final_raw.rationale,
        });
    }
    Ok(decisions)
}

// ── Helpers ────────────────────────────────────────────────────────────

fn severity_rank(s: FindingSeverity) -> u8 {
    match s {
        FindingSeverity::Low => 1,
        FindingSeverity::Medium => 2,
        FindingSeverity::High => 3,
        FindingSeverity::Critical => 4,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_archeology::archeology_types::{FindingAction, FindingSourceStage};
    use crate::source_archeology::fixtures::{ScriptedReviewer, ScriptedTriager};

    fn finding_with(severity: FindingSeverity, alignment: GoalAlignment) -> Finding {
        Finding {
            id: "f-1".to_string(),
            source_stage: FindingSourceStage::Reconcile,
            severity,
            category: "drift".to_string(),
            evidence_path: "docs/x.md".to_string(),
            description: "test".to_string(),
            proposed_action: FindingAction::Patch {
                diff: "--- a\n+++ b\n".to_string(),
            },
            goal_alignment: alignment,
        }
    }

    async fn triage_single(
        finding: Finding,
        ctx: TriageContext,
        triager: &dyn Triager,
        reviewer: Option<&dyn Reviewer>,
        config: TriageConfig,
    ) -> TriageDecision {
        let out = run(&[finding], &ctx, triager, reviewer, config)
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        out.into_iter().next().unwrap()
    }

    #[tokio::test]
    async fn critical_always_escalates() {
        let triager = DeclarativeTriager::default();
        // Even with Auto autonomy + InScope (which would otherwise resolve).
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let f = finding_with(FindingSeverity::Critical, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Escalate);
        assert!(d.rationale.contains("critical_escalate"));
    }

    #[tokio::test]
    async fn manual_autonomy_escalates_non_critical() {
        let triager = DeclarativeTriager::default();
        let ctx = TriageContext {
            autonomy: Autonomy::Manual,
        };
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Escalate);
        assert!(d.rationale.contains("manual_autonomy_escalate"));
    }

    #[tokio::test]
    async fn out_of_scope_low_severity_defers() {
        let triager = DeclarativeTriager::default();
        let ctx = TriageContext {
            autonomy: Autonomy::Semi,
        };
        let f = finding_with(FindingSeverity::Low, GoalAlignment::OutOfScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Defer);
        assert!(d.rationale.contains("out_of_scope_defer"));
    }

    #[tokio::test]
    async fn out_of_scope_high_severity_resolves() {
        // High > Medium threshold → skips Defer branch → default Resolve.
        let triager = DeclarativeTriager::default();
        let ctx = TriageContext {
            autonomy: Autonomy::Semi,
        };
        let f = finding_with(FindingSeverity::High, GoalAlignment::OutOfScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Resolve);
    }

    #[tokio::test]
    async fn in_scope_medium_auto_resolves() {
        let triager = DeclarativeTriager::default();
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Resolve);
        assert!(d.rationale.contains("default_resolve"));
    }

    #[tokio::test]
    async fn reviewer_disagrees_escalates() {
        let triager = DeclarativeTriager::default();
        let reviewer = ScriptedReviewer::new_ok(RawDisposition {
            disposition: FindingDisposition::Defer,
            rationale: "peer says defer".to_string(),
        });
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        // Triager would normally return Resolve on this input.
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, Some(&reviewer), TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Escalate);
        assert!(d.rationale.contains("peer_disagreement"));
    }

    #[tokio::test]
    async fn reviewer_agrees_preserves_disposition() {
        let triager = DeclarativeTriager::default();
        let reviewer = ScriptedReviewer::new_ok(RawDisposition {
            disposition: FindingDisposition::Resolve,
            rationale: "peer agrees".to_string(),
        });
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, Some(&reviewer), TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Resolve);
        assert!(d.rationale.contains("default_resolve"));
    }

    #[tokio::test]
    async fn triager_error_escalates() {
        let triager = ScriptedTriager::new_err();
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let d = triage_single(f, ctx, &triager, None, TriageConfig::default()).await;
        assert_eq!(d.disposition, FindingDisposition::Escalate);
        assert!(d.rationale.contains("triager_error"));
    }

    #[test]
    fn parse_triage_response_valid_json() {
        let r = parse_triage_response(r#"{"disposition": "resolve", "rationale": "ok"}"#).unwrap();
        assert_eq!(r.disposition, FindingDisposition::Resolve);
        assert_eq!(r.rationale, "ok");
    }

    #[test]
    fn parse_triage_response_rejects_garbage() {
        assert!(parse_triage_response("not json").is_err());
        assert!(parse_triage_response(r#"{"disposition": "maybe"}"#).is_err());
    }

    #[tokio::test]
    async fn llm_triager_uses_llm_response_on_success() {
        use symbiotic_core::protocol::ChatMessage;
        struct StubLlm(std::sync::Mutex<String>);
        #[async_trait::async_trait]
        impl symbiotic_core::protocol::LlmClient for StubLlm {
            async fn chat(&self, _: &[ChatMessage], _: bool) -> anyhow::Result<String> {
                Ok(self.0.lock().unwrap().clone())
            }
        }
        let llm = StubLlm(std::sync::Mutex::new(
            r#"{"disposition": "defer", "rationale": "llm said defer"}"#.to_string(),
        ));
        let triager = LlmTriager::new(&llm, TriageConfig::default());
        let f = finding_with(FindingSeverity::Low, GoalAlignment::InScope);
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let raw = triager.triage(&f, &ctx).await.unwrap();
        assert_eq!(raw.disposition, FindingDisposition::Defer);
        assert_eq!(raw.rationale, "llm said defer");
    }

    #[tokio::test]
    async fn llm_triager_falls_back_on_llm_error() {
        struct ErrLlm;
        #[async_trait::async_trait]
        impl symbiotic_core::protocol::LlmClient for ErrLlm {
            async fn chat(
                &self,
                _: &[symbiotic_core::protocol::ChatMessage],
                _: bool,
            ) -> anyhow::Result<String> {
                Err(anyhow::anyhow!("network down"))
            }
        }
        let triager = LlmTriager::new(&ErrLlm, TriageConfig::default());
        // Input that the declarative tree classifies as Resolve.
        let f = finding_with(FindingSeverity::Medium, GoalAlignment::InScope);
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let raw = triager.triage(&f, &ctx).await.unwrap();
        assert_eq!(
            raw.disposition,
            FindingDisposition::Resolve,
            "fallback to DeclarativeTriager on LLM error"
        );
        assert!(raw.rationale.contains("default_resolve"));
    }

    #[tokio::test]
    async fn llm_triager_falls_back_on_malformed_llm_output() {
        struct JunkLlm;
        #[async_trait::async_trait]
        impl symbiotic_core::protocol::LlmClient for JunkLlm {
            async fn chat(
                &self,
                _: &[symbiotic_core::protocol::ChatMessage],
                _: bool,
            ) -> anyhow::Result<String> {
                Ok("not json at all".to_string())
            }
        }
        let triager = LlmTriager::new(&JunkLlm, TriageConfig::default());
        let f = finding_with(FindingSeverity::Critical, GoalAlignment::InScope);
        let ctx = TriageContext {
            autonomy: Autonomy::Auto,
        };
        let raw = triager.triage(&f, &ctx).await.unwrap();
        // Declarative fallback: Critical always escalates.
        assert_eq!(raw.disposition, FindingDisposition::Escalate);
        assert!(raw.rationale.contains("critical_escalate"));
    }

    #[test]
    fn autonomy_and_config_serde() {
        let a = Autonomy::Manual;
        let s = serde_json::to_string(&a).unwrap();
        assert_eq!(s, "\"manual\"");
        let parsed: Autonomy = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, a);

        // TriageConfig default roundtrip (no serde derive — just ensure
        // the Default values are sane).
        let c = TriageConfig::default();
        assert_eq!(c.out_of_scope_defer_max_severity, FindingSeverity::Medium);
        assert!(c.escalate_on_peer_disagreement);
    }
}
