//! The Distillery Pipeline for intake (Architecture 2.0)
//!
//! This pipeline replaces the flat file-drop model with a five-stage
//! unidirectional flow: Reduce -> Reflect -> Verify -> Reweave -> Archive.
//!
//! Each stage uses direct `llm.chat()` calls (strict prompt chaining)
//! rather than the ReAct agent loop, ensuring deterministic extraction.

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use symbiotic_agents::llm::{ChatMessage, LlmClient};
use symbiotic_context::redaction::RedactionEngine;
use symbiotic_core::MemorySpace;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Reweave commands — structured actions for memory mutation
// ---------------------------------------------------------------------------

/// Commands the LLM can issue during the Reweave stage.
///
/// During Reweave, the LLM reviews existing facts against new evidence and
/// outputs structured commands rather than free-form text rewrites.
///
/// - `Add` — create a new fact
/// - `Update` — modify an existing fact's content
/// - `Archive` — soft-delete a fact (sets `status = archived`, never hard-deletes)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ReweaveCommand {
    /// Add a new fact to the memory store.
    Add {
        entity_id: String,
        fact: String,
        #[serde(default)]
        fact_type: Option<String>,
    },
    /// Update an existing fact's content.
    Update { memory_id: String, fact: String },
    /// Archive (soft-delete) a fact. The fact remains in the database
    /// but is excluded from standard context retrieval.
    Archive { memory_id: String, reason: String },
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during the distillery pipeline.
#[derive(Debug, Error)]
pub enum DistilleryError {
    #[error("LLM call failed: {0}")]
    LlmFailed(String),

    #[error("failed to parse LLM output as JSON: {0}")]
    ParseFailed(String),

    #[error("filesystem IO error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("serialization error: {0}")]
    SerdeError(#[from] serde_json::Error),

    #[error("verification rejected all claims")]
    AllClaimsRejected,

    #[error("vault write failed: {0}")]
    VaultWriteFailed(String),
}

/// Shorthand result type for the distillery pipeline.
pub type DistilleryResult<T> = Result<T, DistilleryError>;

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Raw input to the distillery pipeline — a URL and its fetched content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawInput {
    pub source_url: String,
    pub raw_content: String,
}

/// A single atomic claim extracted from raw content during the Reduce stage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtomicClaim {
    /// Free-form claim content stripped of hedging and filler.
    pub content: String,
    /// Impact score (1-10): how fundamentally this claim changes the user's
    /// worldview or operational state.
    pub impact_score: u8,
    /// Reference back to the source (URL or note identifier).
    pub source_ref: String,
    /// When this claim was observed/extracted (ISO 8601).
    /// Used for temporal conflict resolution: newer claims take precedence
    /// over older ones when a contradiction is detected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
}

/// A proposed link between a new claim and an existing knowledge node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedLink {
    /// Index into the claims array identifying the source claim.
    pub source_claim_idx: usize,
    /// File-stem ID of the target knowledge node (e.g. `"rust-safety"`
    /// maps to `knowledge/rust-safety.md`).
    pub target_node_id: String,
    /// A short description of the relationship (e.g. `"supports"`,
    /// `"contradicts"`, `"extends"`).
    pub relationship: String,
    /// Which memory space the target node lives in. Defaults to
    /// `Knowledge` for backward compatibility.
    #[serde(default = "default_space")]
    pub target_space: MemorySpace,
}

fn default_space() -> MemorySpace {
    MemorySpace::Knowledge
}

/// Output of the Reflect stage — claims annotated with proposed graph links.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReflectedGraph {
    pub claims: Vec<AtomicClaim>,
    pub proposed_links: Vec<ProposedLink>,
}

/// A validated version of [`ReflectedGraph`] where all claims pass validation
/// and all proposed links reference files that exist on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedGraph {
    pub claims: Vec<AtomicClaim>,
    pub proposed_links: Vec<ProposedLink>,
}

// `MemorySpace` is defined in `symbiotic-core` and imported at the top of
// this file via `use symbiotic_core::MemorySpace`.

/// Configuration for the distillery pipeline stages.
#[derive(Debug, Clone)]
pub struct DistilleryStageConfig {
    /// Root path to the Archive directory (`knowledge-base/`).
    /// Files are expected at `{kb_root}/knowledge/{link_id}.md`.
    pub kb_root: PathBuf,
    /// When true (default), PII is redacted from content before it is sent
    /// to LLMs during the Reduce, Reflect, Classify, and Reweave stages.
    /// The original unredacted content is preserved for Archive (stage 5).
    pub redact_llm_prompts: bool,
}

impl Default for DistilleryStageConfig {
    fn default() -> Self {
        Self {
            kb_root: PathBuf::from("knowledge-base"),
            redact_llm_prompts: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared PII redaction helper
// ---------------------------------------------------------------------------

/// Redacts PII from text if the config has `redact_llm_prompts` enabled.
///
/// Returns the original text unchanged when redaction is disabled. Used by
/// every distillery stage that sends user content to an LLM to ensure PII
/// is stripped from prompts while the original content is preserved for
/// archival (stage 5).
fn maybe_redact(text: &str, config: &DistilleryStageConfig) -> String {
    if config.redact_llm_prompts {
        RedactionEngine::new().redact(text)
    } else {
        text.to_string()
    }
}

// ---------------------------------------------------------------------------
// Stage 1: Reduce (Enzymatic Breakdown)
// ---------------------------------------------------------------------------

/// Strips fluff, framing, and hedging. Extracts only the atomic building blocks.
///
/// When `config.redact_llm_prompts` is enabled, PII in the raw content is
/// redacted before it is sent to the LLM. The original `RawInput` is not
/// modified so that downstream stages (Archive) preserve the full text.
pub async fn reduce(
    input: RawInput,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<Vec<AtomicClaim>> {
    let system_prompt = "\
You are the Enzymatic Breakdown agent. Your only job is to extract atomic claims from the provided text. \
Strip all framing, hedging, and conversational filler. \
Assign an impact_score (1-10) based on how fundamentally this claim changes the user's worldview or operational state. \
Respond ONLY with a JSON array matching the schema: \
[{\"content\": \"string\", \"impact_score\": number, \"source_ref\": \"string\"}].";

    let redacted_content = maybe_redact(&input.raw_content, config);

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!(
                "URL: {}\n\nContent:\n{}",
                input.source_url, redacted_content
            ),
        },
    ];

    let response = llm
        .chat(&messages, true)
        .await
        .map_err(|e| DistilleryError::LlmFailed(e.to_string()))?;

    let claims: Vec<AtomicClaim> =
        serde_json::from_str(&response).map_err(|e| DistilleryError::ParseFailed(e.to_string()))?;

    Ok(claims)
}

// ---------------------------------------------------------------------------
// Stage 2: Reflect (Circulation)
// ---------------------------------------------------------------------------

/// Identifies where new atomic claims fit into the existing Neural Graph.
///
/// When `config.redact_llm_prompts` is enabled, PII in the claims and
/// graph context is redacted before being sent to the LLM.
pub async fn reflect(
    claims: Vec<AtomicClaim>,
    current_graph_context: &str,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<ReflectedGraph> {
    let system_prompt = "\
You are the Circulation agent. Your job is to find connections between new claims and existing knowledge. \
Compare the new claims against the provided current_graph_context. \
Respond ONLY with a JSON object matching the schema: \
{\"claims\": [{\"content\": \"string\", \"impact_score\": number, \"source_ref\": \"string\"}], \
\"proposed_links\": [{\"source_claim_idx\": number, \"target_node_id\": \"string\", \"relationship\": \"string\"}]}. \
CRITICAL: Only output proposed_links for target_node_id values that actually exist in the provided context. \
Do not hallucinate new nodes. \
The source_claim_idx must be a valid zero-based index into the claims array. \
The relationship should be a short verb phrase like \"supports\", \"contradicts\", \"extends\", or \"exemplifies\".";

    let claims_json = serde_json::to_string(&claims)?;
    let redacted_claims = maybe_redact(&claims_json, config);
    let redacted_context = maybe_redact(current_graph_context, config);

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!(
                "Current Graph Nodes:\n{}\n\nNew Claims to Integrate:\n{}",
                redacted_context, redacted_claims
            ),
        },
    ];

    let response = llm
        .chat(&messages, true)
        .await
        .map_err(|e| DistilleryError::LlmFailed(e.to_string()))?;

    let graph: ReflectedGraph =
        serde_json::from_str(&response).map_err(|e| DistilleryError::ParseFailed(e.to_string()))?;

    Ok(graph)
}

// ---------------------------------------------------------------------------
// Stage 3a: Semantic Verify (LLM-assisted validation)
// ---------------------------------------------------------------------------

/// A single claim validation result from the LLM semantic verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticClaimVerdict {
    /// Zero-based index into the claims array.
    pub claim_index: usize,
    /// Whether the LLM judges the claim as factually supported by the source.
    pub valid: bool,
    /// Reason for the verdict.
    pub reason: String,
}

/// Result of the semantic verification stage.
#[derive(Debug, Clone)]
pub struct SemanticVerifyResult {
    /// The filtered graph with invalid claims removed.
    pub graph: VerifiedGraph,
    /// Claims that were rejected by the LLM with reasons.
    pub rejected_claims: Vec<(AtomicClaim, String)>,
}

/// LLM-assisted semantic verification of claims against the original source text.
///
/// Sends the verified claims and the original source text to an LLM, asking it
/// to validate that each claim is factually supported by the source. Claims the
/// LLM flags as invalid (hallucinated, unsupported, or distorted) are removed.
///
/// When `redact_llm_prompts` is true, PII in the source text is redacted before
/// sending to the LLM.
///
/// If the LLM call fails, all claims pass through with a warning (graceful
/// fallthrough). This ensures the pipeline never breaks due to LLM unavailability.
pub async fn verify_with_semantic(
    graph: VerifiedGraph,
    source_text: &str,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> SemanticVerifyResult {
    let system_prompt = "\
You are a Fact Verification agent. You will be given a list of claims and the original source text. \
For each claim, determine if it is factually supported by the source text. \
Check for: hallucination, distortion, unsupported inferences, and misattribution. \
Respond ONLY with a JSON array matching the schema: \
[{\"claim_index\": number, \"valid\": boolean, \"reason\": \"string\"}]. \
Return exactly one entry per claim, in order.";

    let redacted_source = maybe_redact(source_text, config);

    let claims_text: String = graph
        .claims
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{i}. {}", c.content))
        .collect::<Vec<_>>()
        .join("\n");

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!("Source Text:\n{redacted_source}\n\nClaims to Verify:\n{claims_text}"),
        },
    ];

    let verdicts: Vec<SemanticClaimVerdict> = match llm.chat(&messages, true).await {
        Ok(response) => match serde_json::from_str(&response) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Failed to parse semantic verify LLM response — passing all claims through"
                );
                return SemanticVerifyResult {
                    graph,
                    rejected_claims: vec![],
                };
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Semantic verify LLM call failed — passing all claims through"
            );
            return SemanticVerifyResult {
                graph,
                rejected_claims: vec![],
            };
        }
    };

    // Build set of invalid claim indices
    let mut rejected_claims = Vec::new();
    let mut invalid_indices = std::collections::HashSet::new();
    for verdict in &verdicts {
        if !verdict.valid && verdict.claim_index < graph.claims.len() {
            invalid_indices.insert(verdict.claim_index);
            rejected_claims.push((
                graph.claims[verdict.claim_index].clone(),
                verdict.reason.clone(),
            ));
        }
    }

    // Filter claims, keeping only valid ones
    let filtered_claims: Vec<AtomicClaim> = graph
        .claims
        .into_iter()
        .enumerate()
        .filter(|(i, _)| !invalid_indices.contains(i))
        .map(|(_, c)| c)
        .collect();

    // Re-index links to match the new claims array.
    // Build a mapping from old index to new index. The original claim count
    // is the sum of kept + rejected claims.
    let original_claim_count = filtered_claims.len() + invalid_indices.len();
    let mut old_to_new: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    let mut new_idx = 0usize;
    for old_idx in 0..original_claim_count {
        if !invalid_indices.contains(&old_idx) {
            old_to_new.insert(old_idx, new_idx);
            new_idx += 1;
        }
    }

    let filtered_links: Vec<ProposedLink> = graph
        .proposed_links
        .into_iter()
        .filter_map(|mut link| {
            if let Some(&new_idx) = old_to_new.get(&link.source_claim_idx) {
                link.source_claim_idx = new_idx;
                Some(link)
            } else {
                None // Link referenced a rejected claim
            }
        })
        .collect();

    SemanticVerifyResult {
        graph: VerifiedGraph {
            claims: filtered_claims,
            proposed_links: filtered_links,
        },
        rejected_claims,
    }
}

// ---------------------------------------------------------------------------
// Stage 3: Verify (Validation Gate)
// ---------------------------------------------------------------------------

