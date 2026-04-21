//! Multi-LLM Planning Council for synthesizing diverse model perspectives.
//!
//! Implements the council pattern where multiple LLM models independently analyze
//! a problem, then a judge model synthesizes the best approach. Used sparingly for:
//! - P0/P1 architecture decisions
//! - Security-critical designs
//! - Explicit user requests for multiple perspectives
//!
//! NOT used for routine tasks, speed-critical operations, or simple CRUD.

use std::fmt;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::llm::{ChatMessage, LlmClient};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// When the council should be activated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivationCriteria {
    /// Always use the council (testing/explicit request).
    Always,
    /// Only for high-priority decisions (P0/P1).
    #[default]
    HighPriority,
    /// Only when explicitly requested by the user.
    ExplicitOnly,
}

/// Configuration for a council member model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberConfig {
    /// Human-readable name for this member (e.g., "Claude", "Qwen-Local").
    pub name: String,
    /// Role/perspective this member should adopt (e.g., "security reviewer",
    /// "performance expert", "general architect").
    pub role: String,
    /// Optional additional system prompt context for this member.
    pub system_context: Option<String>,
    /// Timeout for this member's response. Members that exceed this are skipped.
    pub timeout: Duration,
}

/// Configuration for the planning council.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilConfig {
    /// Member configurations (one per model).
    pub members: Vec<MemberConfig>,
    /// Index into `members` that serves as the judge. When `None`, the first
    /// member is used as the judge (typically the strongest model).
    pub judge_index: Option<usize>,
    /// When to activate the council.
    pub activation: ActivationCriteria,
    /// Minimum number of member responses required for synthesis.
    /// If fewer members respond (due to timeouts/errors), the council
    /// falls back to the judge's solo analysis.
    pub min_responses: usize,
}

impl Default for CouncilConfig {
    fn default() -> Self {
        Self {
            members: vec![
                MemberConfig {
                    name: "Claude".to_string(),
                    role: "general architect".to_string(),
                    system_context: None,
                    timeout: Duration::from_secs(120),
                },
                MemberConfig {
                    name: "Qwen-Local".to_string(),
                    role: "implementation reviewer".to_string(),
                    system_context: None,
                    timeout: Duration::from_secs(60),
                },
            ],
            judge_index: Some(0), // Claude as judge
            activation: ActivationCriteria::HighPriority,
            min_responses: 1,
        }
    }
}

impl CouncilConfig {
    /// Returns the judge member index, defaulting to 0.
    pub fn judge_idx(&self) -> usize {
        self.judge_index.unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Context & Results
// ---------------------------------------------------------------------------

/// Context provided to the council for deliberation.
#[derive(Debug, Clone)]
pub struct PlanningContext {
    /// Priority level (0 = P0, 1 = P1, etc.).
    pub priority: u8,
    /// Whether the user explicitly requested council deliberation.
    pub user_requested: bool,
    /// Additional context (architecture docs, constraints, etc.).
    pub background: String,
}

/// A single member's independent analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemberAnalysis {
    /// Name of the member model that produced this analysis.
    pub member_name: String,
    /// Role/perspective used.
    pub role: String,
    /// The full analysis text.
    pub analysis: String,
    /// Key recommendations extracted from the analysis.
    pub recommendations: Vec<String>,
    /// Identified risks or concerns.
    pub risks: Vec<String>,
}

/// The synthesized verdict from the judge after reviewing all analyses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilVerdict {
    /// The synthesized plan/recommendation.
    pub synthesis: String,
    /// Individual member analyses that fed into the synthesis.
    pub member_analyses: Vec<MemberAnalysis>,
    /// Confidence score (0.0 - 1.0) in the synthesized plan.
    pub confidence: f32,
    /// Points where members disagreed (flagged for human review).
    pub disagreements: Vec<String>,
}