/// Validates the reflected graph before it is used to rewrite knowledge files.
///
/// - Strips claims with empty `content` or `impact_score` outside 1..=10.
/// - Strips proposed links whose `target_node_id` does not correspond to an
///   existing file at `{kb_root}/knowledge/{target_node_id}.md`.
/// - Strips proposed links whose `source_claim_idx` is out of bounds.
/// - Returns `Err(AllClaimsRejected)` only if every claim was invalid.
pub fn verify(
    graph: ReflectedGraph,
    config: &DistilleryStageConfig,
) -> DistilleryResult<VerifiedGraph> {
    let valid_claims: Vec<AtomicClaim> = graph
        .claims
        .into_iter()
        .filter(|c| !c.content.trim().is_empty() && (1..=10).contains(&c.impact_score))
        .collect();

    if valid_claims.is_empty() {
        return Err(DistilleryError::AllClaimsRejected);
    }

    let claims_len = valid_claims.len();

    let valid_links: Vec<ProposedLink> = graph
        .proposed_links
        .into_iter()
        .filter(|link| {
            // source_claim_idx must be within bounds of the *validated* claims
            if link.source_claim_idx >= claims_len {
                return false;
            }
            // target_node_id must correspond to an existing file
            let path = config
                .kb_root
                .join("knowledge")
                .join(format!("{}.md", link.target_node_id));
            path.exists()
        })
        .collect();

    Ok(VerifiedGraph {
        claims: valid_claims,
        proposed_links: valid_links,
    })
}

// ---------------------------------------------------------------------------
// Stage 4: Reweave (Tissue Building)
// ---------------------------------------------------------------------------

/// Modifies existing notes in the Neural Graph to incorporate newly gained knowledge.
///
/// For each unique `target_node_id` in `proposed_links`, reads the file at
/// `{kb_root}/knowledge/{target_node_id}.md`, collects all claims linked to it
/// (with their relationship descriptions), asks the LLM to rewrite the note,
/// and writes the result back.
///
/// When `config.redact_llm_prompts` is enabled, PII in note content and claims
/// is redacted before being sent to the LLM. The rewritten note (from the LLM)
/// is written back as-is since it was produced from redacted input.
///
/// Files that do not exist are silently skipped.
pub async fn reweave(
    graph: &VerifiedGraph,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<u32> {
    let system_prompt = "\
You are the Tissue Building agent. I will provide an old note and a set of new claims \
with their relationship to this note (e.g. \"supports\", \"contradicts\", \"extends\"). \
If this old note were written today, knowing what we now know, what would be different? \
Rewrite the note to seamlessly incorporate the new knowledge. \
Respond ONLY with the raw Markdown of the new note. Do not wrap in code fences.";

    // Group links by target_node_id so each note is rewritten once with all
    // relevant claims rather than once per link.
    let mut links_by_target: std::collections::HashMap<&str, Vec<&ProposedLink>> =
        std::collections::HashMap::new();
    for link in &graph.proposed_links {
        links_by_target
            .entry(link.target_node_id.as_str())
            .or_default()
            .push(link);
    }

    let mut rewritten_count: u32 = 0;

    for (target_id, links) in &links_by_target {
        let note_path = config
            .kb_root
            .join("knowledge")
            .join(format!("{target_id}.md"));

        // Skip if file does not exist (verify should have caught this, but be defensive)
        if !note_path.exists() {
            continue;
        }

        let old_note_content = std::fs::read_to_string(&note_path)?;

        // Build a context block listing each relevant claim and its relationship
        let mut claims_context = String::new();
        for link in links {
            if let Some(claim) = graph.claims.get(link.source_claim_idx) {
                claims_context.push_str(&format!("- [{}] {}\n", link.relationship, claim.content));
            }
        }

        // Redact PII from note content and claims before sending to LLM
        let redacted_note = maybe_redact(&old_note_content, config);
        let redacted_claims = maybe_redact(&claims_context, config);

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: format!(
                    "Old Note ({target_id}):\n{redacted_note}\n\nNew Claims (with relationship):\n{redacted_claims}"
                ),
            },
        ];

        let rewritten_note = llm
            .chat(&messages, false)
            .await
            .map_err(|e| DistilleryError::LlmFailed(e.to_string()))?;

        std::fs::write(&note_path, rewritten_note)?;
        rewritten_count += 1;
    }

    Ok(rewritten_count)
}

// ---------------------------------------------------------------------------
// Stage 4b: Surgical Reweave (Vault-as-Truth)
// ---------------------------------------------------------------------------

/// Surgical reweave: adds new claims as facts and archives contradicted facts.
///
/// Unlike [`reweave`] which sends the entire note to an LLM for rewriting,
/// this function makes surgical Markdown edits via [`VaultWriter`]:
/// - "supports"/"extends"/"exemplifies" → `add_fact()` to `## Facts`
/// - "contradicts" → `archive_fact()` the most similar existing fact + `add_fact()`
///
/// No LLM call is needed — the relationship type from the Reflect stage
/// determines the action.
///
/// Returns a list of [`VaultMutation`] descriptors for commit message generation.
pub fn reweave_surgical(
    graph: &VerifiedGraph,
    config: &DistilleryStageConfig,
) -> DistilleryResult<SurgicalReweaveReport> {
    use symbiotic_memory::vault_writer::{NewFactMetadata, VaultWriter};

    let writer = VaultWriter::new(&config.kb_root);
    let mut report = SurgicalReweaveReport::default();

    // Group links by target_node_id
    let mut links_by_target: std::collections::HashMap<&str, Vec<&ProposedLink>> =
        std::collections::HashMap::new();
    for link in &graph.proposed_links {
        links_by_target
            .entry(link.target_node_id.as_str())
            .or_default()
            .push(link);
    }

    for (target_id, links) in &links_by_target {
        // Read existing facts for contradiction matching.
        // Search all vault subdirectories (not just "entities") to match
        // VaultWriter's behavior.
        let mut existing_facts = read_active_facts(&config.kb_root, target_id);

        for link in links {
            if let Some(claim) = graph.claims.get(link.source_claim_idx) {
                let source = format!("intake:{}", slug_from_url(&claim.source_ref));

                // Handle contradiction: archive the most similar existing fact
                if link.relationship == "contradicts" && !existing_facts.is_empty() {
                    if let Some(contradicted) = find_most_similar(&claim.content, &existing_facts) {
                        match writer.archive_fact(
                            target_id,
                            &contradicted,
                            &format!("contradicted by: {}", truncate(&claim.content, 80)),
                        ) {
                            Ok(m) => {
                                report.mutations.push(m);
                                report.facts_archived += 1;
                                // Remove from local list so subsequent claims
                                // don't try to archive the same fact again.
                                existing_facts.retain(|f| f != &contradicted);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to archive contradicted fact in {}: {}",
                                    target_id,
                                    e
                                );
                            }
                        }
                    }
                }

                // Add the new claim as a fact
                let metadata = NewFactMetadata {
                    source,
                    fact_type: relationship_to_fact_type(&link.relationship),
                    confidence: Some(claim_confidence(claim)),
                };

                match writer.add_fact(target_id, &claim.content, &metadata) {
                    Ok(m) => {
                        report.mutations.push(m);
                        report.facts_added += 1;
                    }
                    Err(symbiotic_memory::vault_writer::VaultWriteError::EntityNotFound(_)) => {
                        // Entity doesn't exist — create it first, then add the fact
                        match writer.create_entity(
                            target_id,
                            target_id,
                            symbiotic_memory::EntityType::Concept,
                            MemorySpace::Knowledge,
                            None,
                        ) {
                            Ok(m) => {
                                report.mutations.push(m);
                                report.entities_created += 1;
                                // Retry adding the fact
                                if let Ok(m2) =
                                    writer.add_fact(target_id, &claim.content, &metadata)
                                {
                                    report.mutations.push(m2);
                                    report.facts_added += 1;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to create entity {}: {}", target_id, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to add fact to {}: {}", target_id, e);
                    }
                }
            }
        }
    }

    Ok(report)
}

/// Like [`reweave_surgical`] but resolves note paths using each link's `target_space`.
pub fn reweave_surgical_with_spaces(
    graph: &VerifiedGraph,
    config: &DistilleryStageConfig,
) -> DistilleryResult<SurgicalReweaveReport> {
    use symbiotic_memory::vault_writer::{NewFactMetadata, VaultWriter};

    let writer = VaultWriter::new(&config.kb_root);
    let mut report = SurgicalReweaveReport::default();

    // Group links by (target_space, target_node_id)
    let mut links_by_target: std::collections::HashMap<(MemorySpace, &str), Vec<&ProposedLink>> =
        std::collections::HashMap::new();
    for link in &graph.proposed_links {
        links_by_target
            .entry((link.target_space, link.target_node_id.as_str()))
            .or_default()
            .push(link);
    }

    for ((space, target_id), links) in &links_by_target {
        let mut existing_facts = read_active_facts(&config.kb_root, target_id);

        for link in links {
            if let Some(claim) = graph.claims.get(link.source_claim_idx) {
                let source = format!("intake:{}", slug_from_url(&claim.source_ref));

                if link.relationship == "contradicts" && !existing_facts.is_empty() {
                    if let Some(contradicted) = find_most_similar(&claim.content, &existing_facts) {
                        match writer.archive_fact(
                            target_id,
                            &contradicted,
                            &format!("contradicted by: {}", truncate(&claim.content, 80)),
                        ) {
                            Ok(m) => {
                                report.mutations.push(m);
                                report.facts_archived += 1;
                                existing_facts.retain(|f| f != &contradicted);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to archive contradicted fact in {}: {}",
                                    target_id,
                                    e
                                );
                            }
                        }
                    }
                }

                let metadata = NewFactMetadata {
                    source,
                    fact_type: relationship_to_fact_type(&link.relationship),
                    confidence: Some(claim_confidence(claim)),
                };

                match writer.add_fact(target_id, &claim.content, &metadata) {
                    Ok(m) => {
                        report.mutations.push(m);
                        report.facts_added += 1;
                    }
                    Err(symbiotic_memory::vault_writer::VaultWriteError::EntityNotFound(_)) => {
                        match writer.create_entity(
                            target_id,
                            target_id,
                            symbiotic_memory::EntityType::Concept,
                            *space,
                            None,
                        ) {
                            Ok(m) => {
                                report.mutations.push(m);
                                report.entities_created += 1;
                                if let Ok(m2) =
                                    writer.add_fact(target_id, &claim.content, &metadata)
                                {
                                    report.mutations.push(m2);
                                    report.facts_added += 1;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to create entity {}: {}", target_id, e);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to add fact to {}: {}", target_id, e);
                    }
                }
            }
        }
    }

    Ok(report)
}

/// Report from a surgical reweave operation.
#[derive(Debug, Clone, Default)]
pub struct SurgicalReweaveReport {
    pub facts_added: usize,
    pub facts_archived: usize,
    pub entities_created: usize,
    pub mutations: Vec<symbiotic_memory::vault_writer::VaultMutation>,
}

impl SurgicalReweaveReport {
    /// Total notes affected (for backward compatibility with `notes_rewritten`).
    pub fn notes_affected(&self) -> u32 {
        let unique_files: std::collections::HashSet<&str> = self
            .mutations
            .iter()
            .map(|m| m.file_path.as_str())
            .collect();
        unique_files.len() as u32
    }
}

/// Canonical entity context for a targeted `vault.process` pass.
#[derive(Debug, Clone)]
pub struct EntityProcessTarget {
    pub entity_id: String,
    pub entity_name: String,
    pub entity_type: symbiotic_memory::EntityType,
    pub space: MemorySpace,
    pub active_facts: Vec<String>,
}

/// Report from a targeted entity-processing pass.
#[derive(Debug, Clone, Default)]
pub struct EntityProcessReport {
    pub claims_extracted: usize,
    pub claims_verified: usize,
    pub reweave: SurgicalReweaveReport,
}

// --- Surgical reweave helpers ---

/// Read active facts from an entity file, searching all vault subdirectories.
fn read_active_facts(vault_root: &Path, entity_id: &str) -> Vec<String> {
    use symbiotic_memory::vault_layout::find_canonical_entity_file;
    use symbiotic_memory::vault_parser;

    if let Ok(Some(path)) = find_canonical_entity_file(vault_root, entity_id) {
        if let Ok(content) = std::fs::read_to_string(&path) {
            return vault_parser::parse_entity_file(&content)
                .ok()
                .map(|p| {
                    p.memories
                        .into_iter()
                        .filter(|m| m.status == symbiotic_memory::MemoryStatus::Active)
                        .map(|m| m.fact)
                        .collect()
                })
                .unwrap_or_default();
        }
    }
    Vec::new()
}

/// Find the most textually similar fact to a claim using case-insensitive word overlap.
fn find_most_similar(claim: &str, facts: &[String]) -> Option<String> {
    let claim_words: std::collections::HashSet<String> = claim
        .split_whitespace()
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| w.len() > 2)
        .collect();

    if claim_words.is_empty() {
        return None;
    }

    facts
        .iter()
        .map(|fact| {
            let fact_words: std::collections::HashSet<String> = fact
                .split_whitespace()
                .map(|w| {
                    w.trim_matches(|c: char| !c.is_alphanumeric())
                        .to_lowercase()
                })
                .filter(|w| w.len() > 2)
                .collect();
            let intersection = claim_words.intersection(&fact_words).count();
            let union = claim_words.union(&fact_words).count();
            let jaccard = if union > 0 {
                intersection as f64 / union as f64
            } else {
                0.0
            };
            (fact.clone(), jaccard)
        })
        .filter(|(_, score)| *score > 0.15) // minimum similarity threshold
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(fact, _)| fact)
}

/// Map claim relationship type to a FactType.
fn relationship_to_fact_type(relationship: &str) -> Option<symbiotic_memory::FactType> {
    match relationship {
        "supports" | "extends" | "exemplifies" => Some(symbiotic_memory::FactType::Finding),
        "contradicts" => Some(symbiotic_memory::FactType::Finding),
        _ => Some(symbiotic_memory::FactType::Finding),
    }
}