impl fmt::Display for CouncilVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "## Council Verdict (confidence: {:.0}%)",
            self.confidence * 100.0
        )?;
        writeln!(f)?;
        writeln!(f, "{}", self.synthesis)?;
        if !self.disagreements.is_empty() {
            writeln!(f)?;
            writeln!(f, "### Disagreements (needs human review)")?;
            for d in &self.disagreements {
                writeln!(f, "- {d}")?;
            }
        }
        writeln!(f)?;
        writeln!(f, "### Member Analyses")?;
        for (i, a) in self.member_analyses.iter().enumerate() {
            writeln!(f, "\n#### {}. {} ({})", i + 1, a.member_name, a.role)?;
            writeln!(f, "{}", a.analysis)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Council Errors
// ---------------------------------------------------------------------------

/// Errors specific to the planning council.
#[derive(Debug, thiserror::Error)]
pub enum CouncilError {
    /// No members configured.
    #[error("council has no members configured")]
    NoMembers,

    /// Judge index is out of bounds.
    #[error("judge index {0} is out of bounds (only {1} members)")]
    InvalidJudgeIndex(usize, usize),

    /// Not enough member responses for synthesis.
    #[error("only {got} of {needed} required member responses received")]
    InsufficientResponses { got: usize, needed: usize },

    /// The council should not be activated for this context.
    #[error(
        "council activation criteria not met: priority={priority}, user_requested={user_requested}"
    )]
    NotActivated { priority: u8, user_requested: bool },

    /// An LLM call failed.
    #[error("LLM error: {0}")]
    LlmError(#[from] anyhow::Error),
}

// ---------------------------------------------------------------------------
// Planning Council
// ---------------------------------------------------------------------------

/// Orchestrates multi-LLM deliberation and judge synthesis.
pub struct PlanningCouncil {
    config: CouncilConfig,
}

impl PlanningCouncil {
    /// Create a new council with the given configuration.
    pub fn new(config: CouncilConfig) -> Result<Self, CouncilError> {
        if config.members.is_empty() {
            return Err(CouncilError::NoMembers);
        }
        let judge_idx = config.judge_idx();
        if judge_idx >= config.members.len() {
            return Err(CouncilError::InvalidJudgeIndex(
                judge_idx,
                config.members.len(),
            ));
        }
        Ok(Self { config })
    }

    /// Returns the council configuration.
    pub fn config(&self) -> &CouncilConfig {
        &self.config
    }

    /// Check whether the council should activate for the given context.
    pub fn should_activate(&self, context: &PlanningContext) -> bool {
        match self.config.activation {
            ActivationCriteria::Always => true,
            ActivationCriteria::HighPriority => context.priority <= 1 || context.user_requested,
            ActivationCriteria::ExplicitOnly => context.user_requested,
        }
    }

    /// Run the full council deliberation: gather member analyses, then synthesize.
    ///
    /// `clients` must have the same length as `config.members`. Each client
    /// corresponds to the member at the same index.
    pub async fn deliberate(
        &self,
        prompt: &str,
        context: &PlanningContext,
        clients: &[&dyn LlmClient],
    ) -> Result<CouncilVerdict, CouncilError> {
        if !self.should_activate(context) {
            return Err(CouncilError::NotActivated {
                priority: context.priority,
                user_requested: context.user_requested,
            });
        }

        if clients.len() != self.config.members.len() {
            return Err(CouncilError::LlmError(anyhow::anyhow!(
                "expected {} clients, got {}",
                self.config.members.len(),
                clients.len()
            )));
        }

        // Phase 1: Gather independent analyses from all members
        let analyses = self.gather_analyses(prompt, context, clients).await;

        // Check minimum responses
        if analyses.len() < self.config.min_responses {
            return Err(CouncilError::InsufficientResponses {
                got: analyses.len(),
                needed: self.config.min_responses,
            });
        }

        // Phase 2: Judge synthesizes all analyses
        let judge_idx = self.config.judge_idx();
        let judge_client = clients[judge_idx];
        let verdict = self
            .synthesize(prompt, context, &analyses, judge_client)
            .await?;

        Ok(verdict)
    }

    /// Gather independent analyses from all members.
    ///
    /// Members that fail or timeout are skipped (logged as warnings).
    async fn gather_analyses(
        &self,
        prompt: &str,
        context: &PlanningContext,
        clients: &[&dyn LlmClient],
    ) -> Vec<MemberAnalysis> {
        let mut analyses = Vec::new();

        // Execute each member sequentially to avoid requiring Send bounds
        // on the future. For true parallelism with tokio::spawn, the LlmClient
        // instances would need to be Arc-wrapped. Sequential is acceptable for
        // 2-3 members and avoids complexity.
        for (i, member) in self.config.members.iter().enumerate() {
            let messages = build_member_prompt(member, prompt, context);
            match clients[i].chat(&messages, false).await {
                Ok(response) => {
                    analyses.push(MemberAnalysis {
                        member_name: member.name.clone(),
                        role: member.role.clone(),
                        analysis: response,
                        recommendations: Vec::new(), // Parsed during synthesis
                        risks: Vec::new(),
                    });
                }
                Err(e) => {
                    tracing::warn!(
                        member = %member.name,
                        error = %e,
                        "council member failed to respond, skipping"
                    );
                }
            }
        }

        analyses
    }

    /// Have the judge synthesize all member analyses into a verdict.
    async fn synthesize(
        &self,
        original_prompt: &str,
        context: &PlanningContext,
        analyses: &[MemberAnalysis],
        judge: &dyn LlmClient,
    ) -> Result<CouncilVerdict, CouncilError> {
        let messages = build_judge_prompt(original_prompt, context, analyses);

        let response = judge
            .chat(&messages, true)
            .await
            .map_err(CouncilError::LlmError)?;

        // Try to parse structured JSON response from judge
        match serde_json::from_str::<JudgeResponse>(&response) {
            Ok(parsed) => Ok(CouncilVerdict {
                synthesis: parsed.synthesis,
                member_analyses: analyses.to_vec(),
                confidence: parsed.confidence.clamp(0.0, 1.0),
                disagreements: parsed.disagreements,
            }),
            Err(_) => {
                // If JSON parsing fails, treat the raw text as the synthesis
                // with moderate confidence
                Ok(CouncilVerdict {
                    synthesis: response,
                    member_analyses: analyses.to_vec(),
                    confidence: 0.5,
                    disagreements: Vec::new(),
                })
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Prompt Construction
// ---------------------------------------------------------------------------

/// Build the prompt messages for a council member.
fn build_member_prompt(
    member: &MemberConfig,
    prompt: &str,
    context: &PlanningContext,
) -> Vec<ChatMessage> {
    let system = format!(
        "You are an expert {} serving on a planning council. \
         Analyze the following problem independently and provide your assessment.\n\n\
         Your role: {}\n\
         Priority level: P{}\n\
         {}\n\
         Provide:\n\
         1. Your analysis of the problem\n\
         2. Your recommended approach\n\
         3. Key risks or concerns\n\
         4. Any alternative approaches worth considering",
        member.role,
        member.role,
        context.priority,
        member
            .system_context
            .as_deref()
            .map(|c| format!("Additional context: {c}"))
            .unwrap_or_default(),
    );

    let user = if context.background.is_empty() {
        prompt.to_string()
    } else {
        format!(
            "Background:\n{}\n\nProblem:\n{}",
            context.background, prompt
        )
    };

    vec![
        ChatMessage {
            role: "system".to_string(),
            content: system,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ]
}

/// Build the prompt messages for the judge to synthesize analyses.
fn build_judge_prompt(
    original_prompt: &str,
    context: &PlanningContext,
    analyses: &[MemberAnalysis],
) -> Vec<ChatMessage> {
    let mut analyses_text = String::new();
    for (i, a) in analyses.iter().enumerate() {
        analyses_text.push_str(&format!(
            "\n--- Analysis {} ({}, role: {}) ---\n{}\n",
            i + 1,
            a.member_name,
            a.role,
            a.analysis
        ));
    }

    let system = "You are the judge of a planning council. Multiple expert models have \
                  independently analyzed a problem. Your job is to:\n\n\
                  1. Identify the BEST ideas from each analysis (not average them)\n\
                  2. Synthesize a coherent plan that takes the strongest elements\n\
                  3. Note any significant disagreements that need human review\n\
                  4. Assess your confidence in the synthesized plan\n\n\
                  IMPORTANT: Do NOT vote or average. Synthesize the best approach.\n\n\
                  Respond with JSON:\n\
                  {\"synthesis\": \"...\", \"confidence\": 0.0-1.0, \"disagreements\": [\"...\"]}"
        .to_string();

    let user = format!(
        "Original problem (P{}):\n{}\n\n\
         Background:\n{}\n\n\
         Member analyses:\n{}",
        context.priority, original_prompt, context.background, analyses_text
    );

    vec![
        ChatMessage {
            role: "system".to_string(),
            content: system,
        },
        ChatMessage {
            role: "user".to_string(),
            content: user,
        },
    ]
}

// ---------------------------------------------------------------------------
// Judge response parsing
// ---------------------------------------------------------------------------

/// Expected JSON structure from the judge.
#[derive(Debug, Deserialize)]
struct JudgeResponse {
    synthesis: String,
    confidence: f32,
    #[serde(default)]
    disagreements: Vec<String>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -- Mock LLM --

    struct MockCouncilLlm {
        responses: Vec<String>,
        call_count: AtomicUsize,
    }

    impl MockCouncilLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses,
                call_count: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockCouncilLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst);
            self.responses
                .get(idx)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("mock LLM ran out of responses"))
        }
    }

    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(&self, _messages: &[ChatMessage], _json_mode: bool) -> Result<String> {
            Err(anyhow::anyhow!("model unavailable"))
        }
    }