/// Derive confidence from claim impact score (1-10 → 0.5-1.0).
fn claim_confidence(claim: &AtomicClaim) -> f64 {
    0.5 + (claim.impact_score as f64 / 20.0)
}

/// Generate a short slug from a URL for source tracking.
fn slug_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .unwrap_or("unknown")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .take(30)
        .collect()
}

/// Truncate a string to a maximum length, adding "..." if truncated.
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

/// Reflect claims against exactly one canonical target entity.
///
/// The LLM may choose to ignore irrelevant claims, but any proposed links that
/// survive are forced onto the provided target entity only.
pub async fn reflect_for_entity(
    claims: Vec<AtomicClaim>,
    target: &EntityProcessTarget,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<ReflectedGraph> {
    let system_prompt = format!(
        "\
You are the Targeted Circulation agent. Your job is to decide which new claims belong on one \
specific canonical entity record and how they relate to that entity's current facts. \
Respond ONLY with a JSON object matching the schema: \
{{\"claims\": [{{\"content\": \"string\", \"impact_score\": number, \"source_ref\": \"string\"}}], \
\"proposed_links\": [{{\"source_claim_idx\": number, \"target_node_id\": \"string\", \"relationship\": \"string\", \"target_space\": \"string\"}}]}}. \
Rules: \
- Only emit links for claims that should mutate the target entity. \
- Every emitted link MUST use target_node_id = \"{entity_id}\". \
- Every emitted link MUST use target_space = \"{target_space}\". \
- Ignore unrelated claims instead of forcing them into the target. \
- Use relationship values like \"supports\", \"contradicts\", \"extends\", or \"exemplifies\". \
- If a claim conflicts with an existing active fact, use \"contradicts\".",
        entity_id = target.entity_id,
        target_space = target.space.as_str(),
    );

    let facts_block = if target.active_facts.is_empty() {
        "- none".to_string()
    } else {
        target
            .active_facts
            .iter()
            .map(|fact| format!("- {fact}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let claims_json = serde_json::to_string(&claims)?;
    let redacted_claims = maybe_redact(&claims_json, config);
    let target_description = format!(
        "Target Entity:\n- id: {}\n- name: {}\n- type: {}\n- space: {}\nCurrent Active Facts:\n{}",
        target.entity_id,
        target.entity_name,
        target.entity_type.frontmatter_str(),
        target.space.as_str(),
        facts_block,
    );
    let redacted_target = maybe_redact(&target_description, config);

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!(
                "{}\n\nNew Claims to Evaluate:\n{}",
                redacted_target, redacted_claims
            ),
        },
    ];

    let response = llm
        .chat(&messages, true)
        .await
        .map_err(|e| DistilleryError::LlmFailed(e.to_string()))?;

    let graph: ReflectedGraph =
        serde_json::from_str(&response).map_err(|e| DistilleryError::ParseFailed(e.to_string()))?;

    let claims_len = graph.claims.len();
    let proposed_links = graph
        .proposed_links
        .into_iter()
        .filter_map(|link| {
            if link.source_claim_idx >= claims_len {
                return None;
            }
            if link.target_node_id != target.entity_id {
                return None;
            }
            Some(ProposedLink {
                source_claim_idx: link.source_claim_idx,
                target_node_id: target.entity_id.clone(),
                relationship: link.relationship,
                target_space: target.space,
            })
        })
        .collect();

    Ok(ReflectedGraph {
        claims: graph.claims,
        proposed_links,
    })
}

/// Run a targeted Distillery pass against a single canonical entity.
///
/// This powers `vault.process`: selected text is reduced into claims, reflected
/// against one entity only, then surgically written into that entity's canonical
/// record.
pub async fn process_text_for_entity(
    input: RawInput,
    target: &EntityProcessTarget,
    metadata_source: &str,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<EntityProcessReport> {
    let claims = reduce(input, config, llm).await?;
    let claims_extracted = claims.len();
    if claims_extracted == 0 {
        return Ok(EntityProcessReport::default());
    }

    let reflected = reflect_for_entity(claims, target, config, llm).await?;
    let valid_claims: Vec<AtomicClaim> = reflected
        .claims
        .into_iter()
        .filter(|c| !c.content.trim().is_empty() && (1..=10).contains(&c.impact_score))
        .collect();
    let claims_verified = valid_claims.len();
    if claims_verified == 0 {
        return Ok(EntityProcessReport {
            claims_extracted,
            claims_verified: 0,
            reweave: SurgicalReweaveReport::default(),
        });
    }

    let verified = VerifiedGraph {
        claims: valid_claims,
        proposed_links: reflected
            .proposed_links
            .into_iter()
            .filter(|link| link.source_claim_idx < claims_verified)
            .collect(),
    };

    if verified.proposed_links.is_empty() {
        return Ok(EntityProcessReport {
            claims_extracted,
            claims_verified,
            reweave: SurgicalReweaveReport::default(),
        });
    }

    let reweave = reweave_surgical_for_entity(&verified, target, metadata_source, config)?;

    Ok(EntityProcessReport {
        claims_extracted,
        claims_verified,
        reweave,
    })
}

fn reweave_surgical_for_entity(
    graph: &VerifiedGraph,
    target: &EntityProcessTarget,
    metadata_source: &str,
    config: &DistilleryStageConfig,
) -> DistilleryResult<SurgicalReweaveReport> {
    use symbiotic_memory::vault_writer::{NewFactMetadata, VaultWriteError, VaultWriter};

    let writer = VaultWriter::new(&config.kb_root);
    let mut report = SurgicalReweaveReport::default();
    let mut existing_facts = target.active_facts.clone();

    for link in graph
        .proposed_links
        .iter()
        .filter(|link| link.target_node_id == target.entity_id)
    {
        let Some(claim) = graph.claims.get(link.source_claim_idx) else {
            continue;
        };

        if link.relationship == "contradicts" && !existing_facts.is_empty() {
            if let Some(contradicted) = find_most_similar(&claim.content, &existing_facts) {
                let archived = writer
                    .archive_fact(
                        &target.entity_id,
                        &contradicted,
                        &format!("contradicted by: {}", truncate(&claim.content, 80)),
                    )
                    .map_err(|e| DistilleryError::VaultWriteFailed(e.to_string()))?;
                report.mutations.push(archived);
                report.facts_archived += 1;
                existing_facts.retain(|fact| fact != &contradicted);
            }
        }

        let metadata = NewFactMetadata {
            source: metadata_source.to_string(),
            fact_type: relationship_to_fact_type(&link.relationship),
            confidence: Some(claim_confidence(claim)),
        };

        match writer.add_fact(&target.entity_id, &claim.content, &metadata) {
            Ok(mutation) => {
                report.mutations.push(mutation);
                report.facts_added += 1;
                existing_facts.push(claim.content.clone());
            }
            Err(VaultWriteError::EntityNotFound(_)) => {
                return Err(DistilleryError::VaultWriteFailed(format!(
                    "target entity not found: {}",
                    target.entity_id
                )));
            }
            Err(err) => return Err(DistilleryError::VaultWriteFailed(err.to_string())),
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Stage 5: Archive (Raw Preservation)
// ---------------------------------------------------------------------------

/// Persists the original raw input to an archive file for provenance tracking.
///
/// Writes to `{kb_root}/operations/archive/{date}-{slug}.md` with YAML frontmatter
/// containing source URL, intake timestamp, and claim count.
pub fn archive(
    input: &RawInput,
    claim_count: usize,
    config: &DistilleryStageConfig,
) -> DistilleryResult<PathBuf> {
    let archive_dir = config.kb_root.join("operations").join("archive");
    std::fs::create_dir_all(&archive_dir)?;

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let slug = slugify(&input.source_url);
    let filename = format!("{date}-{slug}.md");
    let archive_path = archive_dir.join(&filename);

    let timestamp = Utc::now().to_rfc3339();
    let content = format!(
        "---\nsource_url: \"{}\"\ningested_at: \"{}\"\nclaim_count: {}\n---\n\n{}",
        input.source_url, timestamp, claim_count, input.raw_content
    );

    std::fs::write(&archive_path, content)?;

    Ok(archive_path)
}

/// Generate a URL-safe slug from a source URL.
///
/// Strips the scheme, replaces non-alphanumeric characters with hyphens,
/// collapses consecutive hyphens, trims leading/trailing hyphens, and
/// truncates to 80 characters.
fn slugify(url: &str) -> String {
    let stripped = url
        .trim_start_matches("https://")
        .trim_start_matches("http://");

    let slug: String = stripped
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();

    // Collapse consecutive hyphens
    let mut result = String::with_capacity(slug.len());
    let mut prev_was_hyphen = false;
    for c in slug.chars() {
        if c == '-' {
            if !prev_was_hyphen {
                result.push('-');
            }
            prev_was_hyphen = true;
        } else {
            result.push(c);
            prev_was_hyphen = false;
        }
    }

    let trimmed = result.trim_matches('-').to_string();

    // Truncate to 80 chars
    if trimmed.len() > 80 {
        trimmed[..80].trim_end_matches('-').to_string()
    } else {
        trimmed
    }
}

// ---------------------------------------------------------------------------
// Full Pipeline Orchestrator
// ---------------------------------------------------------------------------

/// Result of a full distillery pipeline run.
#[derive(Debug)]
pub struct DistilleryReport {
    /// Number of atomic claims extracted in the reduce stage.
    pub claims_extracted: usize,
    /// Number of claims that survived verification.
    pub claims_verified: usize,
    /// Number of proposed links from the reflect stage (before verification).
    pub links_proposed: usize,
    /// Number of proposed links after verification.
    pub links_verified: usize,
    /// Number of notes rewritten by reweave.
    pub notes_rewritten: u32,
    /// Path to the archive file.
    pub archive_path: PathBuf,
    /// Per-space claim counts (how many claims were routed to each space).
    pub claims_by_space: std::collections::HashMap<MemorySpace, usize>,
}

/// Runs the full distillery pipeline: Reduce -> Reflect -> Verify -> Reweave -> Archive.
///
/// `current_graph_context` is a string dump of existing note IDs/titles used by the
/// reflect stage to propose links. In production this is assembled from the Archive
/// file listing; callers are responsible for building it.
///
/// When `config.redact_llm_prompts` is enabled (default), PII is stripped from
/// all content before it reaches the LLM in the Reduce, Reflect, and Reweave
/// stages. The original unredacted content is preserved for the Archive stage.
pub async fn run_pipeline(
    input: RawInput,
    current_graph_context: &str,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<DistilleryReport> {
    // Stage 1: Reduce (PII redacted inside reduce() when enabled)
    let claims = reduce(input.clone(), config, llm).await?;
    let claims_extracted = claims.len();

    // Stage 2: Reflect (PII redacted inside reflect() when enabled)
    let reflected = reflect(claims, current_graph_context, config, llm).await?;
    let links_proposed = reflected.proposed_links.len();

    // Stage 3: Verify (uses knowledge-only path for backward compatibility)
    let verified = verify(reflected, config)?;
    let claims_verified = verified.claims.len();
    let links_verified = verified.proposed_links.len();

    // Stage 4: Reweave (surgical — no LLM, direct Markdown edits via VaultWriter)
    let surgical_report = reweave_surgical(&verified, config)?;
    let notes_rewritten = surgical_report.notes_affected();

    // Stage 5: Archive
    let archive_path = archive(&input, claims_extracted, config)?;

    // Default: all claims go to Knowledge space
    let mut claims_by_space = std::collections::HashMap::new();
    claims_by_space.insert(MemorySpace::Knowledge, claims_verified);

    Ok(DistilleryReport {
        claims_extracted,
        claims_verified,
        links_proposed,
        links_verified,
        notes_rewritten,
        archive_path,
        claims_by_space,
    })
}

/// Runs the full distillery pipeline with space-aware routing.
///
/// This enhanced version:
/// 1. Reduces input to atomic claims
/// 2. Classifies each claim into a memory space (Knowledge, Self, Methodology)
/// 3. Builds graph context from ALL spaces
/// 4. Reflects with cross-space awareness
/// 5. Verifies links across all spaces
/// 6. Rewrites notes in their respective spaces
/// 7. Archives raw content
pub async fn run_pipeline_with_spaces(
    input: RawInput,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<DistilleryReport> {
    // Stage 1: Reduce (PII redacted inside reduce() when enabled)
    let claims = reduce(input.clone(), config, llm).await?;
    let claims_extracted = claims.len();

    // Stage 1.5: Classify claims into memory spaces
    let classifications = classify_claims(&claims, config, llm).await;
    let mut claims_by_space = std::collections::HashMap::new();
    for class in &classifications {
        *claims_by_space.entry(class.space).or_insert(0usize) += 1;
    }

    // Stage 2: Reflect using cross-space graph context (PII redacted inside reflect())
    let graph_context = build_graph_context_all_spaces(&config.kb_root).unwrap_or_default();
    let reflected = reflect(claims, &graph_context, config, llm).await?;
    let links_proposed = reflected.proposed_links.len();

    // Stage 3: Verify with space-aware file checks
    let verified = verify_with_spaces(reflected, config)?;
    let claims_verified = verified.claims.len();
    let links_verified = verified.proposed_links.len();

    // Stage 4: Reweave (surgical, space-aware — no LLM, direct Markdown edits)
    let surgical_report = reweave_surgical_with_spaces(&verified, config)?;
    let notes_rewritten = surgical_report.notes_affected();

    // Stage 5: Archive
    let archive_path = archive(&input, claims_extracted, config)?;

    Ok(DistilleryReport {
        claims_extracted,
        claims_verified,
        links_proposed,
        links_verified,
        notes_rewritten,
        archive_path,
        claims_by_space,
    })
}

// ---------------------------------------------------------------------------
// Helper: Build graph context from Archive
// ---------------------------------------------------------------------------

/// Scans `{kb_root}/knowledge/` and returns a newline-separated list of note IDs
/// (filenames without the `.md` extension) that can be passed to `reflect()`.
pub fn build_graph_context(kb_root: &Path) -> DistilleryResult<String> {
    build_graph_context_for_space(kb_root, MemorySpace::Knowledge)
}

/// Scans a specific memory space directory and returns a newline-separated list
/// of note IDs (filenames without the `.md` extension).
pub fn build_graph_context_for_space(
    kb_root: &Path,
    space: MemorySpace,
) -> DistilleryResult<String> {
    let dir = space.path(kb_root);
    if !dir.exists() {
        return Ok(String::new());
    }

    let mut ids = Vec::new();
    collect_md_stems(&dir, &mut ids)?;
    ids.sort();
    Ok(ids.join("\n"))
}

/// Scans ALL three memory spaces and returns a combined context string.
///
/// Each entry is prefixed with its space name for the LLM to use:
/// `[knowledge] rust-safety`
/// `[identity] preferences`
/// `[operations] handoff-2026-02-20`
pub fn build_graph_context_all_spaces(kb_root: &Path) -> DistilleryResult<String> {
    let mut lines = Vec::new();
    for space in MemorySpace::all() {
        let dir = space.path(kb_root);
        if !dir.exists() {
            continue;
        }
        let mut ids = Vec::new();
        collect_md_stems(&dir, &mut ids)?;
        ids.sort();
        for id in ids {
            lines.push(format!("[{}] {}", space.dir_name(), id));
        }
    }
    Ok(lines.join("\n"))
}

/// Recursively collect `.md` file stems from a directory (non-recursive — top level only).
fn collect_md_stems(dir: &Path, out: &mut Vec<String>) -> DistilleryResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "md") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Space Classification (LLM-based routing)
// ---------------------------------------------------------------------------

/// Result of classifying a claim into a memory space.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceClassification {
    /// The assigned memory space.
    pub space: MemorySpace,
    /// Brief rationale from the LLM.
    pub rationale: String,
}

/// Classify a set of atomic claims into memory spaces using the LLM.
///
/// Returns one `SpaceClassification` per claim (same order as input).
/// If the LLM fails or returns invalid JSON, all claims default to `Knowledge`.
///
/// When `config.redact_llm_prompts` is enabled, PII in claim content is
/// redacted before being sent to the LLM.
pub async fn classify_claims(
    claims: &[AtomicClaim],
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> Vec<SpaceClassification> {
    let system_prompt = "\
You are the Memory Router. Classify each claim into exactly one memory space:\n\
- \"knowledge\": Facts, entities, external knowledge, technical concepts, research findings.\n\
- \"identity\": Identity, preferences, personal experiences, interaction style, confidence calibration.\n\
- \"operations\": Workflows, processes, skills, task patterns, operational procedures, friction logs.\n\
\n\
Respond ONLY with a JSON array matching the schema:\n\
[{\"space\": \"knowledge\" | \"identity\" | \"operations\", \"rationale\": \"string\"}]\n\
\n\
Return exactly one entry per input claim, in the same order.";

    let claims_json = match serde_json::to_string(claims) {
        Ok(j) => j,
        Err(_) => return default_classifications(claims.len()),
    };

    let redacted_claims = maybe_redact(&claims_json, config);

    let messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.to_string(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: format!("Claims to classify:\n{redacted_claims}"),
        },
    ];

    let response = match llm.chat(&messages, true).await {
        Ok(r) => r,
        Err(_) => return default_classifications(claims.len()),
    };

    match serde_json::from_str::<Vec<SpaceClassification>>(&response) {
        Ok(classifications) if classifications.len() == claims.len() => classifications,
        _ => default_classifications(claims.len()),
    }
}

/// Fallback: classify all claims as Knowledge.
fn default_classifications(count: usize) -> Vec<SpaceClassification> {
    (0..count)
        .map(|_| SpaceClassification {
            space: MemorySpace::Knowledge,
            rationale: "default classification".to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Space-aware Verify
// ---------------------------------------------------------------------------

/// Like [`verify`] but checks link targets across all memory spaces.
///
/// Each `ProposedLink.target_space` determines which directory to check for
/// the target file: `{kb_root}/{space}/{target_node_id}.md`.
pub fn verify_with_spaces(
    graph: ReflectedGraph,
    config: &DistilleryStageConfig,
) -> DistilleryResult<VerifiedGraph> {
    let valid_claims: Vec<AtomicClaim> = graph
        .claims
        .into_iter()
        .filter(|c| !c.content.trim().is_empty() && (1..=10).contains(&c.impact_score))
        .collect();

    if valid_claims.is_empty() {
        return Err(DistilleryError::AllClaimsRejected);
    }

    let claims_len = valid_claims.len();

    let valid_links: Vec<ProposedLink> = graph
        .proposed_links
        .into_iter()
        .filter(|link| {
            if link.source_claim_idx >= claims_len {
                return false;
            }
            let path = link
                .target_space
                .path(&config.kb_root)
                .join(format!("{}.md", link.target_node_id));
            path.exists()
        })
        .collect();

    Ok(VerifiedGraph {
        claims: valid_claims,
        proposed_links: valid_links,
    })
}

// ---------------------------------------------------------------------------
// Space-aware Reweave
// ---------------------------------------------------------------------------

/// Like [`reweave`] but resolves note paths using each link's `target_space`.
///
/// When `config.redact_llm_prompts` is enabled, PII in note content and claims
/// is redacted before being sent to the LLM.
pub async fn reweave_with_spaces(
    graph: &VerifiedGraph,
    config: &DistilleryStageConfig,
    llm: &dyn LlmClient,
) -> DistilleryResult<u32> {
    let system_prompt = "\
You are the Tissue Building agent. I will provide an old note and a set of new claims \
with their relationship to this note (e.g. \"supports\", \"contradicts\", \"extends\"). \
If this old note were written today, knowing what we now know, what would be different? \
Rewrite the note to seamlessly incorporate the new knowledge. \
Respond ONLY with the raw Markdown of the new note. Do not wrap in code fences.";

    // Group links by (target_space, target_node_id) so each note is rewritten once.
    let mut links_by_target: std::collections::HashMap<(MemorySpace, &str), Vec<&ProposedLink>> =
        std::collections::HashMap::new();
    for link in &graph.proposed_links {
        links_by_target
            .entry((link.target_space, link.target_node_id.as_str()))
            .or_default()
            .push(link);
    }

    let mut rewritten_count: u32 = 0;

    for ((space, target_id), links) in &links_by_target {
        let note_path = space.path(&config.kb_root).join(format!("{target_id}.md"));

        if !note_path.exists() {
            continue;
        }

        let old_note_content = std::fs::read_to_string(&note_path)?;

        let mut claims_context = String::new();
        for link in links {
            if let Some(claim) = graph.claims.get(link.source_claim_idx) {
                claims_context.push_str(&format!("- [{}] {}\n", link.relationship, claim.content));
            }
        }

        // Redact PII from note content and claims before sending to LLM
        let redacted_note = maybe_redact(&old_note_content, config);
        let redacted_claims = maybe_redact(&claims_context, config);

        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: system_prompt.to_string(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: format!(
                    "Old Note ({target_id} in {space} space):\n{redacted_note}\n\nNew Claims (with relationship):\n{redacted_claims}"
                ),
            },
        ];

        let rewritten_note = llm
            .chat(&messages, false)
            .await
            .map_err(|e| DistilleryError::LlmFailed(e.to_string()))?;

        std::fs::write(&note_path, rewritten_note)?;
        rewritten_count += 1;
    }

    Ok(rewritten_count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Mock LLM
    // -----------------------------------------------------------------------

    /// A mock LLM client for testing that returns pre-configured responses
    /// based on the system prompt content.
    struct MockLlm {
        reduce_response: String,
        reflect_response: String,
        reweave_response: String,
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
            let system = &messages[0].content;
            if system.contains("Enzymatic Breakdown") {
                Ok(self.reduce_response.clone())
            } else if system.contains("Circulation") {
                Ok(self.reflect_response.clone())
            } else if system.contains("Tissue Building") {
                Ok(self.reweave_response.clone())
            } else {
                Ok("{}".to_string())
            }
        }
    }

    /// A mock LLM that always fails.
    struct FailingLlm;

    #[async_trait::async_trait]
    impl LlmClient for FailingLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Err(anyhow::anyhow!("LLM connection refused"))
        }
    }

    /// A mock LLM that returns invalid JSON.
    struct BadJsonLlm;

    #[async_trait::async_trait]
    impl LlmClient for BadJsonLlm {
        async fn chat(
            &self,
            _messages: &[ChatMessage],
            _json_mode: bool,
        ) -> anyhow::Result<String> {
            Ok("this is not valid json".to_string())
        }
    }

    struct TargetEntityLlm {
        reduce_response: String,
        reflect_response: String,
    }

    #[async_trait::async_trait]
    impl LlmClient for TargetEntityLlm {
        async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
            let system = &messages[0].content;
            if system.contains("Enzymatic Breakdown") {
                Ok(self.reduce_response.clone())
            } else if system.contains("Targeted Circulation") {
                Ok(self.reflect_response.clone())
            } else {
                Ok("{}".to_string())
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn test_config(tmp: &std::path::Path) -> DistilleryStageConfig {
        DistilleryStageConfig {
            kb_root: tmp.to_path_buf(),
            // Disable redaction in existing tests so mock LLM responses
            // (which contain no PII) are not altered.
            redact_llm_prompts: false,
        }
    }

    fn test_config_with_redaction(tmp: &std::path::Path) -> DistilleryStageConfig {
        DistilleryStageConfig {
            kb_root: tmp.to_path_buf(),
            redact_llm_prompts: true,
        }
    }

    fn make_claim(content: &str, score: u8, source: &str) -> AtomicClaim {
        AtomicClaim {
            content: content.to_string(),
            impact_score: score,
            source_ref: source.to_string(),
            observed_at: None,
        }
    }

    fn make_link(idx: usize, target: &str, rel: &str) -> ProposedLink {
        ProposedLink {
            source_claim_idx: idx,
            target_node_id: target.to_string(),
            relationship: rel.to_string(),
            target_space: MemorySpace::Knowledge,
        }
    }

    fn make_link_in_space(idx: usize, target: &str, rel: &str, space: MemorySpace) -> ProposedLink {
        ProposedLink {
            source_claim_idx: idx,
            target_node_id: target.to_string(),
            relationship: rel.to_string(),
            target_space: space,
        }
    }

    // -----------------------------------------------------------------------
    // Slugify tests
    // -----------------------------------------------------------------------

    #[test]
    fn slugify_basic() {
        assert_eq!(
            slugify("https://example.com/path/to/thing"),
            "example-com-path-to-thing"
        );
    }

    #[test]
    fn slugify_strips_scheme() {
        assert_eq!(slugify("http://foo.bar/baz"), "foo-bar-baz");
    }

    #[test]
    fn slugify_truncates_long_urls() {
        let long = format!("https://example.com/{}", "a".repeat(200));
        let slug = slugify(&long);
        assert!(slug.len() <= 80);
    }

    #[test]
    fn slugify_collapses_consecutive_hyphens() {
        assert_eq!(
            slugify("https://example.com///foo///bar"),
            "example-com-foo-bar"
        );
    }

    // -----------------------------------------------------------------------
    // Verify tests
    // -----------------------------------------------------------------------

    #[test]
    fn verify_strips_invalid_claims() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let graph = ReflectedGraph {
            claims: vec![
                make_claim("valid claim", 5, "https://example.com"),
                make_claim("", 3, "https://example.com"), // empty
                make_claim("out of range", 0, "https://example.com"), // score 0
                make_claim("too high", 11, "https://example.com"), // score 11
            ],
            proposed_links: vec![],
        };

        let verified = verify(graph, &config).expect("should succeed with one valid claim");
        assert_eq!(verified.claims.len(), 1);
        assert_eq!(verified.claims[0].content, "valid claim");
    }

    #[test]
    fn verify_rejects_all_invalid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let graph = ReflectedGraph {
            claims: vec![make_claim("   ", 5, "https://example.com")],
            proposed_links: vec![],
        };

        let result = verify(graph, &config);
        assert!(matches!(result, Err(DistilleryError::AllClaimsRejected)));
    }

    #[test]
    fn verify_filters_missing_link_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("existing-note.md"), "# Existing Note").expect("write");

        let graph = ReflectedGraph {
            claims: vec![make_claim("valid claim", 7, "https://example.com")],
            proposed_links: vec![
                make_link(0, "existing-note", "supports"),
                make_link(0, "nonexistent-note", "extends"),
            ],
        };

        let verified = verify(graph, &config).expect("should pass");
        assert_eq!(verified.proposed_links.len(), 1);
        assert_eq!(verified.proposed_links[0].target_node_id, "existing-note");
        assert_eq!(verified.proposed_links[0].relationship, "supports");
    }

    #[test]
    fn verify_filters_out_of_bounds_claim_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note").expect("write");

        let graph = ReflectedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![
                make_link(0, "note", "supports"), // valid: idx 0, one claim
                make_link(99, "note", "extends"), // invalid: idx 99 out of bounds
            ],
        };

        let verified = verify(graph, &config).expect("should pass");
        assert_eq!(verified.proposed_links.len(), 1);
        assert_eq!(verified.proposed_links[0].source_claim_idx, 0);
    }

    #[test]
    fn verify_preserves_impact_score_boundaries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let graph = ReflectedGraph {
            claims: vec![
                make_claim("low", 1, "src"),   // min valid
                make_claim("high", 10, "src"), // max valid
            ],
            proposed_links: vec![],
        };

        let verified = verify(graph, &config).expect("should pass");
        assert_eq!(verified.claims.len(), 2);
    }

    #[test]
    fn verify_strips_claims_and_reindexes_links() {
        // When invalid claims are stripped, link source_claim_idx must still
        // be within bounds of the *validated* claims array.
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note").expect("write");

        let graph = ReflectedGraph {
            claims: vec![
                make_claim("", 5, "src"),      // index 0 -- will be stripped
                make_claim("valid", 5, "src"), // index 1 -- becomes index 0 after strip
            ],
            proposed_links: vec![
                // This link references idx 1 in the original, but after stripping
                // the validated array has only 1 element (at idx 0), so idx 1 is OOB.
                make_link(1, "note", "supports"),
            ],
        };

        let verified = verify(graph, &config).expect("should pass");
        assert_eq!(verified.claims.len(), 1);
        // The link at idx 1 is out of bounds for the single-element verified claims
        assert_eq!(verified.proposed_links.len(), 0);
    }

    // -----------------------------------------------------------------------
    // Archive tests
    // -----------------------------------------------------------------------

    #[test]
    fn archive_creates_file_with_frontmatter() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let input = RawInput {
            source_url: "https://example.com/article".to_string(),
            raw_content: "Some raw content here.".to_string(),
        };

        let path = archive(&input, 3, &config).expect("archive should succeed");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).expect("read archive file");
        assert!(content.contains("source_url: \"https://example.com/article\""));
        assert!(content.contains("claim_count: 3"));
        assert!(content.contains("Some raw content here."));
    }

    #[test]
    fn archive_creates_directory_if_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "content".to_string(),
        };

        // operations/archive/ does not exist yet
        let path = archive(&input, 0, &config).expect("archive should succeed");
        assert!(path.exists());
        assert!(path.parent().expect("parent").exists());
    }

    #[test]
    fn archive_frontmatter_contains_timestamp() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "data".to_string(),
        };

        let path = archive(&input, 1, &config).expect("archive");
        let content = std::fs::read_to_string(&path).expect("read");
        assert!(content.contains("ingested_at:"));
    }

    // -----------------------------------------------------------------------
    // Build graph context tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_graph_context_lists_md_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("alpha.md"), "# Alpha").expect("write");
        std::fs::write(knowledge_dir.join("beta.md"), "# Beta").expect("write");
        std::fs::write(knowledge_dir.join("not-md.txt"), "ignored").expect("write");

        let context = build_graph_context(tmp.path()).expect("should work");
        assert!(context.contains("alpha"));
        assert!(context.contains("beta"));
        assert!(!context.contains("not-md"));
    }

    #[test]
    fn build_graph_context_returns_empty_if_dir_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let context = build_graph_context(tmp.path()).expect("should work");
        assert!(context.is_empty());
    }

    #[test]
    fn build_graph_context_sorts_alphabetically() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("zeta.md"), "# Zeta").expect("write");
        std::fs::write(knowledge_dir.join("alpha.md"), "# Alpha").expect("write");
        std::fs::write(knowledge_dir.join("mu.md"), "# Mu").expect("write");

        let context = build_graph_context(tmp.path()).expect("should work");
        let ids: Vec<&str> = context.lines().collect();
        assert_eq!(ids, vec!["alpha", "mu", "zeta"]);
    }

    // -----------------------------------------------------------------------
    // Reduce stage tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reduce_parses_valid_json() {
        let claims = vec![
            make_claim(
                "Rust prevents use-after-free bugs",
                8,
                "https://example.com",
            ),
            make_claim(
                "Ownership model eliminates data races",
                7,
                "https://example.com",
            ),
        ];
        let llm = MockLlm {
            reduce_response: serde_json::to_string(&claims).expect("serialize"),
            reflect_response: String::new(),
            reweave_response: String::new(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Article about Rust safety".to_string(),
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let result = reduce(input, &config, &llm)
            .await
            .expect("reduce should succeed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].content, "Rust prevents use-after-free bugs");
        assert_eq!(result[0].impact_score, 8);
        assert_eq!(result[1].content, "Ownership model eliminates data races");
    }

    #[tokio::test]
    async fn reduce_returns_error_on_llm_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "content".to_string(),
        };

        let result = reduce(input, &config, &FailingLlm).await;
        assert!(matches!(result, Err(DistilleryError::LlmFailed(_))));
    }

    #[tokio::test]
    async fn reduce_returns_error_on_invalid_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "content".to_string(),
        };

        let result = reduce(input, &config, &BadJsonLlm).await;
        assert!(matches!(result, Err(DistilleryError::ParseFailed(_))));
    }

    #[tokio::test]
    async fn reduce_handles_empty_claims_array() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let llm = MockLlm {
            reduce_response: "[]".to_string(),
            reflect_response: String::new(),
            reweave_response: String::new(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "nothing useful here".to_string(),
        };

        let result = reduce(input, &config, &llm).await.expect("should succeed");
        assert!(result.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reflect stage tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reflect_parses_structured_links() {
        let claims = vec![make_claim("AI models improve yearly", 6, "src")];

        let reflected = ReflectedGraph {
            claims: claims.clone(),
            proposed_links: vec![
                make_link(0, "ai-progress", "supports"),
                make_link(0, "scaling-laws", "extends"),
            ],
        };

        let llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: serde_json::to_string(&reflected).expect("serialize"),
            reweave_response: String::new(),
        };

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let result = reflect(claims, "ai-progress\nscaling-laws", &config, &llm)
            .await
            .expect("reflect");
        assert_eq!(result.proposed_links.len(), 2);
        assert_eq!(result.proposed_links[0].target_node_id, "ai-progress");
        assert_eq!(result.proposed_links[0].relationship, "supports");
        assert_eq!(result.proposed_links[1].source_claim_idx, 0);
    }

    #[tokio::test]
    async fn reflect_returns_error_on_llm_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let claims = vec![make_claim("claim", 5, "src")];
        let result = reflect(claims, "context", &config, &FailingLlm).await;
        assert!(matches!(result, Err(DistilleryError::LlmFailed(_))));
    }

    #[tokio::test]
    async fn reflect_returns_error_on_bad_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let claims = vec![make_claim("claim", 5, "src")];
        let result = reflect(claims, "context", &config, &BadJsonLlm).await;
        assert!(matches!(result, Err(DistilleryError::ParseFailed(_))));
    }

    #[tokio::test]
    async fn reflect_handles_no_links() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let claims = vec![make_claim("orphan claim", 3, "src")];
        let reflected = ReflectedGraph {
            claims: claims.clone(),
            proposed_links: vec![],
        };

        let llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: serde_json::to_string(&reflected).expect("serialize"),
            reweave_response: String::new(),
        };

        let result = reflect(claims, "", &config, &llm).await.expect("reflect");
        assert!(result.proposed_links.is_empty());
    }

    // -----------------------------------------------------------------------
    // Reweave stage tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reweave_reads_and_writes_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note-a.md"), "# Original content").expect("write");

        let verified = VerifiedGraph {
            claims: vec![make_claim("new discovery", 8, "https://example.com")],
            proposed_links: vec![make_link(0, "note-a", "extends")],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "# Updated content with new discovery".to_string(),
        };

        let count = reweave(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 1);

        let updated = std::fs::read_to_string(knowledge_dir.join("note-a.md")).expect("read");
        assert_eq!(updated, "# Updated content with new discovery");
    }

    #[tokio::test]
    async fn reweave_skips_nonexistent_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // Don't create any knowledge files
        let verified = VerifiedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![make_link(0, "missing", "supports")],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "should not be written".to_string(),
        };

        let count = reweave(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn reweave_groups_multiple_links_to_same_note() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("shared.md"), "# Shared note").expect("write");

        let verified = VerifiedGraph {
            claims: vec![
                make_claim("claim A", 5, "src"),
                make_claim("claim B", 7, "src"),
            ],
            proposed_links: vec![
                make_link(0, "shared", "supports"),
                make_link(1, "shared", "extends"),
            ],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "# Rewritten with both claims".to_string(),
        };

        let count = reweave(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        // Should rewrite only once even though there are two links to the same note
        assert_eq!(count, 1);

        let updated = std::fs::read_to_string(knowledge_dir.join("shared.md")).expect("read");
        assert_eq!(updated, "# Rewritten with both claims");
    }

    #[tokio::test]
    async fn reweave_returns_error_on_llm_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note").expect("write");

        let verified = VerifiedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![make_link(0, "note", "supports")],
        };

        let result = reweave(&verified, &config, &FailingLlm).await;
        assert!(matches!(result, Err(DistilleryError::LlmFailed(_))));
    }

    #[tokio::test]
    async fn reweave_handles_empty_links() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let verified = VerifiedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: String::new(),
        };

        let count = reweave(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn reweave_writes_multiple_distinct_notes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note-a.md"), "# A").expect("write");
        std::fs::write(knowledge_dir.join("note-b.md"), "# B").expect("write");

        let verified = VerifiedGraph {
            claims: vec![make_claim("universal claim", 6, "src")],
            proposed_links: vec![
                make_link(0, "note-a", "supports"),
                make_link(0, "note-b", "extends"),
            ],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "# Rewritten".to_string(),
        };

        let count = reweave(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 2);

        let a = std::fs::read_to_string(knowledge_dir.join("note-a.md")).expect("read a");
        let b = std::fs::read_to_string(knowledge_dir.join("note-b.md")).expect("read b");
        assert_eq!(a, "# Rewritten");
        assert_eq!(b, "# Rewritten");
    }

    // -----------------------------------------------------------------------
    // Full pipeline tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn full_pipeline_runs_all_stages() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // Create entity file in vault format (surgical reweave needs ## Facts section)
        let entities_dir = tmp.path().join("ledger/concepts/existing");
        std::fs::create_dir_all(&entities_dir).expect("mkdir");
        std::fs::write(
            entities_dir.join("existing.md"),
            "---\nid: existing\ntype: concept\nspace: knowledge\nsensitivity: private\n\
             created: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n\n\
             # existing\n\n## Facts\n\n## Relationships\n\n## History\n",
        )
        .expect("write");
        // Also create knowledge/ for build_graph_context scanning
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("existing.md"), "# Old note").expect("write");

        let mock_llm = MockLlm {
            reduce_response: serde_json::to_string(&vec![make_claim(
                "Rust is memory safe",
                7,
                "https://example.com/article",
            )])
            .expect("serialize"),
            reflect_response: serde_json::to_string(&ReflectedGraph {
                claims: vec![make_claim(
                    "Rust is memory safe",
                    7,
                    "https://example.com/article",
                )],
                proposed_links: vec![make_link(0, "existing", "supports")],
            })
            .expect("serialize"),
            reweave_response: String::new(), // unused by surgical reweave
        };

        let input = RawInput {
            source_url: "https://example.com/article".to_string(),
            raw_content: "An article about Rust memory safety.".to_string(),
        };

        let graph_context = build_graph_context(tmp.path()).expect("context");
        let report = run_pipeline(input, &graph_context, &config, &mock_llm)
            .await
            .expect("pipeline should succeed");

        assert_eq!(report.claims_extracted, 1);
        assert_eq!(report.claims_verified, 1);
        assert_eq!(report.links_proposed, 1);
        assert_eq!(report.links_verified, 1);
        assert_eq!(report.notes_rewritten, 1);
        assert!(report.archive_path.exists());

        // Surgical reweave adds facts to ## Facts section in entity files
        let updated = std::fs::read_to_string(entities_dir.join("existing.md")).expect("read");
        assert!(
            updated.contains("Rust is memory safe"),
            "entity file should contain the added fact"
        );
    }

    #[tokio::test]
    async fn full_pipeline_with_no_existing_notes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // No knowledge directory at all
        let mock_llm = MockLlm {
            reduce_response: serde_json::to_string(&vec![make_claim(
                "Novel insight",
                9,
                "https://example.com",
            )])
            .expect("serialize"),
            reflect_response: serde_json::to_string(&ReflectedGraph {
                claims: vec![make_claim("Novel insight", 9, "https://example.com")],
                proposed_links: vec![], // no existing nodes to link
            })
            .expect("serialize"),
            reweave_response: String::new(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Brand new content.".to_string(),
        };

        let report = run_pipeline(input, "", &config, &mock_llm)
            .await
            .expect("pipeline should succeed");

        assert_eq!(report.claims_extracted, 1);
        assert_eq!(report.claims_verified, 1);
        assert_eq!(report.links_proposed, 0);
        assert_eq!(report.links_verified, 0);
        assert_eq!(report.notes_rewritten, 0);
        assert!(report.archive_path.exists());
    }

    #[tokio::test]
    async fn full_pipeline_with_multiple_claims_and_links() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("rust-safety.md"), "# Rust Safety").expect("write");
        std::fs::write(knowledge_dir.join("concurrency.md"), "# Concurrency").expect("write");

        let claims = vec![
            make_claim(
                "Rust prevents data races at compile time",
                8,
                "https://blog.com",
            ),
            make_claim(
                "Send and Sync traits enforce thread safety",
                7,
                "https://blog.com",
            ),
            make_claim(
                "Async/await enables efficient concurrency",
                6,
                "https://blog.com",
            ),
        ];

        let mock_llm = MockLlm {
            reduce_response: serde_json::to_string(&claims).expect("serialize"),
            reflect_response: serde_json::to_string(&ReflectedGraph {
                claims: claims.clone(),
                proposed_links: vec![
                    make_link(0, "rust-safety", "supports"),
                    make_link(1, "rust-safety", "extends"),
                    make_link(2, "concurrency", "extends"),
                ],
            })
            .expect("serialize"),
            reweave_response: "# Updated note".to_string(),
        };

        let input = RawInput {
            source_url: "https://blog.com/rust-concurrency".to_string(),
            raw_content: "Deep dive into Rust concurrency safety.".to_string(),
        };

        let graph_context = build_graph_context(tmp.path()).expect("context");
        let report = run_pipeline(input, &graph_context, &config, &mock_llm)
            .await
            .expect("pipeline");

        assert_eq!(report.claims_extracted, 3);
        assert_eq!(report.claims_verified, 3);
        assert_eq!(report.links_proposed, 3);
        assert_eq!(report.links_verified, 3);
        assert_eq!(report.notes_rewritten, 2); // rust-safety + concurrency
    }

    #[tokio::test]
    async fn full_pipeline_fails_on_reduce_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "content".to_string(),
        };

        let result = run_pipeline(input, "", &config, &FailingLlm).await;
        assert!(matches!(result, Err(DistilleryError::LlmFailed(_))));
    }

    #[tokio::test]
    async fn full_pipeline_links_verified_drops_invalid_targets() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("real.md"), "# Real").expect("write");

        let claims = vec![make_claim("some claim", 5, "src")];
        let mock_llm = MockLlm {
            reduce_response: serde_json::to_string(&claims).expect("serialize"),
            reflect_response: serde_json::to_string(&ReflectedGraph {
                claims: claims.clone(),
                proposed_links: vec![
                    make_link(0, "real", "supports"),
                    make_link(0, "hallucinated", "extends"), // does not exist on disk
                ],
            })
            .expect("serialize"),
            reweave_response: "# Updated".to_string(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "content".to_string(),
        };

        let graph_context = build_graph_context(tmp.path()).expect("context");
        let report = run_pipeline(input, &graph_context, &config, &mock_llm)
            .await
            .expect("pipeline");

        assert_eq!(report.links_proposed, 2);
        assert_eq!(report.links_verified, 1); // only "real" survives
        assert_eq!(report.notes_rewritten, 1);
    }

    // -----------------------------------------------------------------------
    // Serialization round-trip tests
    // -----------------------------------------------------------------------

    #[test]
    fn atomic_claim_serde_round_trip() {
        let claim = make_claim("test claim", 5, "https://example.com");
        let json = serde_json::to_string(&claim).expect("serialize");
        let parsed: AtomicClaim = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.content, "test claim");
        assert_eq!(parsed.impact_score, 5);
        assert_eq!(parsed.source_ref, "https://example.com");
    }

    #[test]
    fn proposed_link_serde_round_trip() {
        let link = make_link(2, "target-node", "contradicts");
        let json = serde_json::to_string(&link).expect("serialize");
        let parsed: ProposedLink = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.source_claim_idx, 2);
        assert_eq!(parsed.target_node_id, "target-node");
        assert_eq!(parsed.relationship, "contradicts");
    }

    #[test]
    fn reflected_graph_serde_round_trip() {
        let graph = ReflectedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![make_link(0, "node", "supports")],
        };
        let json = serde_json::to_string(&graph).expect("serialize");
        let parsed: ReflectedGraph = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.claims.len(), 1);
        assert_eq!(parsed.proposed_links.len(), 1);
        assert_eq!(parsed.proposed_links[0].target_node_id, "node");
    }

    // -----------------------------------------------------------------------
    // Error type tests
    // -----------------------------------------------------------------------

    #[test]
    fn distillery_error_display_messages() {
        let err = DistilleryError::LlmFailed("timeout".to_string());
        assert_eq!(err.to_string(), "LLM call failed: timeout");

        let err = DistilleryError::ParseFailed("invalid JSON".to_string());
        assert_eq!(
            err.to_string(),
            "failed to parse LLM output as JSON: invalid JSON"
        );

        let err = DistilleryError::AllClaimsRejected;
        assert_eq!(err.to_string(), "verification rejected all claims");
    }

    // -----------------------------------------------------------------------
    // MemorySpace tests
    // -----------------------------------------------------------------------

    #[test]
    fn memory_space_dir_names() {
        assert_eq!(MemorySpace::Knowledge.dir_name(), "knowledge");
        assert_eq!(MemorySpace::Identity.dir_name(), "identity");
        assert_eq!(MemorySpace::Operations.dir_name(), "operations");
    }

    #[test]
    fn memory_space_path() {
        let root = std::path::Path::new("/kb");
        assert_eq!(
            MemorySpace::Knowledge.path(root),
            PathBuf::from("/kb/knowledge")
        );
        assert_eq!(
            MemorySpace::Identity.path(root),
            PathBuf::from("/kb/identity")
        );
        assert_eq!(
            MemorySpace::Operations.path(root),
            PathBuf::from("/kb/operations")
        );
    }

    #[test]
    fn memory_space_all_returns_three() {
        assert_eq!(MemorySpace::all().len(), 3);
    }

    #[test]
    fn memory_space_display() {
        assert_eq!(format!("{}", MemorySpace::Knowledge), "knowledge");
        assert_eq!(format!("{}", MemorySpace::Identity), "identity");
        assert_eq!(format!("{}", MemorySpace::Operations), "operations");
    }

    #[test]
    fn memory_space_serde_round_trip() {
        for space in MemorySpace::all() {
            let json = serde_json::to_string(space).expect("serialize");
            let parsed: MemorySpace = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*space, parsed);
        }
    }

    #[test]
    fn memory_space_serde_values() {
        assert_eq!(
            serde_json::to_string(&MemorySpace::Knowledge).expect("ser"),
            "\"knowledge\""
        );
        assert_eq!(
            serde_json::to_string(&MemorySpace::Identity).expect("ser"),
            "\"identity\""
        );
        assert_eq!(
            serde_json::to_string(&MemorySpace::Operations).expect("ser"),
            "\"operations\""
        );
    }

    // -----------------------------------------------------------------------
    // build_graph_context_for_space tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_graph_context_for_knowledge_space() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("rust.md"), "# Rust").expect("write");

        let context =
            build_graph_context_for_space(tmp.path(), MemorySpace::Knowledge).expect("ctx");
        assert!(context.contains("rust"));
    }

    #[test]
    fn build_graph_context_for_identity_space() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let identity_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        std::fs::write(identity_dir.join("preferences.md"), "# Prefs").expect("write");

        let context =
            build_graph_context_for_space(tmp.path(), MemorySpace::Identity).expect("ctx");
        assert!(context.contains("preferences"));
    }

    #[test]
    fn build_graph_context_for_operations_space() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let operations_dir = tmp.path().join("operations");
        std::fs::create_dir_all(&operations_dir).expect("mkdir");
        std::fs::write(operations_dir.join("workflow.md"), "# Workflow").expect("write");

        let context =
            build_graph_context_for_space(tmp.path(), MemorySpace::Operations).expect("ctx");
        assert!(context.contains("workflow"));
    }

    #[test]
    fn build_graph_context_for_missing_space_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let context =
            build_graph_context_for_space(tmp.path(), MemorySpace::Identity).expect("ctx");
        assert!(context.is_empty());
    }

    // -----------------------------------------------------------------------
    // build_graph_context_all_spaces tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_graph_context_all_spaces_combines() {
        let tmp = tempfile::tempdir().expect("tempdir");

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("fact-a.md"), "# Fact").expect("write");

        let identity_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        std::fs::write(identity_dir.join("prefs.md"), "# Prefs").expect("write");

        let operations_dir = tmp.path().join("operations");
        std::fs::create_dir_all(&operations_dir).expect("mkdir");
        std::fs::write(operations_dir.join("skill.md"), "# Skill").expect("write");

        let context = build_graph_context_all_spaces(tmp.path()).expect("ctx");
        assert!(context.contains("[knowledge] fact-a"));
        assert!(context.contains("[identity] prefs"));
        assert!(context.contains("[operations] skill"));
    }

    #[test]
    fn build_graph_context_all_spaces_skips_missing_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Only create knowledge
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note").expect("write");

        let context = build_graph_context_all_spaces(tmp.path()).expect("ctx");
        assert!(context.contains("[knowledge] note"));
        assert!(!context.contains("[identity]"));
        assert!(!context.contains("[operations]"));
    }

    // -----------------------------------------------------------------------
    // Space classification tests
    // -----------------------------------------------------------------------

    /// A mock LLM for space classification testing.
    struct ClassifierLlm {
        response: String,
    }

    #[async_trait::async_trait]
    impl LlmClient for ClassifierLlm {
        async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
            let system = &messages[0].content;
            if system.contains("Memory Router") {
                Ok(self.response.clone())
            } else {
                Ok("[]".to_string())
            }
        }
    }

    #[tokio::test]
    async fn classify_claims_routes_correctly() {
        let classifications = vec![
            SpaceClassification {
                space: MemorySpace::Knowledge,
                rationale: "factual claim".to_string(),
            },
            SpaceClassification {
                space: MemorySpace::Identity,
                rationale: "preference".to_string(),
            },
            SpaceClassification {
                space: MemorySpace::Operations,
                rationale: "workflow pattern".to_string(),
            },
        ];

        let llm = ClassifierLlm {
            response: serde_json::to_string(&classifications).expect("serialize"),
        };

        let claims = vec![
            make_claim("Rust is fast", 5, "src"),
            make_claim("I prefer dark mode", 3, "src"),
            make_claim("Always run tests before commit", 6, "src"),
        ];

        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let result = classify_claims(&claims, &config, &llm).await;
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].space, MemorySpace::Knowledge);
        assert_eq!(result[1].space, MemorySpace::Identity);
        assert_eq!(result[2].space, MemorySpace::Operations);
    }

    #[tokio::test]
    async fn classify_claims_defaults_on_llm_failure() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let claims = vec![
            make_claim("claim a", 5, "src"),
            make_claim("claim b", 5, "src"),
        ];

        let result = classify_claims(&claims, &config, &FailingLlm).await;
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].space, MemorySpace::Knowledge);
        assert_eq!(result[1].space, MemorySpace::Knowledge);
    }

    #[tokio::test]
    async fn classify_claims_defaults_on_bad_json() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        let llm = ClassifierLlm {
            response: "not json".to_string(),
        };

        let claims = vec![make_claim("claim", 5, "src")];
        let result = classify_claims(&claims, &config, &llm).await;
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].space, MemorySpace::Knowledge);
    }

    #[tokio::test]
    async fn classify_claims_defaults_on_length_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());
        // LLM returns 1 classification for 2 claims
        let classifications = vec![SpaceClassification {
            space: MemorySpace::Operations,
            rationale: "only one".to_string(),
        }];

        let llm = ClassifierLlm {
            response: serde_json::to_string(&classifications).expect("serialize"),
        };

        let claims = vec![
            make_claim("claim a", 5, "src"),
            make_claim("claim b", 5, "src"),
        ];
        let result = classify_claims(&claims, &config, &llm).await;
        // Should fall back to defaults since count mismatch
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].space, MemorySpace::Knowledge);
    }

    // -----------------------------------------------------------------------
    // verify_with_spaces tests
    // -----------------------------------------------------------------------

    #[test]
    fn verify_with_spaces_checks_correct_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // Create notes in different spaces
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("fact.md"), "# Fact").expect("write");

        let identity_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        std::fs::write(identity_dir.join("pref.md"), "# Pref").expect("write");

        let graph = ReflectedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![
                make_link_in_space(0, "fact", "supports", MemorySpace::Knowledge),
                make_link_in_space(0, "pref", "relates", MemorySpace::Identity),
                make_link_in_space(0, "missing", "extends", MemorySpace::Operations),
            ],
        };

        let verified = verify_with_spaces(graph, &config).expect("should pass");
        assert_eq!(verified.proposed_links.len(), 2);
        assert_eq!(
            verified.proposed_links[0].target_space,
            MemorySpace::Knowledge
        );
        assert_eq!(
            verified.proposed_links[1].target_space,
            MemorySpace::Identity
        );
    }

    #[test]
    fn verify_with_spaces_rejects_wrong_space() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // Create note in knowledge only
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("note.md"), "# Note").expect("write");

        let graph = ReflectedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![
                // This note exists in knowledge/ but the link says identity/
                make_link_in_space(0, "note", "supports", MemorySpace::Identity),
            ],
        };

        let verified = verify_with_spaces(graph, &config).expect("should pass");
        assert_eq!(verified.proposed_links.len(), 0);
    }

    // -----------------------------------------------------------------------
    // reweave_with_spaces tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn reweave_with_spaces_writes_to_correct_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("fact.md"), "# Old fact").expect("write");

        let identity_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        std::fs::write(identity_dir.join("pref.md"), "# Old pref").expect("write");

        let verified = VerifiedGraph {
            claims: vec![make_claim("new info", 7, "src")],
            proposed_links: vec![
                make_link_in_space(0, "fact", "supports", MemorySpace::Knowledge),
                make_link_in_space(0, "pref", "updates", MemorySpace::Identity),
            ],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "# Updated".to_string(),
        };

        let count = reweave_with_spaces(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 2);

        let fact = std::fs::read_to_string(knowledge_dir.join("fact.md")).expect("read");
        assert_eq!(fact, "# Updated");

        let pref = std::fs::read_to_string(identity_dir.join("pref.md")).expect("read");
        assert_eq!(pref, "# Updated");
    }

    #[tokio::test]
    async fn reweave_with_spaces_skips_missing_in_space() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // No directories created at all
        let verified = VerifiedGraph {
            claims: vec![make_claim("claim", 5, "src")],
            proposed_links: vec![make_link_in_space(
                0,
                "missing",
                "supports",
                MemorySpace::Operations,
            )],
        };

        let mock_llm = MockLlm {
            reduce_response: String::new(),
            reflect_response: String::new(),
            reweave_response: "# Rewritten".to_string(),
        };

        let count = reweave_with_spaces(&verified, &config, &mock_llm)
            .await
            .expect("reweave");
        assert_eq!(count, 0);
    }

    // -----------------------------------------------------------------------
    // ProposedLink with space serde tests
    // -----------------------------------------------------------------------

    #[test]
    fn proposed_link_default_space_is_knowledge() {
        // Deserializing without target_space should default to Knowledge
        let json =
            r#"{"source_claim_idx": 0, "target_node_id": "test", "relationship": "supports"}"#;
        let link: ProposedLink = serde_json::from_str(json).expect("deserialize");
        assert_eq!(link.target_space, MemorySpace::Knowledge);
    }

    #[test]
    fn proposed_link_with_space_serde_round_trip() {
        let link = make_link_in_space(1, "pref", "updates", MemorySpace::Identity);
        let json = serde_json::to_string(&link).expect("serialize");
        let parsed: ProposedLink = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.target_space, MemorySpace::Identity);
        assert_eq!(parsed.target_node_id, "pref");
    }

    // -----------------------------------------------------------------------
    // Full pipeline with spaces test
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn full_pipeline_with_spaces_classifies_and_routes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path());

        // Create entity files in vault format (surgical reweave needs ## Facts section)
        let entities_dir = tmp.path().join("ledger/concepts/existing");
        std::fs::create_dir_all(&entities_dir).expect("mkdir");
        std::fs::write(
            entities_dir.join("existing.md"),
            "---\nid: existing\ntype: concept\nspace: knowledge\nsensitivity: private\n\
             created: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n\n\
             # existing\n\n## Facts\n\n## Relationships\n\n## History\n",
        )
        .expect("write");
        // knowledge/ still needed for build_graph_context scanning
        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("existing.md"), "# Old note").expect("write");

        let identity_dir = tmp.path().join("identity");
        std::fs::create_dir_all(&identity_dir).expect("mkdir");
        std::fs::write(
            identity_dir.join("prefs.md"),
            "---\nid: prefs\ntype: preference\nspace: identity\nsensitivity: private\n\
             created: 2026-01-01T00:00:00Z\nupdated: 2026-01-01T00:00:00Z\n---\n\n\
             # prefs\n\n## Facts\n\n## Relationships\n\n## History\n",
        )
        .expect("write");

        let claims = vec![
            make_claim("Rust is fast", 7, "https://example.com"),
            make_claim("I prefer vim", 3, "https://example.com"),
        ];

        // Mock LLM that handles all stages
        struct MultiStageLlm {
            reduce_response: String,
            classify_response: String,
            reflect_response: String,
            reweave_response: String,
        }

        #[async_trait::async_trait]
        impl LlmClient for MultiStageLlm {
            async fn chat(
                &self,
                messages: &[ChatMessage],
                _json_mode: bool,
            ) -> anyhow::Result<String> {
                let system = &messages[0].content;
                if system.contains("Enzymatic Breakdown") {
                    Ok(self.reduce_response.clone())
                } else if system.contains("Memory Router") {
                    Ok(self.classify_response.clone())
                } else if system.contains("Circulation") {
                    Ok(self.reflect_response.clone())
                } else if system.contains("Tissue Building") {
                    Ok(self.reweave_response.clone())
                } else {
                    Ok("{}".to_string())
                }
            }
        }

        let reflected = ReflectedGraph {
            claims: claims.clone(),
            proposed_links: vec![
                make_link_in_space(0, "existing", "supports", MemorySpace::Knowledge),
                make_link_in_space(1, "prefs", "updates", MemorySpace::Identity),
            ],
        };

        let classifications = vec![
            SpaceClassification {
                space: MemorySpace::Knowledge,
                rationale: "factual".to_string(),
            },
            SpaceClassification {
                space: MemorySpace::Identity,
                rationale: "preference".to_string(),
            },
        ];

        let mock_llm = MultiStageLlm {
            reduce_response: serde_json::to_string(&claims).expect("ser"),
            classify_response: serde_json::to_string(&classifications).expect("ser"),
            reflect_response: serde_json::to_string(&reflected).expect("ser"),
            reweave_response: "# Updated".to_string(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Content about Rust and vim preferences.".to_string(),
        };

        let report = run_pipeline_with_spaces(input, &config, &mock_llm)
            .await
            .expect("pipeline");

        assert_eq!(report.claims_extracted, 2);
        assert_eq!(report.claims_verified, 2);
        assert_eq!(report.links_proposed, 2);
        assert_eq!(report.links_verified, 2);
        assert_eq!(report.notes_rewritten, 2);
        assert!(report.archive_path.exists());

        // Verify space classification counts
        assert_eq!(
            report.claims_by_space.get(&MemorySpace::Knowledge),
            Some(&1)
        );
        assert_eq!(report.claims_by_space.get(&MemorySpace::Identity), Some(&1));

        // Verify facts were added to entity files in correct directories
        let fact =
            std::fs::read_to_string(entities_dir.join("existing.md")).expect("read knowledge");
        assert!(
            fact.contains("Rust is fast"),
            "knowledge entity should contain the added fact"
        );

        let pref = std::fs::read_to_string(identity_dir.join("prefs.md")).expect("read identity");
        assert!(
            pref.contains("I prefer vim"),
            "identity entity should contain the added fact"
        );
    }

    // -----------------------------------------------------------------------
    // PII Redaction in LLM Prompts
    // -----------------------------------------------------------------------

    /// A mock LLM that captures all user messages it receives, for verifying
    /// that PII has been redacted before reaching the LLM.
    struct CapturingLlm {
        captured_user_messages: std::sync::Mutex<Vec<String>>,
        reduce_response: String,
        reflect_response: String,
        reweave_response: String,
    }

    impl CapturingLlm {
        fn new_for_reduce(response: &str) -> Self {
            Self {
                captured_user_messages: std::sync::Mutex::new(Vec::new()),
                reduce_response: response.to_string(),
                reflect_response: String::new(),
                reweave_response: String::new(),
            }
        }

        fn new_for_reflect(response: &str) -> Self {
            Self {
                captured_user_messages: std::sync::Mutex::new(Vec::new()),
                reduce_response: String::new(),
                reflect_response: response.to_string(),
                reweave_response: String::new(),
            }
        }

        fn new_for_reweave(response: &str) -> Self {
            Self {
                captured_user_messages: std::sync::Mutex::new(Vec::new()),
                reduce_response: String::new(),
                reflect_response: String::new(),
                reweave_response: response.to_string(),
            }
        }

        fn captured(&self) -> Vec<String> {
            self.captured_user_messages.lock().expect("lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for CapturingLlm {
        async fn chat(&self, messages: &[ChatMessage], _json_mode: bool) -> anyhow::Result<String> {
            // Capture all user messages
            for msg in messages {
                if msg.role == "user" {
                    self.captured_user_messages
                        .lock()
                        .expect("lock")
                        .push(msg.content.clone());
                }
            }

            let system = &messages[0].content;
            if system.contains("Enzymatic Breakdown") {
                Ok(self.reduce_response.clone())
            } else if system.contains("Circulation") {
                Ok(self.reflect_response.clone())
            } else if system.contains("Tissue Building") {
                Ok(self.reweave_response.clone())
            } else if system.contains("Memory Router") {
                // Return default classifications
                Ok("[]".to_string())
            } else {
                Ok("{}".to_string())
            }
        }
    }

    #[tokio::test]
    async fn reduce_redacts_pii_from_llm_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config_with_redaction(tmp.path());

        let claims = vec![make_claim("some claim", 5, "https://example.com")];
        let llm = CapturingLlm::new_for_reduce(&serde_json::to_string(&claims).expect("serialize"));

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Contact user@example.com or call 555-123-4567 for details".to_string(),
        };

        let _result = reduce(input, &config, &llm)
            .await
            .expect("reduce should succeed");

        let captured = llm.captured();
        assert_eq!(captured.len(), 1, "should have captured one user message");

        // The email should be redacted in the prompt sent to the LLM
        assert!(
            captured[0].contains("[redacted-email]"),
            "email should be redacted in LLM prompt, got: {}",
            captured[0]
        );
        assert!(
            captured[0].contains("[redacted-phone]"),
            "phone should be redacted in LLM prompt, got: {}",
            captured[0]
        );
        assert!(
            !captured[0].contains("user@example.com"),
            "original email should not appear in LLM prompt"
        );
        assert!(
            !captured[0].contains("555-123-4567"),
            "original phone should not appear in LLM prompt"
        );
    }

    #[tokio::test]
    async fn reduce_preserves_raw_content_when_redaction_disabled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config(tmp.path()); // redaction disabled

        let claims = vec![make_claim("some claim", 5, "https://example.com")];
        let llm = CapturingLlm::new_for_reduce(&serde_json::to_string(&claims).expect("serialize"));

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Contact user@example.com for details".to_string(),
        };

        let _result = reduce(input, &config, &llm)
            .await
            .expect("reduce should succeed");

        let captured = llm.captured();
        assert!(
            captured[0].contains("user@example.com"),
            "with redaction disabled, original email should appear in LLM prompt"
        );
    }

    #[tokio::test]
    async fn reflect_redacts_pii_from_llm_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config_with_redaction(tmp.path());

        let claims = vec![make_claim(
            "User admin@corp.com reported issue at 192.168.1.1",
            6,
            "src",
        )];

        let reflected = ReflectedGraph {
            claims: claims.clone(),
            proposed_links: vec![],
        };

        let llm =
            CapturingLlm::new_for_reflect(&serde_json::to_string(&reflected).expect("serialize"));

        let graph_context = "Server runs at 10.0.0.42 for admin@internal.org";

        let _result = reflect(claims, graph_context, &config, &llm)
            .await
            .expect("reflect should succeed");

        let captured = llm.captured();
        assert_eq!(captured.len(), 1);

        // Both the claims and graph context should have PII redacted
        assert!(
            !captured[0].contains("admin@corp.com"),
            "email in claims should be redacted"
        );
        assert!(
            !captured[0].contains("admin@internal.org"),
            "email in graph context should be redacted"
        );
        assert!(
            captured[0].contains("[redacted-email]"),
            "should contain email redaction placeholder"
        );
    }

    #[tokio::test]
    async fn reweave_redacts_pii_from_llm_prompt() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config_with_redaction(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(
            knowledge_dir.join("contacts.md"),
            "# Contacts\n\nReach out to admin@example.com or call 555-987-6543",
        )
        .expect("write");

        let verified = VerifiedGraph {
            claims: vec![make_claim(
                "New contact info: boss@corp.com, phone 555-111-2222",
                7,
                "src",
            )],
            proposed_links: vec![make_link(0, "contacts", "extends")],
        };

        let llm = CapturingLlm::new_for_reweave("# Updated contacts note");

        let _count = reweave(&verified, &config, &llm)
            .await
            .expect("reweave should succeed");

        let captured = llm.captured();
        assert_eq!(captured.len(), 1);

        // The old note content and new claims should both have PII redacted
        assert!(
            !captured[0].contains("admin@example.com"),
            "email from old note should be redacted"
        );
        assert!(
            !captured[0].contains("555-987-6543"),
            "phone from old note should be redacted"
        );
        assert!(
            !captured[0].contains("boss@corp.com"),
            "email from new claims should be redacted"
        );
        assert!(
            !captured[0].contains("555-111-2222"),
            "phone from new claims should be redacted"
        );
        assert!(
            captured[0].contains("[redacted-email]"),
            "should contain email redaction placeholders"
        );
        assert!(
            captured[0].contains("[redacted-phone]"),
            "should contain phone redaction placeholders"
        );
    }

    #[tokio::test]
    async fn full_pipeline_redacts_pii_but_preserves_archive() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config_with_redaction(tmp.path());

        let knowledge_dir = tmp.path().join("knowledge");
        std::fs::create_dir_all(&knowledge_dir).expect("mkdir");
        std::fs::write(knowledge_dir.join("existing.md"), "# Old note").expect("write");

        let claims = vec![make_claim("Rust is fast", 7, "https://example.com")];
        let reflected = ReflectedGraph {
            claims: claims.clone(),
            proposed_links: vec![make_link(0, "existing", "supports")],
        };

        let llm = CapturingLlm {
            captured_user_messages: std::sync::Mutex::new(Vec::new()),
            reduce_response: serde_json::to_string(&claims).expect("serialize"),
            reflect_response: serde_json::to_string(&reflected).expect("serialize"),
            reweave_response: "# Updated note".to_string(),
        };

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "Contact admin@secret-corp.com for Rust info. Server at 10.0.0.1"
                .to_string(),
        };

        let graph_context = build_graph_context(tmp.path()).expect("context");
        let report = run_pipeline(input.clone(), &graph_context, &config, &llm)
            .await
            .expect("pipeline should succeed");

        // Archive should contain the ORIGINAL unredacted content
        let archive_content = std::fs::read_to_string(&report.archive_path).expect("read archive");
        assert!(
            archive_content.contains("admin@secret-corp.com"),
            "archive should preserve original PII content"
        );
        assert!(
            archive_content.contains("10.0.0.1"),
            "archive should preserve original IP address"
        );

        // LLM prompts should NOT contain PII
        let captured = llm.captured();
        for (i, msg) in captured.iter().enumerate() {
            assert!(
                !msg.contains("admin@secret-corp.com"),
                "LLM prompt {} should not contain email PII, got: {}",
                i,
                msg
            );
        }
    }

    #[tokio::test]
    async fn clean_content_passes_through_redaction_unchanged() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let config = test_config_with_redaction(tmp.path());

        let claims = vec![make_claim("Rust is safe", 7, "https://example.com")];
        let llm = CapturingLlm::new_for_reduce(&serde_json::to_string(&claims).expect("serialize"));

        let input = RawInput {
            source_url: "https://example.com".to_string(),
            raw_content: "An article about Rust memory safety with no PII".to_string(),
        };

        let _result = reduce(input, &config, &llm)
            .await
            .expect("reduce should succeed");

        let captured = llm.captured();
        assert!(
            captured[0].contains("An article about Rust memory safety with no PII"),
            "clean content should pass through unchanged"
        );
    }

    #[tokio::test]
    async fn redaction_default_enabled_in_config() {
        let config = DistilleryStageConfig::default();
        assert!(
            config.redact_llm_prompts,
            "PII redaction should be enabled by default"
        );
    }

    // -------------------------------------------------------------------
    // Surgical reweave tests
    // -------------------------------------------------------------------

    /// Set up a vault with an entity file for surgical reweave testing.
    fn setup_surgical_vault() -> (tempfile::TempDir, DistilleryStageConfig) {
        let vault = tempfile::TempDir::new().unwrap();
        let entities_dir = vault
            .path()
            .join("ledger")
            .join("concepts")
            .join("rust-safety");
        std::fs::create_dir_all(&entities_dir).unwrap();

        let rust_entity = "\
---
id: rust-safety
type: concept
space: knowledge
created: 2026-01-01T00:00:00Z
updated: 2026-01-01T00:00:00Z
---

# Rust Safety

## Facts
- Memory safety without garbage collection [source: manual] [type: finding]
- Borrow checker prevents data races [source: article-001] [type: finding]

## Relationships

## History
";
        std::fs::write(entities_dir.join("rust-safety.md"), rust_entity).unwrap();

        let config = DistilleryStageConfig {
            kb_root: vault.path().to_path_buf(),
            redact_llm_prompts: false,
        };

        (vault, config)
    }

    #[test]
    fn surgical_reweave_adds_supporting_claims() {
        let (_vault, config) = setup_surgical_vault();

        let graph = VerifiedGraph {
            claims: vec![AtomicClaim {
                content: "Rust prevents use-after-free bugs at compile time".to_string(),
                impact_score: 7,
                source_ref: "https://example.com/rust-safety".to_string(),
                observed_at: None,
            }],
            proposed_links: vec![ProposedLink {
                source_claim_idx: 0,
                target_node_id: "rust-safety".to_string(),
                relationship: "supports".to_string(),
                target_space: MemorySpace::Knowledge,
            }],
        };

        let report = reweave_surgical(&graph, &config).unwrap();

        assert_eq!(report.facts_added, 1);
        assert_eq!(report.facts_archived, 0);
        assert_eq!(report.notes_affected(), 1);

        // Verify the file was updated
        let content = std::fs::read_to_string(
            _vault
                .path()
                .join("ledger/concepts/rust-safety/rust-safety.md"),
        )
        .unwrap();
        assert!(content.contains("Rust prevents use-after-free bugs at compile time"));
        // Original facts still present
        assert!(content.contains("Memory safety without garbage collection"));
    }

    #[test]
    fn surgical_reweave_archives_contradicted_fact() {
        let (_vault, config) = setup_surgical_vault();

        let graph = VerifiedGraph {
            claims: vec![AtomicClaim {
                content: "Rust's borrow checker has known soundness holes".to_string(),
                impact_score: 8,
                source_ref: "https://example.com/soundness".to_string(),
                observed_at: None,
            }],
            proposed_links: vec![ProposedLink {
                source_claim_idx: 0,
                target_node_id: "rust-safety".to_string(),
                relationship: "contradicts".to_string(),
                target_space: MemorySpace::Knowledge,
            }],
        };

        let report = reweave_surgical(&graph, &config).unwrap();

        assert_eq!(report.facts_added, 1);
        assert_eq!(report.facts_archived, 1);

        let content = std::fs::read_to_string(
            _vault
                .path()
                .join("ledger/concepts/rust-safety/rust-safety.md"),
        )
        .unwrap();
        // New fact added
        assert!(content.contains("borrow checker has known soundness holes"));
        // Old fact is moved to Archived, not hard-deleted.
        assert!(content.contains("~~Borrow checker prevents data races~~"));
    }

    #[test]
    fn surgical_reweave_creates_missing_entity() {
        let (_vault, config) = setup_surgical_vault();

        let graph = VerifiedGraph {
            claims: vec![AtomicClaim {
                content: "Go uses garbage collection for memory management".to_string(),
                impact_score: 5,
                source_ref: "https://example.com/go".to_string(),
                observed_at: None,
            }],
            proposed_links: vec![ProposedLink {
                source_claim_idx: 0,
                target_node_id: "go-memory".to_string(),
                relationship: "extends".to_string(),
                target_space: MemorySpace::Knowledge,
            }],
        };

        let report = reweave_surgical(&graph, &config).unwrap();

        assert_eq!(report.entities_created, 1);
        assert_eq!(report.facts_added, 1);

        // New entity file should exist
        let path = _vault.path().join("ledger/concepts/go-memory/go-memory.md");
        assert!(path.exists());
    }

    #[test]
    fn surgical_reweave_multiple_contradictions_dont_double_archive() {
        let (_vault, config) = setup_surgical_vault();

        // Two claims both contradict facts in the same entity
        let graph = VerifiedGraph {
            claims: vec![
                AtomicClaim {
                    content: "Borrow checker has soundness bugs".to_string(),
                    impact_score: 8,
                    source_ref: "https://example.com/a".to_string(),
                    observed_at: None,
                },
                AtomicClaim {
                    content: "Memory safety requires runtime checks too".to_string(),
                    impact_score: 7,
                    source_ref: "https://example.com/b".to_string(),
                    observed_at: None,
                },
            ],
            proposed_links: vec![
                ProposedLink {
                    source_claim_idx: 0,
                    target_node_id: "rust-safety".to_string(),
                    relationship: "contradicts".to_string(),
                    target_space: MemorySpace::Knowledge,
                },
                ProposedLink {
                    source_claim_idx: 1,
                    target_node_id: "rust-safety".to_string(),
                    relationship: "contradicts".to_string(),
                    target_space: MemorySpace::Knowledge,
                },
            ],
        };

        let report = reweave_surgical(&graph, &config).unwrap();

        // Both claims should be added as facts
        assert_eq!(report.facts_added, 2);
        // Each should archive a DIFFERENT fact (2 original facts, 2 contradictions)
        assert_eq!(report.facts_archived, 2);

        // Verify the file is well-formed and parseable
        let content = std::fs::read_to_string(
            _vault
                .path()
                .join("ledger/concepts/rust-safety/rust-safety.md"),
        )
        .unwrap();
        let parsed = symbiotic_memory::vault_parser::parse_entity_file(&content).unwrap();
        assert_eq!(parsed.memories.len(), 4);
    }

    #[test]
    fn surgical_reweave_empty_graph_noop() {
        let (_vault, config) = setup_surgical_vault();

        let graph = VerifiedGraph {
            claims: vec![],
            proposed_links: vec![],
        };

        let report = reweave_surgical(&graph, &config).unwrap();
        assert_eq!(report.facts_added, 0);
        assert_eq!(report.notes_affected(), 0);
    }

    #[tokio::test]
    async fn process_text_for_entity_adds_fact_to_target_entity() {
        let tmp = tempfile::tempdir().unwrap();
        let entity_dir = tmp.path().join("ledger/concepts/rust");
        std::fs::create_dir_all(&entity_dir).unwrap();
        std::fs::write(
            entity_dir.join("rust.md"),
            "\
---
id: rust
type: concept
space: knowledge
sensitivity: shareable
created: 2026-04-06T00:00:00Z
updated: 2026-04-06T00:00:00Z
---

# Rust

## Facts

## Relationships

## History
",
        )
        .unwrap();

        let llm = TargetEntityLlm {
            reduce_response: r#"[{"content":"Rust uses ownership for memory safety","impact_score":8,"source_ref":"selection"}]"#.to_string(),
            reflect_response: r#"{"claims":[{"content":"Rust uses ownership for memory safety","impact_score":8,"source_ref":"selection"}],"proposed_links":[{"source_claim_idx":0,"target_node_id":"rust","relationship":"supports","target_space":"knowledge"}]}"#.to_string(),
        };
        let config = test_config(tmp.path());
        let target = EntityProcessTarget {
            entity_id: "rust".to_string(),
            entity_name: "Rust".to_string(),
            entity_type: symbiotic_memory::EntityType::Concept,
            space: MemorySpace::Knowledge,
            active_facts: Vec::new(),
        };
        let input = RawInput {
            source_url: "vault-process://rust".to_string(),
            raw_content: "Rust uses ownership for memory safety".to_string(),
        };

        let report = process_text_for_entity(input, &target, "thread:thread-rust", &config, &llm)
            .await
            .unwrap();

        assert_eq!(report.claims_extracted, 1);
        assert_eq!(report.claims_verified, 1);
        assert_eq!(report.reweave.facts_added, 1);
        assert_eq!(report.reweave.facts_archived, 0);

        let updated = std::fs::read_to_string(entity_dir.join("rust.md")).unwrap();
        assert!(updated.contains("Rust uses ownership for memory safety"));
        assert!(updated.contains("[source: thread:thread-rust]"));
    }

    #[tokio::test]
    async fn process_text_for_entity_returns_successful_noop_when_no_claims_match() {
        let tmp = tempfile::tempdir().unwrap();
        let entity_dir = tmp.path().join("ledger/concepts/rust");
        std::fs::create_dir_all(&entity_dir).unwrap();
        std::fs::write(
            entity_dir.join("rust.md"),
            "\
---
id: rust
type: concept
space: knowledge
sensitivity: shareable
created: 2026-04-06T00:00:00Z
updated: 2026-04-06T00:00:00Z
---

# Rust

## Facts
- Rust is compiled ahead of time [source: manual] [type: finding] [confidence: 0.8]

## Relationships

## History
",
        )
        .unwrap();

        let llm = TargetEntityLlm {
            reduce_response: r#"[{"content":"Alice prefers espresso","impact_score":4,"source_ref":"selection"}]"#.to_string(),
            reflect_response: r#"{"claims":[{"content":"Alice prefers espresso","impact_score":4,"source_ref":"selection"}],"proposed_links":[]}"#.to_string(),
        };
        let config = test_config(tmp.path());
        let target = EntityProcessTarget {
            entity_id: "rust".to_string(),
            entity_name: "Rust".to_string(),
            entity_type: symbiotic_memory::EntityType::Concept,
            space: MemorySpace::Knowledge,
            active_facts: vec!["Rust is compiled ahead of time".to_string()],
        };
        let input = RawInput {
            source_url: "vault-process://rust".to_string(),
            raw_content: "Alice prefers espresso".to_string(),
        };

        let report = process_text_for_entity(input, &target, "manual:process", &config, &llm)
            .await
            .unwrap();

        assert_eq!(report.claims_extracted, 1);
        assert_eq!(report.claims_verified, 1);
        assert!(report.reweave.mutations.is_empty());

        let updated = std::fs::read_to_string(entity_dir.join("rust.md")).unwrap();
        assert!(!updated.contains("Alice prefers espresso"));
    }

    #[test]
    fn find_most_similar_works() {
        let facts = vec![
            "Memory safety without garbage collection".to_string(),
            "Borrow checker prevents data races".to_string(),
            "Compile-time error checking".to_string(),
        ];

        // Should match "Borrow checker" fact
        let result = find_most_similar(
            "The borrow checker has soundness issues with data races",
            &facts,
        );
        assert_eq!(
            result,
            Some("Borrow checker prevents data races".to_string())
        );
    }

    #[test]
    fn find_most_similar_returns_none_below_threshold() {
        let facts = vec!["Completely unrelated fact about cooking".to_string()];

        let result = find_most_similar("Rust memory safety guarantees", &facts);
        assert_eq!(result, None); // No overlap above threshold
    }

    #[test]
    fn claim_confidence_maps_correctly() {
        let low = AtomicClaim {
            content: "x".to_string(),
            impact_score: 1,
            source_ref: "".to_string(),
            observed_at: None,
        };
        let high = AtomicClaim {
            content: "x".to_string(),
            impact_score: 10,
            source_ref: "".to_string(),
            observed_at: None,
        };

        assert!((claim_confidence(&low) - 0.55).abs() < f64::EPSILON);
        assert!((claim_confidence(&high) - 1.0).abs() < f64::EPSILON);
    }
}