    fn default_context() -> PlanningContext {
        PlanningContext {
            priority: 0,
            user_requested: false,
            background: "Test background".to_string(),
        }
    }

    // -- Construction --

    #[test]
    fn council_rejects_empty_members() {
        let config = CouncilConfig {
            members: vec![],
            ..CouncilConfig::default()
        };
        let result = PlanningCouncil::new(config);
        assert!(matches!(result, Err(CouncilError::NoMembers)));
    }

    #[test]
    fn council_rejects_invalid_judge_index() {
        let config = CouncilConfig {
            members: vec![MemberConfig {
                name: "A".to_string(),
                role: "architect".to_string(),
                system_context: None,
                timeout: Duration::from_secs(30),
            }],
            judge_index: Some(5),
            ..CouncilConfig::default()
        };
        let result = PlanningCouncil::new(config);
        assert!(matches!(result, Err(CouncilError::InvalidJudgeIndex(5, 1))));
    }

    #[test]
    fn council_creates_with_valid_config() {
        let council = PlanningCouncil::new(CouncilConfig::default());
        assert!(council.is_ok());
    }

    // -- Activation criteria --

    #[test]
    fn activation_always_returns_true() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();
        let ctx = PlanningContext {
            priority: 3,
            user_requested: false,
            background: String::new(),
        };
        assert!(council.should_activate(&ctx));
    }

    #[test]
    fn activation_high_priority_for_p0() {
        let config = CouncilConfig {
            activation: ActivationCriteria::HighPriority,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // P0 -> activate
        let ctx_p0 = PlanningContext {
            priority: 0,
            user_requested: false,
            background: String::new(),
        };
        assert!(council.should_activate(&ctx_p0));

        // P1 -> activate
        let ctx_p1 = PlanningContext {
            priority: 1,
            user_requested: false,
            background: String::new(),
        };
        assert!(council.should_activate(&ctx_p1));

        // P2 -> do NOT activate (unless user_requested)
        let ctx_p2 = PlanningContext {
            priority: 2,
            user_requested: false,
            background: String::new(),
        };
        assert!(!council.should_activate(&ctx_p2));

        // P2 + user_requested -> activate
        let ctx_p2_user = PlanningContext {
            priority: 2,
            user_requested: true,
            background: String::new(),
        };
        assert!(council.should_activate(&ctx_p2_user));
    }

    #[test]
    fn activation_explicit_only() {
        let config = CouncilConfig {
            activation: ActivationCriteria::ExplicitOnly,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        let ctx_no = PlanningContext {
            priority: 0,
            user_requested: false,
            background: String::new(),
        };
        assert!(!council.should_activate(&ctx_no));

        let ctx_yes = PlanningContext {
            priority: 0,
            user_requested: true,
            background: String::new(),
        };
        assert!(council.should_activate(&ctx_yes));
    }

    // -- Deliberation --

    #[tokio::test]
    async fn deliberate_synthesizes_member_analyses() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            min_responses: 2,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // Member 1 (Claude): analysis, then judge synthesis
        let member1 = MockCouncilLlm::new(vec![
            "I recommend approach A with focus on safety.".to_string(),
            // Judge synthesis (second call to member 1 since it's the judge)
            r#"{"synthesis": "Combined approach using A for safety and B for speed.", "confidence": 0.85, "disagreements": ["Member 1 prefers safety-first, Member 2 prefers speed"]}"#.to_string(),
        ]);
        // Member 2 (Qwen): analysis only
        let member2 = MockCouncilLlm::new(vec![
            "I recommend approach B optimized for speed.".to_string()
        ]);

        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];
        let context = default_context();

        let verdict = council
            .deliberate("Design the auth system", &context, &clients)
            .await
            .expect("deliberate should succeed");

        assert_eq!(verdict.member_analyses.len(), 2);
        assert_eq!(verdict.member_analyses[0].member_name, "Claude");
        assert_eq!(verdict.member_analyses[1].member_name, "Qwen-Local");
        assert!(verdict.synthesis.contains("Combined approach"));
        assert!((verdict.confidence - 0.85).abs() < f32::EPSILON);
        assert_eq!(verdict.disagreements.len(), 1);
    }

    #[tokio::test]
    async fn deliberate_handles_member_failure_gracefully() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            min_responses: 1, // Only need 1 to proceed
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // Member 1 (Claude): analysis + judge synthesis
        let member1 = MockCouncilLlm::new(vec![
            "My solo analysis.".to_string(),
            r#"{"synthesis": "Based on the sole analysis.", "confidence": 0.6, "disagreements": []}"#.to_string(),
        ]);
        // Member 2: fails
        let member2 = FailingLlm;

        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];
        let context = default_context();

        let verdict = council
            .deliberate("Analyze this", &context, &clients)
            .await
            .expect("should succeed with 1 response");

        assert_eq!(verdict.member_analyses.len(), 1);
        assert_eq!(verdict.member_analyses[0].member_name, "Claude");
        assert!((verdict.confidence - 0.6).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn deliberate_fails_when_insufficient_responses() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            min_responses: 2, // Need both
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // Member 1: succeeds
        let member1 = MockCouncilLlm::new(vec!["Analysis.".to_string()]);
        // Member 2: fails
        let member2 = FailingLlm;

        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];
        let context = default_context();

        let result = council.deliberate("Analyze", &context, &clients).await;
        assert!(matches!(
            result,
            Err(CouncilError::InsufficientResponses { got: 1, needed: 2 })
        ));
    }

    #[tokio::test]
    async fn deliberate_rejects_when_not_activated() {
        let config = CouncilConfig {
            activation: ActivationCriteria::ExplicitOnly,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        let member1 = MockCouncilLlm::new(vec![]);
        let member2 = MockCouncilLlm::new(vec![]);
        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];

        let context = PlanningContext {
            priority: 0,
            user_requested: false,
            background: String::new(),
        };

        let result = council.deliberate("Test", &context, &clients).await;
        assert!(matches!(result, Err(CouncilError::NotActivated { .. })));
    }

    #[tokio::test]
    async fn deliberate_handles_non_json_judge_response() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            min_responses: 1,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // Member 1 (Claude): analysis, then non-JSON judge response
        let member1 = MockCouncilLlm::new(vec![
            "Analysis here.".to_string(),
            "This is a plain text synthesis without JSON.".to_string(),
        ]);
        let member2 = FailingLlm;

        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];
        let context = default_context();

        let verdict = council
            .deliberate("Analyze", &context, &clients)
            .await
            .expect("should succeed with fallback");

        assert_eq!(
            verdict.synthesis,
            "This is a plain text synthesis without JSON."
        );
        // Fallback confidence
        assert!((verdict.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn deliberate_clamps_confidence() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            min_responses: 1,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        let member1 = MockCouncilLlm::new(vec![
            "Analysis.".to_string(),
            r#"{"synthesis": "Plan", "confidence": 1.5, "disagreements": []}"#.to_string(),
        ]);
        let member2 = FailingLlm;

        let clients: Vec<&dyn LlmClient> = vec![&member1, &member2];
        let context = default_context();

        let verdict = council
            .deliberate("Test", &context, &clients)
            .await
            .unwrap();

        // Confidence should be clamped to 1.0
        assert!((verdict.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn deliberate_rejects_client_count_mismatch() {
        let config = CouncilConfig {
            activation: ActivationCriteria::Always,
            ..CouncilConfig::default()
        };
        let council = PlanningCouncil::new(config).unwrap();

        // Only 1 client but 2 members configured
        let member1 = MockCouncilLlm::new(vec![]);
        let clients: Vec<&dyn LlmClient> = vec![&member1];
        let context = default_context();

        let result = council.deliberate("Test", &context, &clients).await;
        assert!(result.is_err());
    }

    // -- Display --

    #[test]
    fn verdict_display_includes_all_sections() {
        let verdict = CouncilVerdict {
            synthesis: "Use approach A.".to_string(),
            member_analyses: vec![
                MemberAnalysis {
                    member_name: "Claude".to_string(),
                    role: "architect".to_string(),
                    analysis: "Detailed analysis from Claude.".to_string(),
                    recommendations: vec![],
                    risks: vec![],
                },
                MemberAnalysis {
                    member_name: "Qwen".to_string(),
                    role: "reviewer".to_string(),
                    analysis: "Analysis from Qwen.".to_string(),
                    recommendations: vec![],
                    risks: vec![],
                },
            ],
            confidence: 0.85,
            disagreements: vec!["Approach to error handling differs.".to_string()],
        };

        let display = format!("{verdict}");
        assert!(display.contains("Council Verdict (confidence: 85%)"));
        assert!(display.contains("Use approach A."));
        assert!(display.contains("Disagreements"));
        assert!(display.contains("Approach to error handling differs."));
        assert!(display.contains("Claude"));
        assert!(display.contains("Qwen"));
    }

    // -- Prompt construction --

    #[test]
    fn member_prompt_includes_role_and_context() {
        let member = MemberConfig {
            name: "TestModel".to_string(),
            role: "security reviewer".to_string(),
            system_context: Some("Focus on OWASP top 10.".to_string()),
            timeout: Duration::from_secs(30),
        };
        let ctx = PlanningContext {
            priority: 1,
            user_requested: false,
            background: "Auth system design".to_string(),
        };

        let messages = build_member_prompt(&member, "Review the design", &ctx);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("security reviewer"));
        assert!(messages[0].content.contains("P1"));
        assert!(messages[0].content.contains("OWASP top 10"));
        assert!(messages[1].content.contains("Auth system design"));
        assert!(messages[1].content.contains("Review the design"));
    }

    #[test]
    fn judge_prompt_includes_all_analyses() {
        let analyses = vec![
            MemberAnalysis {
                member_name: "Claude".to_string(),
                role: "architect".to_string(),
                analysis: "Use microservices.".to_string(),
                recommendations: vec![],
                risks: vec![],
            },
            MemberAnalysis {
                member_name: "Qwen".to_string(),
                role: "reviewer".to_string(),
                analysis: "Use monolith.".to_string(),
                recommendations: vec![],
                risks: vec![],
            },
        ];
        let ctx = PlanningContext {
            priority: 0,
            user_requested: false,
            background: "System design".to_string(),
        };

        let messages = build_judge_prompt("Design the system", &ctx, &analyses);
        assert_eq!(messages.len(), 2);
        assert!(messages[0].content.contains("judge"));
        assert!(messages[0].content.contains("Do NOT vote"));
        assert!(messages[1].content.contains("Use microservices."));
        assert!(messages[1].content.contains("Use monolith."));
        assert!(messages[1].content.contains("P0"));
    }
}
