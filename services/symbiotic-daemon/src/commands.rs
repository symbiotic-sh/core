//! ControlCommand dispatch logic.
//!
//! Contains the main `route_matrix_message` and `route_matrix_message_with_targets`
//! methods on `SymbioticDaemon`, plus intake message routing, sender authorization,
//! room classification, and the transport pump / send helpers.

use anyhow::Result;
use credential_gateway::CredentialVault;
use log::{debug, info, warn};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use symbiotic_agents::llm::LlmClient;
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;
use symbiotic_matrix::transport::{MatrixMessage, MatrixTransport};
use symbiotic_memory::recall_probes::{
    RecallProbeBaselineTarget, RecallProbeRunOutcome, RecallProbeStore, RecallProbeSubject,
    RecallProbeTargetKind, RecallRemediationFlag,
};
use symbiotic_memory::store::MemoryStore;
use symbiotic_memory::vault_writer::{MutationKind, NewFactMetadata, VaultMutation};
use symbiotic_vault_store::keys::{ExposeSecret, Identity as AgeIdentity};

use crate::auth_orchestrator::AuthOrchestrator;
use crate::auto_promotion::handle_promotion_command;
use crate::entity_profiles::{
    build_entity_profile_updated_envelope, preserved_referenced_in_titles, EntityProfileGenerator,
};
use crate::events::{simple_hash, RoomCreationRequest, RoutedMatrixEnvelope};
use crate::goal_state::*;
use crate::memory_docs::compute_content_hash;
use crate::proposals;
use crate::push::*;
use crate::recall_probes::{
    build_probe_subjects, collect_recall_probe_escalations, run_recall_probe_batch,
    run_recall_probe_batch_for_subjects, PERIODIC_RECALL_PROBE_COHORT,
};
use crate::routing::*;
use crate::secrets;
use crate::thread_manager::ThreadManager;
use crate::topic_router::TopicRouter;
use crate::ux_classifier::{UxClass, UxClassifier};
use crate::SymbioticDaemon;

// ---------------------------------------------------------------------------
// System prompts
// ---------------------------------------------------------------------------

/// Rich conversational system prompt for the Brain Bootstrap and general chat.
///
/// Designed to make Symbiotic warm, curious, and proactive rather than a
/// generic Q&A bot. Drives the onboarding interview naturally while also
/// handling day-to-day conversation once the bootstrap phase is over.
pub(crate) const BRAIN_BOOTSTRAP_SYSTEM_PROMPT: &str = "\
You are Symbiotic — a personal AI that learns and grows with the user over time. \
You are not a search engine or a one-shot Q&A bot. You are the user's long-term \
thinking partner, extended arm, and memory.

Your personality:
- Warm, direct, and curious. Never robotic, never preachy.
- Keep responses concise: 2-4 sentences is ideal. Only go longer when the topic genuinely demands it.
- Ask follow-up questions naturally — you want to understand the person, not just answer their question.
- Be proactive: suggest next steps, spot patterns, offer to help before being asked.

Your core job right now:
- Learn about the user: their role, projects, tech stack, priorities, goals, key people, and preferences.
- Every answer they give is a fact you're building into their brain (memory graph).
- When you've learned enough context around a topic, suggest promoting it into a goal with a concrete plan.

Conversation style:
- Match the user's energy and tone. If they're terse, be terse. If they're chatty, engage.
- Never lecture. Never repeat back what they just said unless clarifying ambiguity.
- Use short acknowledgments ('Got it.', 'Nice.', 'Makes sense.') then move the conversation forward.
- When you don't know something about the user, ask — don't guess.

Things you should NEVER do:
- Don't apologize excessively or use filler phrases ('I'd be happy to help!', 'Great question!').
- Don't give unsolicited life advice or motivational speeches.
- Don't start responses with 'As an AI...' or 'I'm just a language model...'.
- Don't dump walls of text. If something needs detail, use bullet points or structure.
";

/// Task-focused system prompt for ShortTask classification.
///
/// Keeps the identity and tone consistent with the conversational prompt but
/// shifts focus to completing a concrete request and returning a clear result.
pub(crate) const SHORT_TASK_SYSTEM_PROMPT: &str = "\
You are Symbiotic — a personal AI operating system that acts as the user's extended arm. \
The user has given you a specific task to complete. Execute it thoroughly and return a clear result.

Guidelines:
- Be direct and structured. Use bullet points, code blocks, or tables when they help clarity.
- If the task is ambiguous, make a reasonable interpretation and note your assumption briefly.
- Include only what the user needs — no preamble, no filler.
- When relevant, suggest a concrete next step at the end (one sentence max).
";

/// System prompt for LLM-based UX classification fallback.
///
/// Used when the rule-based `UxClassifier` returns low confidence (< 0.75).
/// The LLM picks between quick/short_task/goal with a single-word response.
pub(crate) const UX_CLASSIFIER_LLM_PROMPT: &str = "\
You are a message classifier. Classify the user's message as exactly one of:\n\
- \"quick\" — simple question, greeting, factual lookup, or casual conversation\n\
- \"short_task\" — single-step task like summarize, translate, draft, or analyze\n\
- \"goal\" — complex multi-step work requiring planning and execution\n\
\n\
Reply with ONLY the classification word, nothing else.";

/// Parse an LLM classification response into a [`UxClass`].
///
/// Tolerates whitespace, quotes, and surrounding punctuation.
/// Returns `None` if the response does not contain a recognized class.
pub(crate) fn parse_llm_classification(response: &str) -> Option<UxClass> {
    let cleaned = response
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == '`')
        .to_lowercase();
    match cleaned.as_str() {
        "quick" => Some(UxClass::Quick),
        "short_task" => Some(UxClass::ShortTask),
        "goal" => Some(UxClass::Goal),
        _ => None,
    }
}

fn only_declared_condition_matches(
    task: &crate::goal_management::PlannedTaskRecord,
    condition_kind: &str,
) -> bool {
    let mut remaining = 0usize;
    if task.declared_context.waiting_for.is_some() {
        remaining += usize::from(condition_kind != "waiting_for");
    }
    if task.declared_context.review_target.is_some() {
        remaining += usize::from(condition_kind != "review_target");
    }
    if task.declared_context.coordination_target.is_some() {
        remaining += usize::from(condition_kind != "coordination_target");
    }
    if task.declared_context.external_dependency.is_some() {
        remaining += usize::from(condition_kind != "external_dependency");
    }
    remaining == 0
}

/// The initial greeting message sent to the stream room when the daemon
/// connects for the first time (Brain Bootstrap, Depth 1).
pub(crate) const BRAIN_BOOTSTRAP_GREETING: &str = "\
Hey. I'm going to be working with you for a long time \
— let me learn the basics so I'm not starting from zero.\n\n\
What do you do? (role, industry, main focus)";

struct EntityProfileRefresh {
    entity_id: String,
    content_hash: String,
    doc_path: PathBuf,
}

struct VaultProcessExecution {
    detail: String,
    claims_extracted: usize,
    claims_verified: usize,
    facts_added: usize,
    facts_archived: usize,
    mutations: Vec<VaultMutation>,
}

impl SymbioticDaemon {
    fn vault_kb_root(&self) -> std::path::PathBuf {
        self.config
            .archive_path
            .clone()
            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"))
    }

    fn apply_vault_edit_follow_through(
        &self,
        mutations: &[VaultMutation],
        source: Option<&str>,
    ) -> Result<(Option<String>, usize, Vec<EntityProfileRefresh>), String> {
        let vault_root = self.vault_kb_root();

        let commit = symbiotic_memory::vault_git::commit_mutations(&vault_root, mutations, source)
            .map_err(|e| format!("git commit failed: {e}"))?;

        let files_indexed = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let indexer = std::sync::Arc::clone(&self.vault_indexer);
                let root = vault_root.clone();
                std::thread::scope(|s| {
                    s.spawn(move || {
                        handle.block_on(async move {
                            let indexer = indexer.lock().await;
                            indexer.initialize().await?;
                            indexer.update(&root).await
                        })
                    })
                    .join()
                    .expect("vault reindex thread should not panic")
                })
                .map_err(|e| format!("re-index failed: {e}"))?
                .files_indexed
            }
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("failed to build runtime for re-index: {e}"))?;
                runtime
                    .block_on(async {
                        let indexer = self.vault_indexer.lock().await;
                        indexer.initialize().await?;
                        indexer.update(&vault_root).await
                    })
                    .map_err(|e| format!("re-index failed: {e}"))?
                    .files_indexed
            }
        };

        let mut entity_ids = BTreeSet::new();
        for mutation in mutations {
            entity_ids.insert(mutation.entity_id.clone());
        }

        let mut profile_refreshes = Vec::new();
        for entity_id in entity_ids {
            let refresh = self
                .refresh_entity_profile_artifact(&vault_root, &entity_id)
                .map_err(|e| format!("entity profile refresh failed for {entity_id}: {e}"))?;
            if let Some(refresh) = refresh {
                profile_refreshes.push(refresh);
            }
        }

        Ok((commit.commit_hash, files_indexed, profile_refreshes))
    }

    fn refresh_entity_profile_artifact(
        &self,
        vault_root: &std::path::Path,
        entity_id: &str,
    ) -> Result<Option<EntityProfileRefresh>, String> {
        let generator = EntityProfileGenerator::new(vault_root);

        let (entity, memories, relationships) = match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let store = std::sync::Arc::clone(&self.memory_store);
                let entity_id = entity_id.to_string();
                std::thread::scope(|s| {
                    s.spawn(move || {
                        handle.block_on(async move {
                            let entity = store.get_entity(&entity_id).await?;
                            let memories = store.get_memories(&entity_id, None).await?;
                            let relationships = store.get_relationships(&entity_id).await?;
                            Ok::<_, symbiotic_memory::types::MemoryStoreError>((
                                entity,
                                memories,
                                relationships,
                            ))
                        })
                    })
                    .join()
                    .expect("entity profile refresh thread should not panic")
                })
            }
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| {
                        format!("failed to build runtime for entity profile refresh: {e}")
                    })?;
                runtime.block_on(async {
                    let entity = self.memory_store.get_entity(entity_id).await?;
                    let memories = self.memory_store.get_memories(entity_id, None).await?;
                    let relationships = self.memory_store.get_relationships(entity_id).await?;
                    Ok::<_, symbiotic_memory::types::MemoryStoreError>((
                        entity,
                        memories,
                        relationships,
                    ))
                })
            }
        }
        .map_err(|e| e.to_string())?;

        let profile_path = generator.profile_path(&entity);
        let referenced_in =
            preserved_referenced_in_titles(&profile_path).map_err(|e| e.to_string())?;
        let generated_path = generator
            .generate(&entity, &memories, &relationships, &referenced_in)
            .map_err(|e| e.to_string())?;
        let content = std::fs::read_to_string(&generated_path)
            .map_err(|e| format!("failed to read refreshed entity profile: {e}"))?;

        Ok(Some(EntityProfileRefresh {
            entity_id: entity.id,
            content_hash: compute_content_hash(&content),
            doc_path: generated_path,
        }))
    }

    pub(crate) fn handle_vault_process_command_with_llm(
        &self,
        message: &MatrixMessage,
        value: &serde_json::Value,
        now: u64,
        llm: &dyn LlmClient,
    ) -> Result<Vec<RoutedMatrixEnvelope>, String> {
        let entity_id = value
            .get("entity_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let text = value
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let source = value
            .get("source")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("manual:process");

        if entity_id.is_empty() || text.is_empty() {
            let event = MatrixEventEnvelope::new(
                Kind::Message,
                Status::Fail,
                now,
                "vault.process requires entity_id and non-empty text",
            )
            .with_detail_field("room", &*message.room_id);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        let execution = self.execute_vault_process_with_llm(entity_id, text, source, llm)?;

        let commit_source = format!("matrix:{}", message.sender);
        let (status, detail, commit_hash, files_indexed, profile_refreshes) = if execution
            .mutations
            .is_empty()
        {
            (
                Status::Success,
                execution.detail,
                None,
                None,
                Vec::<EntityProfileRefresh>::new(),
            )
        } else {
            match self.apply_vault_edit_follow_through(&execution.mutations, Some(&commit_source)) {
                Ok((commit_hash, files_indexed, profile_refreshes)) => (
                    Status::Success,
                    execution.detail,
                    commit_hash,
                    Some(files_indexed),
                    profile_refreshes,
                ),
                Err(e) => (
                    Status::Fail,
                    format!("vault.process applied but follow-through failed: {e}"),
                    None,
                    None,
                    Vec::new(),
                ),
            }
        };

        let mut event = MatrixEventEnvelope::new(Kind::Message, status, now, &detail)
            .with_detail_field("room", &*message.room_id)
            .with_detail_field("entity_id", entity_id)
            .with_detail_field("operation", "process")
            .with_detail_field("claims_extracted", execution.claims_extracted)
            .with_detail_field("claims_verified", execution.claims_verified)
            .with_detail_field("facts_added", execution.facts_added)
            .with_detail_field("facts_archived", execution.facts_archived)
            .with_detail_field(
                "mutations_json",
                serde_json::to_string(&serialize_mutations(&execution.mutations))
                    .unwrap_or_else(|_| "[]".to_string()),
            );
        if let Some(ref commit_hash) = commit_hash {
            event = event.with_detail_field("git_commit", commit_hash.clone());
        }
        if let Some(files_indexed) = files_indexed {
            event = event.with_detail_field("files_indexed", files_indexed);
        }

        let mut envelopes = vec![RoutedMatrixEnvelope {
            room_id: message.room_id.clone(),
            envelope: event,
        }];
        if status == Status::Success {
            for refresh in profile_refreshes {
                envelopes.push(build_entity_profile_updated_envelope(
                    &refresh.entity_id,
                    &refresh.content_hash,
                    &refresh.doc_path,
                    now,
                    &message.room_id,
                ));
            }
        }

        Ok(envelopes)
    }

    fn execute_vault_process_with_llm(
        &self,
        entity_id: &str,
        text: &str,
        source: &str,
        llm: &dyn LlmClient,
    ) -> Result<VaultProcessExecution, String> {
        let target = self.load_entity_process_target(entity_id)?;
        let config = symbiotic_intake::distillery::DistilleryStageConfig {
            kb_root: self.vault_kb_root(),
            redact_llm_prompts: true,
        };
        let input = symbiotic_intake::distillery::RawInput {
            source_url: format!("vault-process://{entity_id}"),
            raw_content: text.to_string(),
        };

        let report = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(move || {
                    handle.block_on(symbiotic_intake::distillery::process_text_for_entity(
                        input, &target, source, &config, llm,
                    ))
                })
                .join()
                .expect("vault.process thread should not panic")
            }),
            Err(_) => {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("failed to build runtime for vault.process: {e}"))?;
                runtime.block_on(symbiotic_intake::distillery::process_text_for_entity(
                    input, &target, source, &config, llm,
                ))
            }
        }
        .map_err(|e| format!("vault.process failed: {e}"))?;

        let detail = if report.reweave.mutations.is_empty() {
            format!("Processed text for {entity_id}: no durable canonical changes extracted")
        } else {
            format!(
                "Processed text for {entity_id}: {} fact(s) added, {} archived",
                report.reweave.facts_added, report.reweave.facts_archived
            )
        };

        Ok(VaultProcessExecution {
            detail,
            claims_extracted: report.claims_extracted,
            claims_verified: report.claims_verified,
            facts_added: report.reweave.facts_added,
            facts_archived: report.reweave.facts_archived,
            mutations: report.reweave.mutations,
        })
    }

    fn load_entity_process_target(
        &self,
        entity_id: &str,
    ) -> Result<symbiotic_intake::distillery::EntityProcessTarget, String> {
        let vault_root = self.vault_kb_root();
        let entity_path =
            symbiotic_memory::vault_layout::find_canonical_entity_file(&vault_root, entity_id)
                .map_err(|e| format!("failed to locate canonical entity file: {e}"))?
                .ok_or_else(|| format!("entity not found: {entity_id}"))?;
        let content = std::fs::read_to_string(&entity_path)
            .map_err(|e| format!("failed to read canonical entity file: {e}"))?;
        let parsed = symbiotic_memory::vault_parser::parse_entity_file(&content)
            .map_err(|e| format!("failed to parse canonical entity file: {e}"))?;

        let active_facts = parsed
            .memories
            .into_iter()
            .filter(|memory| memory.status == symbiotic_memory::MemoryStatus::Active)
            .map(|memory| memory.fact)
            .collect();

        Ok(symbiotic_intake::distillery::EntityProcessTarget {
            entity_id: parsed.entity.id,
            entity_name: parsed.entity.name,
            entity_type: parsed.entity.entity_type,
            space: parsed.entity.space,
            active_facts,
        })
    }

    /// Returns `true` when the sender is authorized to issue commands.
    ///
    /// Authorization is checked against `allowed_senders`.
    /// When the set is empty, access is denied unless explicit open-access
    /// mode is enabled for local development.
    pub(crate) fn is_sender_authorized(&self, sender: &str) -> bool {
        if self.allowed_senders.is_empty() {
            return self.allow_open_access;
        }
        self.allowed_senders
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(sender))
    }

    /// Classify a room_id into a role.  Prefers the persisted room-role map
    /// when configured; falls back to alias-pattern matching for backwards
    /// compatibility with file-transport tests.
    pub(crate) fn classify_room(&self, room_id: &str) -> Option<RoomRole> {
        if self.room_roles.is_configured() {
            return self.room_roles.role_for(room_id);
        }
        // Fallback: alias-pattern routing for dev/test
        if is_intake_room(room_id) {
            Some(RoomRole::Intake)
        } else if is_control_room(room_id) {
            Some(RoomRole::Control)
        } else if is_status_room(room_id) {
            Some(RoomRole::Status)
        } else if is_alerts_room(room_id) {
            Some(RoomRole::Alerts)
        } else if is_credentials_room(room_id) {
            Some(RoomRole::Credentials)
        } else if is_goals_room(room_id) {
            Some(RoomRole::Goals)
        } else if is_stream_room(room_id) {
            Some(RoomRole::Stream)
        } else {
            None
        }
    }

    pub fn route_matrix_message(
        &self,
        message: &MatrixMessage,
        now: u64,
    ) -> Result<Vec<MatrixEventEnvelope>> {
        let routed = self.route_matrix_message_with_targets(message, now)?;
        Ok(routed
            .into_iter()
            .filter(|item| item.room_id == message.room_id)
            .map(|item| item.envelope)
            .collect())
    }

    pub fn route_matrix_message_with_targets(
        &self,
        message: &MatrixMessage,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let role = self.classify_room(&message.room_id);
        // Never preview payloads for control/credentials rooms. These commands can
        // carry secrets or other sensitive material.
        let body_preview = match role {
            Some(RoomRole::Control) => "[REDACTED — control payload]".to_string(),
            _ if message.body.contains("secret") || message.body.contains("SECRET") => {
                "[REDACTED — contains secret payload]".to_string()
            }
            _ if is_credentials_room(&message.room_id) => {
                "[REDACTED — credentials payload]".to_string()
            }
            _ => message.body[..message.body.len().min(80)].to_string(),
        };
        debug!(
            "route: room_id={} sender={} role={:?} body_len={} body_preview={}",
            message.room_id,
            message.sender,
            role,
            message.body.len(),
            body_preview
        );
        if message.body.len() > self.config.max_matrix_message_bytes {
            let event = MatrixEventEnvelope::new(
                Kind::Message,
                Status::Fail,
                now,
                "Message exceeds max allowed size",
            )
            .with_detail_field("room", &*message.room_id)
            .with_detail_field("size_bytes", message.body.len())
            .with_detail_field("limit_bytes", self.config.max_matrix_message_bytes);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        if let Some(forwarded) = self.escalation_forward_from_failed_event(message, now)? {
            return Ok(vec![forwarded]);
        }

        // F2: Sender authorization gate
        if !self.is_sender_authorized(&message.sender) {
            let event =
                MatrixEventEnvelope::new(Kind::Message, Status::Fail, now, "Sender not authorized")
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field("sender", &*message.sender);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        // F1: Route by room-role map when configured, else fall back to alias patterns
        // (role already computed at top of function for debug logging)

        if role == Some(RoomRole::Intake) || (role.is_none() && is_intake_room(&message.room_id)) {
            let reply = self.handle_intake_message(&message.body)?;
            debug!(
                "intake: ingested={} duplicates={} invalid={} failed={} body_preview={}",
                reply.result.summary.ingested,
                reply.result.summary.duplicates,
                reply.result.summary.invalid,
                reply.result.summary.failed,
                &reply.body[..reply.body.len().min(120)]
            );
            let accepted_count = reply.result.summary.ingested
                + reply.result.summary.duplicates
                + reply.result.summary.secure_routed;
            let intake_status = if accepted_count > 0 {
                Status::Working
            } else {
                Status::Fail
            };
            let mut event =
                MatrixEventEnvelope::new(Kind::Message, intake_status, now, &reply.body);
            event = event
                .with_detail_field("total", reply.result.summary.total)
                .with_detail_field("accepted", accepted_count)
                .with_detail_field("ingested", reply.result.summary.ingested)
                .with_detail_field("duplicates", reply.result.summary.duplicates)
                .with_detail_field("invalid", reply.result.summary.invalid)
                .with_detail_field("failed", reply.result.summary.failed);
            // Add first URL to event details for feed display
            if let Some(first_item) = reply.result.items.first() {
                if let Some(ref url) = first_item.normalized_url {
                    event = event.with_detail_field("url", url.as_str());
                }
            }
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        if role == Some(RoomRole::Control) || (role.is_none() && is_control_room(&message.room_id))
        {
            // Forward goal.answer, goal.plan.*, and explicit goal.task.* commands
            // to the goal handler
            // (below the control room block). The app sends these to #control,
            // so we must NOT let them fall into parse_control_command → Unknown.
            let is_goal_forward = if message.body.starts_with('{') {
                // v2 (sym.c) format only
                let cmd_name =
                    crate::routing::extract_command_from_json(&message.body).map(|ext| ext.command);
                cmd_name.is_some_and(|cmd| {
                    matches!(
                        cmd.as_str(),
                        "goal.answer"
                            | "goal.message"
                            | "goal.plan.approved"
                            | "goal.plan.rejected"
                            | "goal.task.condition.set"
                            | "goal.task.condition.satisfied"
                            | "goal.task.transition"
                            | "goal.task.assign"
                            | "thread.promotion.approve"
                            | "thread.promotion.dismiss"
                            | "vault.edit"
                            | "vault.process"
                    )
                })
            } else {
                false
            };

            if is_goal_forward {
                debug!("forwarding goal command from #control to goal handler");
                // Fall through to the goal handler at the bottom of this function.
            } else {
                let command = parse_control_command(&message.body);
                return match command {
                    ControlCommand::GoalList => {
                        let states = self.list_goal_states()?;
                        let summary = if states.is_empty() {
                            "No goal runs found".to_string()
                        } else {
                            format!("{} goal runs", states.len())
                        };
                        let items = states
                            .iter()
                            .map(|state| {
                                format!(
                                    "{}:{}:{}",
                                    room_alias(&state.goal_room),
                                    state.template,
                                    state.status
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(";");
                        let items_json = serde_json::to_string(
                            &states
                                .iter()
                                .map(|state| {
                                    serde_json::json!({
                                        "room": room_alias(&state.goal_room),
                                        "template": &state.template,
                                        "status": &state.status,
                                        "job_id": &state.last_job_id,
                                        "run_id": &state.last_run_id,
                                        "owner": &state.owner,
                                        "updated_at": state.updated_at,
                                    })
                                })
                                .collect::<Vec<_>>(),
                        )
                        .unwrap_or_else(|_| "[]".to_string());
                        let event =
                            MatrixEventEnvelope::new(Kind::Message, Status::Success, now, &summary)
                                .with_detail_field("room", &*message.room_id)
                                .with_detail_field("sender", &*message.sender)
                                .with_detail_field("count", states.len())
                                .with_detail_field("items", items)
                                .with_detail_field("items_json", items_json)
                                .with_detail_field("items_version", 2);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::GoalDeliberate { description } => {
                        let event_result = self.process_goal_through_pipeline(
                            &description,
                            &message.room_id,
                            &message.sender,
                            now,
                        )?;

                        // Map pipeline-specific statuses to v2 Status enum
                        let v2_status = match event_result.status.as_str() {
                            "classifying" | "auto_executing" | "running" => Status::Working,
                            "awaiting_approval" => Status::Awaiting,
                            "completed" => Status::Success,
                            "failed" => Status::Fail,
                            _ => Status::Working,
                        };
                        let mut envelope = MatrixEventEnvelope::new(
                            Kind::Message,
                            v2_status,
                            now,
                            &event_result.detail,
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("description", &*description)
                        .with_detail_field(
                            "title",
                            description.chars().take(80).collect::<String>(),
                        )
                        .with_detail_field("template", "deliberation")
                        .with_detail_field("pipeline_status", event_result.status.as_str());
                        if let Some(ref gid) = event_result.goal_id {
                            envelope = envelope.with_detail_field("goal_id", &**gid);
                            // Legacy compatibility: use goal_id as the initial
                            // thread identity when a dedicated thread surface
                            // has not been created yet.
                            envelope = envelope.with_thread(gid);
                        }

                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        }])
                    }
                    ControlCommand::GoalStart { template } => {
                        // Check if this goal is already active (dedup)
                        if let Some(_existing_job_id) =
                            self.find_active_goal_workflow(&message.room_id, &template)?
                        {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Working,
                                now,
                                "Goal is already running",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("template", template);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let (job_id, goal_id) = self.queue_workflow_run_for_goal(
                            &template,
                            &message.room_id,
                            &message.sender,
                        )?;
                        append_goal_log(
                            &self.config.goal_log_file,
                            GoalLogEntry {
                                ts: now,
                                event: "goal.started",
                                workflow_job_id: &job_id,
                                goal_room: Some(&message.room_id),
                                goal_sender: Some(&message.sender),
                                template: &template,
                                detail: "queued",
                            },
                        )?;
                        upsert_goal_state(
                            &self.config.goal_state_file,
                            GoalState {
                                goal_room: message.room_id.clone(),
                                thread_id: None,
                                project_id: crate::goals::default_unscoped_project_id(),
                                template: template.clone(),
                                status: "queued".to_string(),
                                last_job_id: job_id.clone(),
                                last_run_id: None,
                                owner: Some(message.sender.clone()),
                                updated_at: now,
                                complexity: None,
                                pipeline_stage: None,
                                audit_id: None,
                                plan_id: None,
                            },
                        )?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Goal run accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("template", template)
                        .with_detail_field("goal_id", goal_id);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::GoalRetry { template } => {
                        let (job_id, goal_id) = self.queue_workflow_run_for_goal(
                            &template,
                            &message.room_id,
                            &message.sender,
                        )?;
                        append_goal_log(
                            &self.config.goal_log_file,
                            GoalLogEntry {
                                ts: now,
                                event: "goal.retry",
                                workflow_job_id: &job_id,
                                goal_room: Some(&message.room_id),
                                goal_sender: Some(&message.sender),
                                template: &template,
                                detail: "queued",
                            },
                        )?;
                        upsert_goal_state(
                            &self.config.goal_state_file,
                            GoalState {
                                goal_room: message.room_id.clone(),
                                thread_id: None,
                                project_id: crate::goals::default_unscoped_project_id(),
                                template: template.clone(),
                                status: "queued".to_string(),
                                last_job_id: job_id.clone(),
                                last_run_id: None,
                                owner: Some(message.sender.clone()),
                                updated_at: now,
                                complexity: None,
                                pipeline_stage: None,
                                audit_id: None,
                                plan_id: None,
                            },
                        )?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Goal retry accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("template", template)
                        .with_detail_field("goal_id", goal_id);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::GoalStop { template } => {
                        let Some(job_id) =
                            self.find_active_goal_workflow(&message.room_id, &template)?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No active goal run to stop",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("template", template)
                            .with_detail_field("hint", "start a goal first");
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        append_goal_log(
                            &self.config.goal_log_file,
                            GoalLogEntry {
                                ts: now,
                                event: "goal.stop_requested",
                                workflow_job_id: &job_id,
                                goal_room: Some(&message.room_id),
                                goal_sender: Some(&message.sender),
                                template: &template,
                                detail: "queued",
                            },
                        )?;
                        upsert_goal_state(
                            &self.config.goal_state_file,
                            GoalState {
                                goal_room: message.room_id.clone(),
                                thread_id: None,
                                project_id: crate::goals::default_unscoped_project_id(),
                                template: template.clone(),
                                status: "cancel_requested".to_string(),
                                last_job_id: job_id.clone(),
                                last_run_id: None,
                                owner: Some(message.sender.clone()),
                                updated_at: now,
                                complexity: None,
                                pipeline_stage: None,
                                audit_id: None,
                                plan_id: None,
                            },
                        )?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Goal stop requested",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("template", template);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::RunWorkflow { template } => {
                        let _job_id = self.queue_workflow_run(&template)?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Workflow accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "workflow.run")
                        .with_detail_field("template", template);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallRun { mode, install_id } => {
                        let resolved_install_id = install_id
                            .clone()
                            .unwrap_or_else(|| format!("install-{}", symbiotic_queue::now_unix()));
                        let _job_id = self.queue_install_run(&mode, &resolved_install_id)?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Install run accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "install.run")
                        .with_detail_field("mode", mode)
                        .with_detail_field("install_id", resolved_install_id);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallProvision { mode, install_id } => {
                        let resolved_install_id = install_id
                            .clone()
                            .unwrap_or_else(|| format!("install-{}", symbiotic_queue::now_unix()));
                        let _job_id = self.queue_install_provision(&mode, &resolved_install_id)?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Install provision accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "install.provision")
                        .with_detail_field("mode", mode)
                        .with_detail_field("install_id", resolved_install_id);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallBootstrap => {
                        let _job_id = self.queue_install_bootstrap()?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Install bootstrap accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "install.bootstrap");
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallVerify { mode, install_id } => {
                        let _job_id = self.queue_install_verify(&mode, install_id.as_deref())?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Install verify accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "install.verify")
                        .with_detail_field("mode", mode);
                        let event = if let Some(install_id) = install_id {
                            event.with_detail_field("install_id", install_id)
                        } else {
                            event
                        };
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::RecallProbeRun {
                        top_k,
                        max_subjects,
                        max_queries_per_subject,
                    } => self.handle_recall_probe_run(
                        &message.room_id,
                        top_k,
                        max_subjects,
                        max_queries_per_subject,
                        now,
                    ),
                    ControlCommand::RecallProbeStatus { run_id } => {
                        self.handle_recall_probe_status(&message.room_id, run_id, now)
                    }
                    ControlCommand::RecallProbeSummary {
                        target_kind,
                        target_id,
                    } => self.handle_recall_probe_summary(
                        &message.room_id,
                        target_kind,
                        target_id,
                        now,
                    ),
                    ControlCommand::RecallProbeHealth { limit } => {
                        self.handle_recall_probe_health(&message.room_id, limit, now)
                    }
                    ControlCommand::RecallProbeRegressions { run_id, limit } => {
                        self.handle_recall_probe_regressions(&message.room_id, run_id, limit, now)
                    }
                    ControlCommand::IssueAuth { target, scopes } => {
                        let _job_id = self.queue_auth_issue_request(&target, scopes.clone())?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Auth request accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "auth.issue")
                        .with_detail_field("target", target)
                        .with_detail_field("scopes", scopes.join(","));
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::BookmarksSync { source, limit } => {
                        let _job_id = self.queue_bookmarks_sync(&source, limit)?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Working,
                            now,
                            "Bookmarks sync accepted",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "bookmarks.sync")
                        .with_detail_field("source", source)
                        .with_detail_field("limit", limit);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::RegisterPush {
                        device_id,
                        token,
                        platform,
                    } => {
                        let record =
                            self.register_push_device(&device_id, &token, &platform, now)?;
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Push device registered",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("type", "push.register")
                        .with_detail_field("device_id", record.device_id)
                        .with_detail_field("platform", record.platform)
                        .with_detail_field("token_hash", record.token_hash);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::PushAck {
                        notification_id,
                        run_id,
                        device_id,
                    } => {
                        record_push_ack(
                            &self.config.push_ack_file,
                            &notification_id,
                            run_id.as_deref(),
                            now,
                        )?;
                        // Reset badge count for the device if device_id is provided.
                        if let Some(ref did) = device_id {
                            self.push_registry.reset_badge(did);
                        }
                        let event = MatrixEventEnvelope::state(
                            "push.ack",
                            now,
                            "Push acknowledgement recorded",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("notification_id", &*notification_id)
                        .with_detail_field("run_id", run_id.clone().unwrap_or_default())
                        .with_detail_field("device_id", device_id.clone().unwrap_or_default())
                        .with_detail_field("badge_reset", device_id.is_some());
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::UnregisterPush { device_id } => {
                        let removed = self.push_registry.unregister(&device_id)?;
                        let (_status, body) = if removed {
                            ("completed", "Push device unregistered")
                        } else {
                            ("completed", "Push device not found (already unregistered)")
                        };
                        let event = MatrixEventEnvelope::state("push.unregistered", now, body)
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("device_id", &*device_id)
                            .with_detail_field("removed", removed);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::UpdatePushPreferences {
                        failures,
                        goal_completions,
                        capture_confirmations,
                        install_progress,
                    } => {
                        let mut updated = false;
                        if let Ok(mut prefs) = self.push_preferences.lock() {
                            if let Some(v) = failures {
                                prefs.failures = v;
                            }
                            if let Some(v) = goal_completions {
                                prefs.goal_completions = v;
                            }
                            if let Some(v) = capture_confirmations {
                                prefs.capture_confirmations = v;
                            }
                            if let Some(v) = install_progress {
                                prefs.install_progress = v;
                            }
                            prefs.enforce_invariants();
                            let _ = prefs.save(&self.config.push_preferences_file);
                            updated = true;
                        }
                        let event = MatrixEventEnvelope::state(
                            "push.preferences",
                            now,
                            if updated {
                                "Push preferences updated"
                            } else {
                                "Failed to update push preferences"
                            },
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallSecretPut { mode, key, value } => {
                        // Validate key against the explicit setup mode.
                        if !secrets::is_allowed_secret_key(&key, &mode) {
                            let event = MatrixEventEnvelope::state(
                                "install.secret.rejected",
                                now,
                                "Unknown secret key",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("mode", &*mode)
                            .with_detail_field("key", &*key)
                            .with_detail_field("reason", "key not allowed for selected mode");
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        if value.is_empty() {
                            let event = MatrixEventEnvelope::state(
                                "install.secret.rejected",
                                now,
                                "Secret value must not be empty",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("mode", &*mode)
                            .with_detail_field("key", &*key);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        // Write secret — NEVER log or emit the value
                        secrets::put_secret(&self.config.secrets_file, &key, &value)?;
                        let event = MatrixEventEnvelope::state(
                            "install.secret.stored",
                            now,
                            "Secret stored",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("mode", &*mode)
                        .with_detail_field("key", &*key)
                        .with_detail_field("status", "present");
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::InstallSecretValidate { mode } => {
                        let status_map =
                            secrets::validate_secrets(&self.config.secrets_file, &mode)?;
                        let all_present =
                            secrets::all_required_present(&self.config.secrets_file, &mode)?;
                        let (_status, body) = if all_present {
                            ("completed", "All required secrets present")
                        } else {
                            ("failed", "Some required secrets missing")
                        };
                        let status_json =
                            serde_json::to_string(&status_map).unwrap_or_else(|_| "{}".to_string());
                        let event =
                            MatrixEventEnvelope::state("install.secret.validated", now, body)
                                .with_detail_field("room", &*message.room_id)
                                .with_detail_field("sender", &*message.sender)
                                .with_detail_field("mode", &*mode)
                                .with_detail_field("all_present", all_present)
                                .with_detail_field("secrets", status_json);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::KeyRotationStart => {
                        self.handle_key_rotation(&message.room_id, &message.sender, now)
                    }
                    ControlCommand::ProposalApprove { proposal_id } => self
                        .handle_proposal_approve(
                            &proposal_id,
                            &message.room_id,
                            &message.sender,
                            now,
                        ),
                    ControlCommand::ProposalDismiss { proposal_id } => {
                        self.handle_proposal_dismiss(&proposal_id, &message.room_id, now)
                    }
                    ControlCommand::ApprovalApprove { ticket_id } => {
                        let result = {
                            let mut gate = self
                                .approval_gate
                                .lock()
                                .expect("approval_gate mutex poisoned");
                            gate.approve(&ticket_id, &message.sender, now)
                        };
                        let envelope = match result {
                            Ok(()) => MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                &format!("Approved ticket {ticket_id}"),
                            )
                            .with_detail_field("ticket_id", ticket_id.as_str())
                            .with_detail_field("approved_by", message.sender.as_str()),
                            Err(e) => MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                &format!("Approval failed: {e}"),
                            )
                            .with_detail_field("ticket_id", ticket_id.as_str())
                            .with_detail_field("error", e.to_string().as_str()),
                        };
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        }])
                    }
                    ControlCommand::ApprovalDeny { ticket_id, reason } => {
                        let result = {
                            let mut gate = self
                                .approval_gate
                                .lock()
                                .expect("approval_gate mutex poisoned");
                            gate.deny(&ticket_id, &message.sender, now, reason.clone())
                        };
                        let reason_display = reason.clone().unwrap_or_else(|| "(none)".to_string());
                        let envelope = match result {
                            Ok(()) => MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                &format!("Denied ticket {ticket_id}"),
                            )
                            .with_detail_field("ticket_id", ticket_id.as_str())
                            .with_detail_field("denied_by", message.sender.as_str())
                            .with_detail_field("reason", reason_display.as_str()),
                            Err(e) => MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                &format!("Denial failed: {e}"),
                            )
                            .with_detail_field("ticket_id", ticket_id.as_str())
                            .with_detail_field("error", e.to_string().as_str()),
                        };
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        }])
                    }
                    ControlCommand::ApprovalInspect { ticket_id } => {
                        // Look up the ticket — take a read-only snapshot, then
                        // drop the lock before shelling out to git (avoid
                        // holding the mutex across any potentially-slow
                        // operation).
                        let snapshot = {
                            let gate = self
                                .approval_gate
                                .lock()
                                .expect("approval_gate mutex poisoned");
                            gate.get(&ticket_id).cloned()
                        };
                        let envelope = match snapshot {
                            Some(ticket) => {
                                let ctx = &ticket.context;
                                let output = std::process::Command::new("git")
                                    .arg("-C")
                                    .arg(&ctx.local_bare_path)
                                    .arg("diff")
                                    .arg(format!(
                                        "{}..{}",
                                        ctx.commit_range.from_sha, ctx.commit_range.to_sha
                                    ))
                                    .output();
                                let diff_body = match output {
                                    Ok(out) if out.status.success() => {
                                        let raw = String::from_utf8_lossy(&out.stdout);
                                        if raw.len() > 100_000 {
                                            format!(
                                                "{}\n\n[... truncated at 100KB ...]",
                                                &raw[..100_000]
                                            )
                                        } else {
                                            raw.to_string()
                                        }
                                    }
                                    Ok(out) => format!(
                                        "git diff failed: exit status {}, stderr: {}",
                                        out.status,
                                        String::from_utf8_lossy(&out.stderr),
                                    ),
                                    Err(e) => format!("git diff could not run: {e}"),
                                };
                                MatrixEventEnvelope::new(
                                    Kind::Message,
                                    Status::Success,
                                    now,
                                    &format!("Inspect {ticket_id}"),
                                )
                                .with_detail_field("ticket_id", ticket_id.as_str())
                                .with_detail_field("repo_id", ctx.repo_id.as_str())
                                .with_detail_field("branch", ctx.branch.as_str())
                                .with_detail_field("commit_count", ctx.commit_range.commit_count)
                                .with_detail_field("diff", diff_body.as_str())
                            }
                            None => MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                &format!("Unknown ticket {ticket_id}"),
                            )
                            .with_detail_field("ticket_id", ticket_id.as_str()),
                        };
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        }])
                    }
                    ControlCommand::CredentialSubmit { .. }
                    | ControlCommand::CredentialQuery { .. }
                    | ControlCommand::CredentialRemove { .. } => {
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Fail,
                            now,
                            "Credential commands must be sent to the credentials room",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("hint", "send credential commands to #credentials");
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                    ControlCommand::Unknown { reason } => {
                        // JSON payloads that failed command parsing should be rejected
                        // (they're structured commands with invalid fields, not natural language).
                        if message.body.trim().starts_with('{') {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "Unsupported command",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("reason", reason);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        // Natural language — classify via UxClassifier and route accordingly.
                        self.handle_classified_message(message, now)
                    }
                    _ => {
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Fail,
                            now,
                            "Command not supported in control room",
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field(
                            "hint",
                            "credential commands should be sent to the #credentials room",
                        );
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }])
                    }
                };
            }
        }

        if role == Some(RoomRole::Credentials)
            || (role.is_none() && is_credentials_room(&message.room_id))
        {
            let command = parse_control_command(&message.body);
            return match command {
                ControlCommand::IssueAuth { target, scopes } => {
                    let _job_id = self.queue_auth_issue_request(&target, scopes.clone())?;
                    let event = MatrixEventEnvelope::new(
                        Kind::Message,
                        Status::Working,
                        now,
                        "Credential request accepted",
                    )
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field("sender", &*message.sender)
                    .with_detail_field("target", target)
                    .with_detail_field("scopes", scopes.join(","));
                    Ok(vec![RoutedMatrixEnvelope {
                        room_id: message.room_id.clone(),
                        envelope: event,
                    }])
                }
                ControlCommand::CredentialSubmit {
                    service,
                    username,
                    secret,
                    totp_secret,
                } => {
                    let record = credential_gateway::CredentialRecord {
                        service: service.clone(),
                        username: username.clone(),
                        secret,
                        totp_secret,
                    };
                    match self.credential_gateway.put_credential(record) {
                        Ok(()) => {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                "Credential stored",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("service", service)
                            .with_detail_field("username", username);
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                        Err(e) => {
                            warn!("credential.submit failed: {e}");
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "Failed to store credential",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("service", service)
                            .with_detail_field("error", e.to_string());
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                    }
                }
                ControlCommand::CredentialQuery { service } => {
                    match self.credential_gateway.get_credential(&service) {
                        Ok(Some(record)) => {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                "Credential found",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("service", &*record.service)
                            .with_detail_field("username", &*record.username)
                            .with_detail_field("has_totp", record.totp_secret.is_some());
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                        Ok(None) => {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                "Credential not found",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("service", service);
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                        Err(e) => {
                            warn!("credential.query failed: {e}");
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "Failed to query credential",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("service", service)
                            .with_detail_field("error", e.to_string());
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                    }
                }
                ControlCommand::CredentialRemove { service } => {
                    match self.credential_gateway.delete_credential(&service) {
                        Ok(true) => {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                "Credential removed",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("sender", &*message.sender)
                            .with_detail_field("service", service);
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                        Ok(false) => {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Success,
                                now,
                                "Credential not found",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("service", service);
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                        Err(e) => {
                            warn!("credential.remove failed: {e}");
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "Failed to remove credential",
                            )
                            .with_detail_field("room", &*message.room_id)
                            .with_detail_field("service", service)
                            .with_detail_field("error", e.to_string());
                            Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }])
                        }
                    }
                }
                ControlCommand::ApiCredentialSubmit {
                    key,
                    value,
                    validate,
                } => self.handle_api_credential_submit(
                    &message.room_id,
                    &message.sender,
                    key,
                    value,
                    validate,
                    now,
                ),
                ControlCommand::ApiCredentialQuery { keys } => {
                    self.handle_api_credential_query(&message.room_id, &message.sender, keys, now)
                }
                ControlCommand::ApiCredentialRemove { key } => {
                    self.handle_api_credential_remove(&message.room_id, &message.sender, key, now)
                }
                ControlCommand::CredentialAuthenticate { domain } => self
                    .handle_credential_authenticate(&message.room_id, &message.sender, domain, now),
                ControlCommand::CredentialApprove {
                    request_id,
                    remember_for_secs,
                } => self.handle_credential_approve(
                    &message.room_id,
                    &message.sender,
                    request_id,
                    remember_for_secs,
                    now,
                ),
                ControlCommand::CredentialDeny { request_id, reason } => self
                    .handle_credential_deny(
                        &message.room_id,
                        &message.sender,
                        request_id,
                        reason,
                        now,
                    ),
                ControlCommand::CredentialRespond { request_id, value } => self
                    .handle_credential_respond(
                        &message.room_id,
                        &message.sender,
                        request_id,
                        value,
                        now,
                    ),
                ControlCommand::CredentialApprovalPolicyList { include_inactive } => self
                    .handle_credential_approval_policy_list(
                        &message.room_id,
                        include_inactive,
                        now,
                    ),
                ControlCommand::CredentialApprovalPolicyRevoke { policy_id } => {
                    self.handle_credential_approval_policy_revoke(&message.room_id, policy_id, now)
                }
                ControlCommand::Unknown { reason } => {
                    let event = MatrixEventEnvelope::new(
                                                Kind::Message,
                                                Status::Fail,
                                                now,
                                                "Credential command rejected",
                                            )
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field("reason", reason)
                    .with_detail_field(
                        "hint",
                        "use: credential.submit, credential.query, credential.remove, credential.authenticate, credential.approve, credential.deny, credential.approval_policy.list, credential.approval_policy.revoke, or auth issue <target> [scopes]",
                    );
                    Ok(vec![RoutedMatrixEnvelope {
                        room_id: message.room_id.clone(),
                        envelope: event,
                    }])
                }
                _ => {
                    let event = MatrixEventEnvelope::new(
                                                Kind::Message,
                                                Status::Fail,
                                                now,
                                                "Credential room only accepts credential and auth commands",
                                            )
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field(
                        "hint",
                        "use: credential.submit, credential.query, credential.remove, credential.authenticate, credential.approve, credential.deny, credential.approval_policy.list, credential.approval_policy.revoke, or auth issue <target> [scopes]",
                    );
                    Ok(vec![RoutedMatrixEnvelope {
                        room_id: message.room_id.clone(),
                        envelope: event,
                    }])
                }
            };
        }

        // --- Credential event guard ---
        // Credential commands (credential.submit, credential.query, credential.remove,
        // credential.authenticate, credential.approve, credential.deny, credential.respond)
        // must ONLY be processed by the credentials room handler.
        // If they arrive on any other room (e.g. #stream due to mis-routing or echoed
        // events), they must be intercepted here to prevent UxClassifier from treating
        // them as user-facing messages — which would create spurious threads, run
        // inquisition on API key names, and leak credential metadata into the UI.
        let is_cred_room = role == Some(RoomRole::Credentials)
            || (role.is_none() && is_credentials_room(&message.room_id));
        if !is_cred_room {
            if let Some(cmd_type) = Self::detect_credential_command(&message.body) {
                info!(
                    "credential_guard: intercepted {} on non-credentials room {} — handling silently",
                    cmd_type, message.room_id
                );
                let command = parse_control_command(&message.body);
                let cred_room = self.room_roles.resolve(RoomRole::Credentials).to_string();
                return match command {
                    ControlCommand::ApiCredentialSubmit {
                        key,
                        value,
                        validate,
                    } => self.handle_api_credential_submit(
                        &cred_room,
                        &message.sender,
                        key,
                        value,
                        validate,
                        now,
                    ),
                    ControlCommand::ApiCredentialQuery { keys } => {
                        self.handle_api_credential_query(&cred_room, &message.sender, keys, now)
                    }
                    ControlCommand::ApiCredentialRemove { key } => {
                        self.handle_api_credential_remove(&cred_room, &message.sender, key, now)
                    }
                    ControlCommand::CredentialAuthenticate { domain } => self
                        .handle_credential_authenticate(&cred_room, &message.sender, domain, now),
                    ControlCommand::CredentialApprove {
                        request_id,
                        remember_for_secs,
                    } => self.handle_credential_approve(
                        &cred_room,
                        &message.sender,
                        request_id,
                        remember_for_secs,
                        now,
                    ),
                    ControlCommand::CredentialDeny { request_id, reason } => self
                        .handle_credential_deny(
                            &cred_room,
                            &message.sender,
                            request_id,
                            reason,
                            now,
                        ),
                    ControlCommand::CredentialRespond { request_id, value } => self
                        .handle_credential_respond(
                            &cred_room,
                            &message.sender,
                            request_id,
                            value,
                            now,
                        ),
                    ControlCommand::CredentialApprovalPolicyList { include_inactive } => self
                        .handle_credential_approval_policy_list(&cred_room, include_inactive, now),
                    ControlCommand::CredentialApprovalPolicyRevoke { policy_id } => {
                        self.handle_credential_approval_policy_revoke(&cred_room, policy_id, now)
                    }
                    ControlCommand::CredentialSubmit { .. }
                    | ControlCommand::CredentialQuery { .. }
                    | ControlCommand::CredentialRemove { .. } => {
                        // Login-format credential on wrong room — silently acknowledge.
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Credential command redirected to credentials room",
                        )
                        .with_detail_field("original_room", &*message.room_id)
                        .with_detail_field("target_room", &*cred_room);
                        Ok(vec![RoutedMatrixEnvelope {
                            room_id: cred_room,
                            envelope: event,
                        }])
                    }
                    _ => {
                        // Detected as credential-like but didn't parse as a valid
                        // credential command. Silently drop to avoid UI leakage.
                        debug!(
                            "credential_guard: credential-like message on {} did not parse — dropping silently",
                            message.room_id
                        );
                        Ok(vec![])
                    }
                };
            }
        }

        // --- #stream room: classify and handle user messages ---
        // Must come before the catch-all goal commands block, which returns
        // `goal.message.received` for all non-JSON messages.
        if role == Some(RoomRole::Stream) {
            return self.handle_classified_message(message, now);
        }

        // --- Goal commands: handle goal.answer, goal.plan.approved/rejected ---
        // These can arrive on #control (via sendCommand) or #goals (via goals room).
        // We extract the command from v2 (sym.c) JSON format.
        {
            if message.body.starts_with('{') {
                // Extract command + fields from v2 (sym.c) format only
                let extracted = crate::routing::extract_command_from_json(&message.body);
                if let Some(ext) = extracted {
                    let value = serde_json::Value::Object(ext.fields.clone());
                    let command = ext.command.as_str();
                    // --- Declared task condition set ---
                    if command == "goal.task.condition.set" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");
                        let task_id = value.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let condition_kind = value
                            .get("condition_kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let condition_value = value
                            .get("condition_value")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let note = value.get("note").and_then(|v| v.as_str());

                        if goal_id.is_empty()
                            || task_id.is_empty()
                            || condition_kind.is_empty()
                            || condition_value.is_empty()
                        {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.set requires goal_id, task_id, condition_kind, and condition_value",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let archive_root = self
                            .config
                            .archive_path
                            .clone()
                            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
                        let Some(task) = crate::goal_management::load_goal_task_from_archive(
                            &archive_root,
                            goal_id,
                            task_id,
                        )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive task matches goal_id + task_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };
                        if !matches!(
                            task.task_driver,
                            symbiotic_control_plane::types::GoalTaskDriver::Declared
                        ) {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.set only applies to declared tasks",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        if !matches!(
                            condition_kind,
                            "waiting_for"
                                | "review_target"
                                | "coordination_target"
                                | "external_dependency"
                        ) {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.set condition_kind must be one of waiting_for|review_target|coordination_target|external_dependency",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        let Some((project_id, goal_title, goal_summary, goal_thread_id)) =
                            crate::goal_management::load_goal_identity_from_archive(
                                &archive_root,
                                goal_id,
                            )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive goal matches goal_id",
                            )
                            .with_detail_field("goal_id", goal_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        let context = crate::goal_management::GoalHierarchyContext {
                            project_id: &project_id,
                            goal_id,
                            title: &goal_title,
                            summary: &goal_summary,
                            owner: Some(&message.sender),
                            thread_id: goal_thread_id.as_deref(),
                            observed_at: now as i64,
                        };
                        crate::goal_management::persist_goal_task_condition_set_archive(
                            &archive_root,
                            &context,
                            &task,
                            condition_kind,
                            condition_value,
                            Some(&message.sender),
                            note,
                        )?;

                        let mut event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Task condition updated",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("condition_kind", condition_kind)
                        .with_detail_field("condition_value", condition_value);
                        if let Some(note) = note {
                            event = event.with_detail_field("note", note);
                        }

                        let thread_room = goal_thread_id
                            .as_deref()
                            .and_then(|thread_id| self.resolve_thread_room(thread_id))
                            .unwrap_or_else(|| self.resolve_room(RoomRole::Goals).to_string());
                        let update_event = MatrixEventEnvelope::state(
                            "goal.task.condition.set",
                            now,
                            "Declared task condition updated",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("condition_kind", condition_kind)
                        .with_detail_field("condition_value", condition_value)
                        .with_detail_field(
                            "detail",
                            note.unwrap_or("Declared task condition updated."),
                        );
                        let update_event = if let Some(thread_id) = goal_thread_id.as_deref() {
                            update_event.with_thread(thread_id)
                        } else {
                            update_event
                        };

                        return Ok(vec![
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                            RoutedMatrixEnvelope {
                                room_id: thread_room,
                                envelope: update_event,
                            },
                        ]);
                    }

                    // --- Declared task condition satisfied ---
                    if command == "goal.task.condition.satisfied" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");
                        let task_id = value.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let condition_kind = value
                            .get("condition_kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let note = value.get("note").and_then(|v| v.as_str());

                        if goal_id.is_empty() || task_id.is_empty() || condition_kind.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.satisfied requires goal_id, task_id, and condition_kind",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let archive_root = self
                            .config
                            .archive_path
                            .clone()
                            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
                        let Some(task) = crate::goal_management::load_goal_task_from_archive(
                            &archive_root,
                            goal_id,
                            task_id,
                        )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive task matches goal_id + task_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };
                        if !matches!(
                            task.task_driver,
                            symbiotic_control_plane::types::GoalTaskDriver::Declared
                        ) {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.satisfied only applies to declared tasks",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        let condition_value = match condition_kind {
                            "waiting_for" => task.declared_context.waiting_for.as_deref(),
                            "review_target" => task.declared_context.review_target.as_deref(),
                            "coordination_target" => {
                                task.declared_context.coordination_target.as_deref()
                            }
                            "external_dependency" => {
                                task.declared_context.external_dependency.as_deref()
                            }
                            _ => None,
                        };
                        if condition_value.is_none() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.satisfied requires a matching declared condition on the task",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id)
                            .with_detail_field("condition_kind", condition_kind);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                        let Some((project_id, goal_title, goal_summary, goal_thread_id)) =
                            crate::goal_management::load_goal_identity_from_archive(
                                &archive_root,
                                goal_id,
                            )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive goal matches goal_id",
                            )
                            .with_detail_field("goal_id", goal_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        let context = crate::goal_management::GoalHierarchyContext {
                            project_id: &project_id,
                            goal_id,
                            title: &goal_title,
                            summary: &goal_summary,
                            owner: Some(&message.sender),
                            thread_id: goal_thread_id.as_deref(),
                            observed_at: now as i64,
                        };
                        let Some(condition_value) =
                            crate::goal_management::persist_goal_task_condition_cleared_archive(
                                &archive_root,
                                &context,
                                &task,
                                condition_kind,
                                Some(&message.sender),
                                note,
                            )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.condition.satisfied could not clear the requested condition",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id)
                            .with_detail_field("condition_kind", condition_kind);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        let resumed = if matches!(
                            task.execution_status,
                            symbiotic_control_plane::types::GoalTaskStatus::Blocked
                        ) && only_declared_condition_matches(&task, condition_kind)
                        {
                            let goal_events =
                                crate::goal_management::load_goal_events_from_archive(
                                    &archive_root,
                                    goal_id,
                                )?;
                            let resume_status =
                                crate::goal_management::infer_declared_task_resume_status(
                                    &goal_events,
                                    &task.task_id,
                                );
                            crate::goal_management::sync_goal_task_declared_status(
                                &self.management_store,
                                Some(&archive_root),
                                &context,
                                &task,
                                resume_status,
                                Some(&message.sender),
                                Some(
                                    "Declared external dependency cleared; task resumed to its last runnable status.",
                                ),
                            );
                            true
                        } else {
                            false
                        };

                        let mut event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Task condition marked satisfied",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("condition_kind", condition_kind);
                        if resumed {
                            event = event.with_detail_field("resumed", true);
                        }
                        if let Some(note) = note {
                            event = event.with_detail_field("note", note);
                        }

                        let thread_room = goal_thread_id
                            .as_deref()
                            .and_then(|thread_id| self.resolve_thread_room(thread_id))
                            .unwrap_or_else(|| self.resolve_room(RoomRole::Goals).to_string());
                        let wake_event = MatrixEventEnvelope::state(
                            "goal.task.condition.satisfied",
                            now,
                            "Declared task condition satisfied",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("condition_kind", condition_kind)
                        .with_detail_field("condition_value", condition_value.as_str())
                        .with_detail_field(
                            "detail",
                            note.unwrap_or("Declared task condition marked satisfied."),
                        );
                        let wake_event = if let Some(thread_id) = goal_thread_id.as_deref() {
                            wake_event.with_thread(thread_id)
                        } else {
                            wake_event
                        };

                        return Ok(vec![
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                            RoutedMatrixEnvelope {
                                room_id: thread_room,
                                envelope: wake_event,
                            },
                        ]);
                    }

                    // --- Task transition ---
                    if command == "goal.task.transition" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");
                        let task_id = value.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let next_status_raw = value
                            .get("execution_status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let note = value.get("note").and_then(|v| v.as_str());

                        if goal_id.is_empty() || task_id.is_empty() || next_status_raw.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.transition requires goal_id, task_id, and execution_status",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let next_status = match next_status_raw {
                            "planned" => symbiotic_control_plane::types::GoalTaskStatus::Planned,
                            "in_progress" => {
                                symbiotic_control_plane::types::GoalTaskStatus::InProgress
                            }
                            "blocked" => symbiotic_control_plane::types::GoalTaskStatus::Blocked,
                            "done" => symbiotic_control_plane::types::GoalTaskStatus::Done,
                            "cancelled" => {
                                symbiotic_control_plane::types::GoalTaskStatus::Cancelled
                            }
                            _ => {
                                let event = MatrixEventEnvelope::new(
                                    Kind::Message,
                                    Status::Fail,
                                    now,
                                    "goal.task.transition execution_status must be one of planned|in_progress|blocked|done|cancelled",
                                )
                                .with_detail_field("goal_id", goal_id)
                                .with_detail_field("task_id", task_id);
                                return Ok(vec![RoutedMatrixEnvelope {
                                    room_id: message.room_id.clone(),
                                    envelope: event,
                                }]);
                            }
                        };

                        let archive_root = self
                            .config
                            .archive_path
                            .clone()
                            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
                        let Some(task) = crate::goal_management::load_goal_task_from_archive(
                            &archive_root,
                            goal_id,
                            task_id,
                        )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive task matches goal_id + task_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };
                        let Some((project_id, goal_title, goal_summary, goal_thread_id)) =
                            crate::goal_management::load_goal_identity_from_archive(
                                &archive_root,
                                goal_id,
                            )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive goal matches goal_id",
                            )
                            .with_detail_field("goal_id", goal_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        let context = crate::goal_management::GoalHierarchyContext {
                            project_id: &project_id,
                            goal_id,
                            title: &goal_title,
                            summary: &goal_summary,
                            owner: Some(&message.sender),
                            thread_id: goal_thread_id.as_deref(),
                            observed_at: now as i64,
                        };
                        let escalation = crate::goal_management::sync_goal_task_declared_status(
                            &self.management_store,
                            Some(&archive_root),
                            &context,
                            &task,
                            next_status,
                            Some(&message.sender),
                            note,
                        );

                        let mut event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Task transition applied",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("execution_status", next_status_raw);
                        if let Some(note) = note {
                            event = event.with_detail_field("note", note);
                        }
                        let mut routed = vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }];
                        if let Some(escalation) = escalation {
                            let thread_room = goal_thread_id
                                .as_deref()
                                .and_then(|thread_id| self.resolve_thread_room(thread_id))
                                .unwrap_or_else(|| message.room_id.clone());
                            let operator_notice = MatrixEventEnvelope::new(
                                Kind::Question,
                                Status::Awaiting,
                                now,
                                &format!(
                                    "Task blocked: {}. {}",
                                    escalation.task_title, escalation.detail
                                ),
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id)
                            .with_detail_field(
                                "escalation_policy",
                                match escalation.policy {
                                    symbiotic_control_plane::types::GoalTaskEscalationPolicy::NotifyOperator => {
                                        "notify_operator"
                                    }
                                    symbiotic_control_plane::types::GoalTaskEscalationPolicy::RaiseAlert => {
                                        "raise_alert"
                                    }
                                    symbiotic_control_plane::types::GoalTaskEscalationPolicy::AutoReplan => {
                                        "auto_replan"
                                    }
                                },
                            )
                            .with_detail_field(
                                "escalation_audience",
                                crate::goal_management::escalation_audience_label(
                                    escalation.audience.as_deref(),
                                    escalation.policy,
                                ),
                            )
                            .with_detail_field("escalation_trigger", "on_enter_blocked")
                            .with_detail_field(
                                "escalation_severity",
                                escalation
                                    .severity
                                    .map(crate::goal_management::escalation_severity_label)
                                    .unwrap_or(
                                        crate::goal_management::escalation_severity_label(
                                            symbiotic_control_plane::types::GoalTaskEscalationSeverity::Normal,
                                        ),
                                    ),
                            )
                            .with_detail_field("detail", escalation.detail.as_str());
                            let operator_notice = if let Some(thread_id) = goal_thread_id.as_deref()
                            {
                                operator_notice.with_thread(thread_id)
                            } else {
                                operator_notice
                            };
                            routed.push(RoutedMatrixEnvelope {
                                room_id: thread_room,
                                envelope: operator_notice,
                            });

                            let escalation_audience =
                                crate::goal_management::escalation_audience_label(
                                    escalation.audience.as_deref(),
                                    escalation.policy,
                                );
                            let escalation_severity = escalation.severity.unwrap_or(
                                crate::goal_management::default_escalation_severity(
                                    escalation.policy,
                                ),
                            );

                            if crate::goal_management::escalation_targets_alerts(
                                escalation.policy,
                                escalation_audience,
                                escalation_severity,
                            ) {
                                routed.push(RoutedMatrixEnvelope {
                                    room_id: self.resolve_room(RoomRole::Alerts).to_string(),
                                    envelope: MatrixEventEnvelope::state(
                                        "goal.task.escalated",
                                        now,
                                        "Declared task escalated to alerts",
                                    )
                                    .with_detail_field("goal_id", goal_id)
                                    .with_detail_field("task_id", task_id)
                                    .with_detail_field("detail", escalation.detail.as_str())
                                    .with_detail_field(
                                        "escalation_policy",
                                        match escalation.policy {
                                            symbiotic_control_plane::types::GoalTaskEscalationPolicy::NotifyOperator => {
                                                "notify_operator"
                                            }
                                            symbiotic_control_plane::types::GoalTaskEscalationPolicy::RaiseAlert => {
                                                "raise_alert"
                                            }
                                            symbiotic_control_plane::types::GoalTaskEscalationPolicy::AutoReplan => {
                                                "auto_replan"
                                            }
                                        },
                                    )
                                    .with_detail_field("escalation_audience", escalation_audience)
                                    .with_detail_field(
                                        "escalation_trigger",
                                        "on_enter_blocked",
                                    )
                                    .with_detail_field(
                                        "escalation_severity",
                                        crate::goal_management::escalation_severity_label(
                                            escalation_severity,
                                        ),
                                    ),
                                });
                            }

                            if matches!(
                                escalation.policy,
                                symbiotic_control_plane::types::GoalTaskEscalationPolicy::AutoReplan
                            ) {
                                routed.push(RoutedMatrixEnvelope {
                                    room_id: message.room_id.clone(),
                                    envelope: MatrixEventEnvelope::state(
                                        "goal.task.replan.requested",
                                        now,
                                        "Blocked task requested replanning",
                                    )
                                    .with_detail_field("goal_id", goal_id)
                                    .with_detail_field("task_id", task_id)
                                    .with_detail_field("detail", escalation.detail.as_str())
                                    .with_detail_field("escalation_policy", "auto_replan")
                                    .with_detail_field("escalation_trigger", "on_enter_blocked")
                                    .with_detail_field("escalation_audience", escalation_audience)
                                    .with_detail_field(
                                        "escalation_severity",
                                        crate::goal_management::escalation_severity_label(
                                            escalation_severity,
                                        ),
                                    ),
                                });
                            }
                        }
                        return Ok(routed);
                    }

                    if command == "goal.task.assign" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");
                        let task_id = value.get("task_id").and_then(|v| v.as_str()).unwrap_or("");
                        let owner_hint = value
                            .get("owner_hint")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let note = value.get("note").and_then(|v| v.as_str());

                        if goal_id.is_empty() || task_id.is_empty() || owner_hint.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.task.assign requires goal_id, task_id, and owner_hint",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let archive_root = self
                            .config
                            .archive_path
                            .clone()
                            .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
                        let Some(task) = crate::goal_management::load_goal_task_from_archive(
                            &archive_root,
                            goal_id,
                            task_id,
                        )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive task matches goal_id + task_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("task_id", task_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };
                        let Some((project_id, goal_title, goal_summary, goal_thread_id)) =
                            crate::goal_management::load_goal_identity_from_archive(
                                &archive_root,
                                goal_id,
                            )?
                        else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No archive goal matches goal_id",
                            )
                            .with_detail_field("goal_id", goal_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        };

                        let context = crate::goal_management::GoalHierarchyContext {
                            project_id: &project_id,
                            goal_id,
                            title: &goal_title,
                            summary: &goal_summary,
                            owner: Some(&message.sender),
                            thread_id: goal_thread_id.as_deref(),
                            observed_at: now as i64,
                        };
                        crate::goal_management::sync_goal_task_owner(
                            &self.management_store,
                            Some(&archive_root),
                            &context,
                            &task,
                            owner_hint,
                            Some(&message.sender),
                            note,
                        );

                        let mut event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            "Task assignment applied",
                        )
                        .with_detail_field("goal_id", goal_id)
                        .with_detail_field("task_id", task_id)
                        .with_detail_field("owner_hint", owner_hint);
                        if let Some(note) = note {
                            event = event.with_detail_field("note", note);
                        }
                        return Ok(vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }]);
                    }

                    // --- Plan approval / rejection ---
                    if command == "goal.plan.approved" || command == "goal.plan.rejected" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");

                        if goal_id.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.plan.approved/rejected requires non-empty goal_id",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let goal_states = self.list_goal_states().unwrap_or_default();
                        let matching_goal = goal_states.iter().find(|gs| {
                            (gs.pipeline_stage.as_deref() == Some("awaiting_approval")
                                || gs.status == "awaiting_approval")
                                && gs.last_run_id.as_deref() == Some(goal_id)
                        });

                        if let Some(goal) = matching_goal {
                            if command == "goal.plan.approved" {
                                // Build a dynamic multi-role workflow from the
                                // ProposedPlan. Each plan step becomes an
                                // agent.execute workflow step with its role.
                                let resume_room = goal.goal_room.clone();
                                let resume_owner =
                                    goal.owner.clone().unwrap_or_else(|| message.sender.clone());
                                let questionnaire_context =
                                    crate::goals::questionnaire_context_from_qa(
                                        crate::goals::read_goal_qa(
                                            &self.config.data_dir,
                                            &resume_room,
                                            &goal.template,
                                        )
                                        .as_deref(),
                                    );

                                let original_goal = crate::goals::read_goal_text(
                                    &self.config.data_dir,
                                    &resume_room,
                                    &goal.template,
                                );

                                // Read the stored plan JSON from the goal log
                                let plan_json = crate::goals::read_pending_plan_json(
                                    &self.config.data_dir,
                                    &resume_room,
                                    &goal.template,
                                );

                                // Reuse the original template name so that
                                // goal state tracking and completion events
                                // consistently reference the same template.
                                // The plan JSON (in user_answer) drives
                                // multi-role workflow generation via
                                // resolve_workflow_template_with_plan.
                                let exec_template = goal.template.clone();

                                let payload = crate::goals::WorkflowRunPayload {
                                    template: exec_template.clone(),
                                    goal_room: Some(resume_room.clone()),
                                    goal_sender: Some(resume_owner.clone()),
                                    project_id: Some(goal.project_id.clone()),
                                    user_answer: plan_json,
                                    goal_id: Some(goal_id.to_string()),
                                    user_goal: original_goal,
                                    replan_context: None,
                                };

                                let (job_id, _) = self.queue_workflow_run_with_payload(&payload)?;

                                if let Some(plan_json) = payload.user_answer.as_deref() {
                                    if let Some(tasks) =
                                        crate::goals::planned_execution_tasks_from_plan_json(
                                            &exec_template,
                                            plan_json,
                                        )
                                    {
                                        let tasks: Vec<_> = tasks
                                            .into_iter()
                                            .map(|mut task| {
                                                task.questionnaire_context =
                                                    questionnaire_context.clone();
                                                task
                                            })
                                            .collect();
                                        let goal_scope =
                                            crate::goal_management::stable_goal_scope_id(
                                                &exec_template,
                                                goal_id,
                                            );
                                        let goal_title = payload
                                            .user_goal
                                            .as_deref()
                                            .filter(|value| !value.trim().is_empty())
                                            .unwrap_or(&exec_template)
                                            .to_string();
                                        let goal_summary = format!(
                                            "Approved execution plan for goal '{}'.",
                                            goal_title
                                        );
                                        let context =
                                            crate::goal_management::GoalHierarchyContext {
                                                project_id: &goal.project_id,
                                                goal_id: &goal_scope,
                                                title: &goal_title,
                                                summary: &goal_summary,
                                                owner: Some(&resume_owner),
                                                thread_id: goal.thread_id.as_deref(),
                                                observed_at: now as i64,
                                            };
                                        let archive_root =
                                            self.config.archive_path.clone().unwrap_or_else(|| {
                                                self.config.data_dir.join("../knowledge-base")
                                            });
                                        if let Err(error) =
                                            crate::goal_management::persist_goal_plan_archive(
                                                &archive_root,
                                                &context,
                                                "implementation",
                                                &tasks,
                                            )
                                        {
                                            tracing::warn!(
                                                goal_id = %goal_scope,
                                                %error,
                                                "goal_management: failed to persist approved goal plan to archive"
                                            );
                                        }
                                        let archive_tasks = crate::goal_management::
                                            load_active_goal_tasks_from_archive(
                                                &archive_root,
                                                &goal_scope,
                                            );
                                        let hierarchy_tasks = if archive_tasks.is_empty() {
                                            &tasks
                                        } else {
                                            &archive_tasks
                                        };
                                        crate::goal_management::sync_goal_plan_hierarchy(
                                            &self.management_store,
                                            &context,
                                            hierarchy_tasks,
                                            symbiotic_control_plane::WorkItemStatus::Running,
                                        );
                                    }
                                }

                                upsert_goal_state(
                                    &self.config.goal_state_file,
                                    GoalState {
                                        goal_room: resume_room.clone(),
                                        thread_id: None,
                                        project_id: goal.project_id.clone(),
                                        template: exec_template.clone(),
                                        status: "running".to_string(),
                                        last_job_id: job_id.clone(),
                                        last_run_id: Some(goal_id.to_string()),
                                        owner: Some(resume_owner),
                                        updated_at: now,
                                        complexity: None,
                                        pipeline_stage: Some("executing".to_string()),
                                        audit_id: None,
                                        plan_id: goal.plan_id.clone(),
                                    },
                                )?;

                                let event = MatrixEventEnvelope::new(
                                    Kind::Message,
                                    Status::Working,
                                    now,
                                    "Plan approved, starting multi-role execution",
                                )
                                .with_detail_field("goal_id", goal_id)
                                .with_detail_field("template", &*exec_template)
                                .with_detail_field("room", &*resume_room);

                                return Ok(vec![RoutedMatrixEnvelope {
                                    room_id: message.room_id.clone(),
                                    envelope: event,
                                }]);
                            } else {
                                // Rejected — cancel the goal.
                                let reason = value
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("User rejected proposed plan");

                                upsert_goal_state(
                                    &self.config.goal_state_file,
                                    GoalState {
                                        goal_room: goal.goal_room.clone(),
                                        thread_id: None,
                                        project_id: goal.project_id.clone(),
                                        template: goal.template.clone(),
                                        status: "rejected".to_string(),
                                        last_job_id: goal.last_job_id.clone(),
                                        last_run_id: Some(goal_id.to_string()),
                                        owner: goal.owner.clone(),
                                        updated_at: now,
                                        complexity: None,
                                        pipeline_stage: Some("rejected".to_string()),
                                        audit_id: None,
                                        plan_id: goal.plan_id.clone(),
                                    },
                                )?;

                                let event = MatrixEventEnvelope::new(
                                    Kind::Message,
                                    Status::Success,
                                    now,
                                    reason,
                                )
                                .with_detail_field("goal_id", goal_id)
                                .with_detail_field("reason", reason);

                                return Ok(vec![RoutedMatrixEnvelope {
                                    room_id: message.room_id.clone(),
                                    envelope: event,
                                }]);
                            }
                        } else {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No goal awaiting approval matches this goal_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                    }

                    if command == "goal.message" || command == "goal.answer" {
                        let goal_id = value.get("goal_id").and_then(|v| v.as_str()).unwrap_or("");
                        let template = value.get("template").and_then(|v| v.as_str()).unwrap_or("");
                        let user_message =
                            value.get("message").and_then(|v| v.as_str()).unwrap_or("");

                        // --- T130 §04a — QuestionResolver routing ---
                        // Grouped-inquisitor answers carry `group_id` +
                        // `question_index`. When both are present we forward
                        // the submission through the resolver and emit the
                        // resulting `goal.unblocked` /
                        // `goal.question_group.expired` event (or nothing if
                        // more answers are still pending). A missing
                        // `group_id` means the app is using the legacy
                        // single-question flow; fall through to the existing
                        // handler below.
                        if command == "goal.answer" {
                            let group_id_opt = value
                                .get("group_id")
                                .and_then(|v| v.as_str())
                                .map(str::to_string);
                            let question_index_opt =
                                value.get("question_index").and_then(|v| v.as_u64());

                            if let (Some(group_id), Some(question_index)) =
                                (group_id_opt.as_deref(), question_index_opt)
                            {
                                let answer_opt = value
                                    .get("skipped")
                                    .and_then(|v| v.as_bool())
                                    .and_then(|skipped| if skipped { Some(None) } else { None })
                                    .unwrap_or_else(|| Some(user_message.to_string()));

                                let submission =
                                    crate::goal_pipeline::question_resolver::QuestionAnswer {
                                        group_id: group_id.to_string(),
                                        question_index: question_index as usize,
                                        answer: answer_opt,
                                    };

                                let now_iso = chrono::DateTime::<chrono::Utc>::from(
                                    std::time::SystemTime::UNIX_EPOCH
                                        + std::time::Duration::from_secs(now),
                                )
                                .format("%Y-%m-%dT%H:%M:%SZ")
                                .to_string();

                                let outcome = {
                                    let mut guard = self.question_resolver.lock().map_err(|e| {
                                        anyhow::anyhow!("resolver lock poisoned: {e}")
                                    })?;
                                    guard.submit_answer(submission, &now_iso)
                                };

                                match outcome {
                                    Ok(Some(
                                        crate::goal_pipeline::question_resolver::GroupResolutionOutcome::Unblocked {
                                            group_id: out_group,
                                            parent_goal_id,
                                            answers,
                                            auto_records,
                                        },
                                    )) => {
                                        let detail_json = serde_json::json!({
                                            "group_id": out_group,
                                            "parent_goal_id": parent_goal_id,
                                            "answers": answers,
                                            "auto_records": auto_records,
                                        })
                                        .to_string();
                                        let event = MatrixEventEnvelope::state(
                                            "goal.unblocked",
                                            now,
                                            "Goal unblocked",
                                        )
                                        .with_detail_field("goal_id", goal_id)
                                        .with_detail_field("group_id", &*out_group)
                                        .with_detail_field(
                                            "unblock",
                                            serde_json::from_str::<serde_json::Value>(
                                                &detail_json,
                                            )
                                            .unwrap_or(serde_json::Value::Null),
                                        )
                                        .with_thread(goal_id);

                                        // T130 §05 — Hand off to the
                                        // Sub-Goal Dispatcher. The
                                        // dispatcher's async routing runs on
                                        // a background tokio task; the
                                        // events it emits are forwarded via
                                        // `matrix_outbound_tx`. No-op when
                                        // no dispatcher is installed.
                                        if let Some(dispatcher) =
                                            self.subgoal_dispatcher()
                                        {
                                            let ctx =
                                                crate::subgoal::UnblockedContext {
                                                    parent_goal_id: parent_goal_id
                                                        .clone(),
                                                    group_id: out_group.clone(),
                                                    thread_id: Some(
                                                        goal_id.to_string(),
                                                    ),
                                                    room_id: message
                                                        .room_id
                                                        .to_string(),
                                                    now,
                                                    answers: answers.clone(),
                                                };
                                            let outbound =
                                                self.matrix_outbound_tx.clone();
                                            tokio::spawn(async move {
                                                match dispatcher
                                                    .on_unblocked(ctx)
                                                    .await
                                                {
                                                    Ok(outcome) => {
                                                        for ev in outcome.events
                                                        {
                                                            let _ = outbound
                                                                .send((
                                                                    ev.room_id,
                                                                    ev.envelope,
                                                                ));
                                                        }
                                                    }
                                                    Err(err) => {
                                                        tracing::warn!(
                                                            error = %err,
                                                            "subgoal dispatcher routing failed"
                                                        );
                                                    }
                                                }
                                            });
                                        }

                                        return Ok(vec![RoutedMatrixEnvelope {
                                            room_id: message.room_id.clone(),
                                            envelope: event,
                                        }]);
                                    }
                                    Ok(Some(
                                        crate::goal_pipeline::question_resolver::GroupResolutionOutcome::Expired {
                                            group_id: out_group,
                                            parent_goal_id,
                                            unresolved_indexes,
                                            auto_records,
                                        },
                                    )) => {
                                        let detail_json = serde_json::json!({
                                            "group_id": out_group,
                                            "parent_goal_id": parent_goal_id,
                                            "unresolved_indexes": unresolved_indexes,
                                            "auto_records": auto_records,
                                        })
                                        .to_string();
                                        let event = MatrixEventEnvelope::state(
                                            "goal.question_group.expired",
                                            now,
                                            "Question group expired without resolution",
                                        )
                                        .with_detail_field("goal_id", goal_id)
                                        .with_detail_field("group_id", &*out_group)
                                        .with_detail_field(
                                            "expired",
                                            serde_json::from_str::<serde_json::Value>(
                                                &detail_json,
                                            )
                                            .unwrap_or(serde_json::Value::Null),
                                        )
                                        .with_thread(goal_id);
                                        return Ok(vec![RoutedMatrixEnvelope {
                                            room_id: message.room_id.clone(),
                                            envelope: event,
                                        }]);
                                    }
                                    Ok(None) => {
                                        // Still waiting for more answers — ack
                                        // the submission but emit no unblock
                                        // event yet.
                                        let event = MatrixEventEnvelope::new(
                                            Kind::Message,
                                            Status::Accepted,
                                            now,
                                            "Answer accepted",
                                        )
                                        .with_detail_field("goal_id", goal_id)
                                        .with_detail_field("group_id", group_id)
                                        .with_detail_field(
                                            "question_index",
                                            question_index as i64,
                                        );
                                        return Ok(vec![RoutedMatrixEnvelope {
                                            room_id: message.room_id.clone(),
                                            envelope: event,
                                        }]);
                                    }
                                    Err(err) => {
                                        let event = MatrixEventEnvelope::new(
                                            Kind::Message,
                                            Status::Fail,
                                            now,
                                            &format!("goal.answer rejected: {err}"),
                                        )
                                        .with_detail_field("goal_id", goal_id)
                                        .with_detail_field("group_id", group_id)
                                        .with_detail_field(
                                            "question_index",
                                            question_index as i64,
                                        );
                                        return Ok(vec![RoutedMatrixEnvelope {
                                            room_id: message.room_id.clone(),
                                            envelope: event,
                                        }]);
                                    }
                                }
                            }
                        }

                        if goal_id.is_empty() || user_message.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "goal.message requires non-empty goal_id and message",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        // Find the goal state matching this goal_id or template in awaiting_input state
                        let goal_states = self.list_goal_states().unwrap_or_default();
                        debug!(
                            "goal.answer: searching for goal_id={} template={} among {} goal states",
                            goal_id, template, goal_states.len()
                        );
                        for gs in &goal_states {
                            debug!(
                                "  goal_state: room={} template={} status={} pipeline_stage={:?} last_run_id={:?}",
                                gs.goal_room, gs.template, gs.status,
                                gs.pipeline_stage, gs.last_run_id
                            );
                        }
                        let matching_goal = goal_states.iter().find(|gs| {
                            (gs.pipeline_stage.as_deref() == Some("awaiting_input")
                                || gs.status == "awaiting_input")
                                && (gs.last_run_id.as_deref() == Some(goal_id)
                                    || (!template.is_empty()
                                        && gs.template.eq_ignore_ascii_case(template)))
                        });
                        debug!("goal.answer: match_found={}", matching_goal.is_some());

                        if let Some(goal) = matching_goal {
                            // Re-queue the workflow with the user's answer.
                            let resume_template = goal.template.clone();
                            let resume_room = goal.goal_room.clone();
                            let resume_owner =
                                goal.owner.clone().unwrap_or_else(|| message.sender.clone());

                            // Pair the pending question with this answer for Q&A history.
                            if let Some(prev_question) = crate::goals::take_pending_question(
                                &self.config.data_dir,
                                &resume_room,
                                &resume_template,
                            ) {
                                crate::goals::append_goal_qa(
                                    &self.config.data_dir,
                                    &resume_room,
                                    &resume_template,
                                    &prev_question,
                                    user_message,
                                );
                            }

                            let original_goal = crate::goals::read_goal_text(
                                &self.config.data_dir,
                                &resume_room,
                                &resume_template,
                            );
                            let payload = crate::goals::WorkflowRunPayload {
                                template: resume_template.clone(),
                                goal_room: Some(resume_room.clone()),
                                goal_sender: Some(resume_owner.clone()),
                                project_id: Some(goal.project_id.clone()),
                                user_answer: Some(user_message.to_string()),
                                goal_id: Some(goal_id.to_string()),
                                user_goal: original_goal,
                                replan_context: None,
                            };

                            let (job_id, _) = self.queue_workflow_run_with_payload(&payload)?;

                            // Update goal state back to running
                            upsert_goal_state(
                                &self.config.goal_state_file,
                                GoalState {
                                    goal_room: resume_room.clone(),
                                    thread_id: None,
                                    project_id: goal.project_id.clone(),
                                    template: resume_template.clone(),
                                    status: "running".to_string(),
                                    last_job_id: job_id.clone(),
                                    last_run_id: Some(goal_id.to_string()),
                                    owner: Some(resume_owner),
                                    updated_at: now,
                                    complexity: None,
                                    pipeline_stage: Some("executing".to_string()),
                                    audit_id: None,
                                    plan_id: None,
                                },
                            )?;

                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Working,
                                now,
                                "User response received, resuming goal",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("template", &*resume_template)
                            .with_detail_field("room", &*resume_room);

                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        } else {
                            // No matching goal in awaiting_input state
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "No goal awaiting input matches this goal_id",
                            )
                            .with_detail_field("goal_id", goal_id)
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }
                    }

                    // --- Thread promotion approval / dismissal ---
                    if command == "thread.promotion.approve"
                        || command == "thread.promotion.dismiss"
                    {
                        let thread_id = value
                            .get("thread_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        if thread_id.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "thread.promotion.approve/dismiss requires non-empty thread_id",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let approved = command == "thread.promotion.approve";
                        let suggested_title = value
                            .get("suggested_title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Untitled Goal");
                        let suggested_template = value
                            .get("suggested_template")
                            .and_then(|v| v.as_str())
                            .unwrap_or("general");

                        let (action, response_event) = handle_promotion_command(
                            thread_id,
                            approved,
                            suggested_title,
                            suggested_template,
                        );

                        // Record cooldown when user dismisses a promotion.
                        if !approved {
                            if let Ok(mut tracker) = self.promotion_cooldown_tracker.lock() {
                                tracker.record_dismissal(thread_id, now);
                                log::debug!(
                                    "auto_promotion: recorded cooldown for thread={}",
                                    thread_id,
                                );
                            }
                        }

                        // Build the response envelope from the DaemonEvent
                        let mut envelopes = Vec::new();

                        if let Some(ref evt) = response_event {
                            let envelope = evt.to_envelope(now);
                            envelopes.push(RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope,
                            });
                        }

                        // If approved, create a goal via queue_workflow_run_for_goal
                        if let Some(promo_action) = action {
                            let (job_id, goal_id) = self.queue_workflow_run_for_goal(
                                &promo_action.template,
                                &message.room_id,
                                &message.sender,
                            )?;

                            append_goal_log(
                                &self.config.goal_log_file,
                                GoalLogEntry {
                                    ts: now,
                                    event: "goal.started",
                                    workflow_job_id: &job_id,
                                    goal_room: Some(&message.room_id),
                                    goal_sender: Some(&message.sender),
                                    template: &promo_action.template,
                                    detail: "promoted from thread",
                                },
                            )?;

                            upsert_goal_state(
                                &self.config.goal_state_file,
                                GoalState {
                                    goal_room: message.room_id.clone(),
                                    thread_id: Some(thread_id.to_string()),
                                    project_id: self.resolve_project_id_for_goal_materialization(
                                        None,
                                        Some(thread_id),
                                    )?,
                                    template: promo_action.template.clone(),
                                    status: "queued".to_string(),
                                    last_job_id: job_id.clone(),
                                    last_run_id: Some(goal_id.clone()),
                                    owner: Some(message.sender.clone()),
                                    updated_at: now,
                                    complexity: None,
                                    pipeline_stage: None,
                                    audit_id: None,
                                    plan_id: None,
                                },
                            )?;

                            let goal_event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Working,
                                now,
                                "Goal created from thread promotion",
                            )
                            .with_detail_field("goal_id", &*goal_id)
                            .with_detail_field("template", &*promo_action.template)
                            .with_detail_field("thread_id", thread_id)
                            .with_detail_field("title", suggested_title)
                            .with_detail_field("room", &*message.room_id)
                            .with_thread(&goal_id);

                            envelopes.push(RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: goal_event,
                            });
                        }

                        return Ok(envelopes);
                    }

                    // --- Vault edit: direct Markdown edits (T108 Phase 6, Pattern A) ---
                    if command == "vault.edit" {
                        let entity_id = value
                            .get("entity_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let operation = value
                            .get("operation")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");

                        if entity_id.is_empty() || operation.is_empty() {
                            let event = MatrixEventEnvelope::new(
                                Kind::Message,
                                Status::Fail,
                                now,
                                "vault.edit requires entity_id and operation",
                            )
                            .with_detail_field("room", &*message.room_id);
                            return Ok(vec![RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            }]);
                        }

                        let result: Result<(String, Vec<VaultMutation>), String> = match operation {
                            "add_fact" => {
                                let fact_text = value
                                    .get("fact_text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let source = value
                                    .get("source")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user");
                                let metadata = NewFactMetadata {
                                    source: source.to_string(),
                                    fact_type: None,
                                    confidence: None,
                                };
                                self.vault_writer
                                    .add_fact(entity_id, fact_text, &metadata)
                                    .map(|m| {
                                        (
                                            format!("Added fact to {}: {}", m.entity_id, fact_text),
                                            vec![m],
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            "archive_fact" => {
                                let fact_text = value
                                    .get("fact_text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let reason = value
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user requested");
                                self.vault_writer
                                    .archive_fact(entity_id, fact_text, reason)
                                    .map(|m| {
                                        (
                                            format!(
                                                "Archived fact in {}: {}",
                                                m.entity_id, fact_text
                                            ),
                                            vec![m],
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            "replace_fact" => {
                                let old_fact_text = value
                                    .get("old_fact_text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let new_fact_text = value
                                    .get("new_fact_text")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let reason = value
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user edited");
                                let source = value
                                    .get("source")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user");
                                let metadata = NewFactMetadata {
                                    source: source.to_string(),
                                    fact_type: None,
                                    confidence: None,
                                };
                                self.vault_writer
                                    .replace_fact(
                                        entity_id,
                                        old_fact_text,
                                        new_fact_text,
                                        reason,
                                        &metadata,
                                    )
                                    .map(|mutations| {
                                        (
                                            format!(
                                                "Replaced fact in {entity_id}: {old_fact_text} -> {new_fact_text}"
                                            ),
                                            mutations,
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            "add_relationship" => {
                                let rel_type =
                                    value.get("rel_type").and_then(|v| v.as_str()).unwrap_or("");
                                let target =
                                    value.get("target").and_then(|v| v.as_str()).unwrap_or("");
                                let since = value.get("since").and_then(|v| v.as_str());
                                self.vault_writer
                                    .add_relationship(
                                        entity_id, rel_type, target, since,
                                    )
                                    .map(|m| {
                                        (
                                            format!(
                                                "Added relationship {entity_id} -> {target} ({rel_type})"
                                            ),
                                            vec![m],
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            "remove_relationship" => {
                                let rel_type =
                                    value.get("rel_type").and_then(|v| v.as_str()).unwrap_or("");
                                let target =
                                    value.get("target").and_then(|v| v.as_str()).unwrap_or("");
                                let reason = value
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user requested");
                                self.vault_writer
                                    .remove_relationship(entity_id, rel_type, target, reason)
                                    .map(|m| {
                                        (
                                            format!(
                                                "Removed relationship {entity_id} -> {target} ({rel_type})"
                                            ),
                                            vec![m],
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            "replace_relationship" => {
                                let rel_type =
                                    value.get("rel_type").and_then(|v| v.as_str()).unwrap_or("");
                                let old_target = value
                                    .get("old_target")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let new_target = value
                                    .get("new_target")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let reason = value
                                    .get("reason")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("user edited");
                                let since = value.get("since").and_then(|v| v.as_str());
                                self.vault_writer
                                    .replace_relationship(
                                        entity_id,
                                        rel_type,
                                        old_target,
                                        new_target,
                                        reason,
                                        since,
                                    )
                                    .map(|m| {
                                        (
                                            format!(
                                                "Replaced relationship {entity_id}: {rel_type} {old_target} -> {new_target}"
                                            ),
                                            vec![m],
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            _ => Err(format!("unknown vault.edit operation: {operation}")),
                        };

                        let commit_source = format!("matrix:{}", message.sender);
                        let (status, detail, commit_hash, files_indexed, profile_refreshes) =
                            match result {
                                Ok((msg, mutations)) => {
                                    match self.apply_vault_edit_follow_through(
                                        &mutations,
                                        Some(&commit_source),
                                    ) {
                                        Ok((commit_hash, files_indexed, profile_refreshes)) => (
                                            Status::Success,
                                            msg,
                                            commit_hash,
                                            Some(files_indexed),
                                            profile_refreshes,
                                        ),
                                        Err(e) => (
                                            Status::Fail,
                                            format!(
                                                "vault.edit applied but follow-through failed: {e}"
                                            ),
                                            None,
                                            None,
                                            Vec::new(),
                                        ),
                                    }
                                }
                                Err(e) => (
                                    Status::Fail,
                                    format!("vault.edit failed: {e}"),
                                    None,
                                    None,
                                    Vec::new(),
                                ),
                            };

                        let mut event =
                            MatrixEventEnvelope::new(Kind::Message, status, now, &detail)
                                .with_detail_field("room", &*message.room_id)
                                .with_detail_field("entity_id", entity_id)
                                .with_detail_field("operation", operation);
                        if let Some(ref commit_hash) = commit_hash {
                            event = event.with_detail_field("git_commit", commit_hash.clone());
                        }
                        if let Some(files_indexed) = files_indexed {
                            event = event.with_detail_field("files_indexed", files_indexed);
                        }
                        let mut envelopes = vec![RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope: event,
                        }];
                        if status == Status::Success {
                            for refresh in profile_refreshes {
                                envelopes.push(build_entity_profile_updated_envelope(
                                    &refresh.entity_id,
                                    &refresh.content_hash,
                                    &refresh.doc_path,
                                    now,
                                    &message.room_id,
                                ));
                            }
                        }
                        return Ok(envelopes);
                    }

                    // --- Vault process: LLM processing via Distillery (T108 Phase 6, Pattern B) ---
                    if command == "vault.process" {
                        let llm = self.make_llm_client(symbiotic_core::Sensitivity::Shareable);
                        return self
                            .handle_vault_process_command_with_llm(message, &value, now, &llm)
                            .map_err(anyhow::Error::msg);
                    }
                }
            }
        } // end goal commands block

        // Non-JSON messages in #goals room — treat as natural language goal
        // if long enough, otherwise plain acknowledgement.
        if role == Some(RoomRole::Goals) || (role.is_none() && is_goals_room(&message.room_id)) {
            let body_trimmed = message.body.trim();
            if body_trimmed.len() > 10 {
                let goal_id = format!("{:x}", simple_hash(&format!("{}{}", body_trimmed, now)));
                let title = crate::thread_manager::ThreadManager::generate_title(body_trimmed);
                let inquisition_template = format!("inquisition:{goal_id}");

                // Persist the original goal text for future inquisition rounds.
                crate::goals::persist_goal_text(
                    &self.config.data_dir,
                    &message.room_id,
                    &inquisition_template,
                    body_trimmed,
                );
                let payload = crate::goals::WorkflowRunPayload {
                    project_id: Some(self.resolve_project_id_for_goal_materialization(
                        None,
                        crate::goals::attached_thread_id_for_room(&message.room_id).as_deref(),
                    )?),
                    template: inquisition_template.clone(),
                    goal_room: Some(message.room_id.clone()),
                    goal_sender: Some(message.sender.clone()),
                    user_answer: Some(body_trimmed.to_string()),
                    goal_id: Some(goal_id.clone()),
                    user_goal: Some(body_trimmed.to_string()),
                    replan_context: None,
                };

                let archive_root = self
                    .config
                    .archive_path
                    .clone()
                    .unwrap_or_else(|| self.config.data_dir.join("../knowledge-base"));
                let goal_summary = format!(
                    "Inquisition intake for goal '{}' from {}.",
                    body_trimmed, message.room_id
                );
                let context = crate::goal_management::GoalHierarchyContext {
                    project_id: payload
                        .project_id
                        .as_deref()
                        .unwrap_or(crate::goals::DEFAULT_UNSCOPED_PROJECT_ID),
                    goal_id: &goal_id,
                    title: &title,
                    summary: &goal_summary,
                    owner: Some(&message.sender),
                    thread_id: None,
                    observed_at: now as i64,
                };
                if let Err(error) = crate::goal_management::persist_goal_plan_archive(
                    &archive_root,
                    &context,
                    "inquisition",
                    &[],
                ) {
                    tracing::warn!(
                        goal_id = %goal_id,
                        %error,
                        "goal_management: failed to persist inquisition goal plan to archive"
                    );
                }

                let (job_id, _) = self.queue_workflow_run_with_payload(&payload)?;

                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: message.room_id.clone(),
                        thread_id: None,
                        project_id: payload
                            .project_id
                            .clone()
                            .unwrap_or_else(crate::goals::default_unscoped_project_id),
                        template: inquisition_template.clone(),
                        status: "running".to_string(),
                        last_job_id: job_id,
                        last_run_id: Some(goal_id.clone()),
                        owner: Some(message.sender.clone()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: Some("clarifying".to_string()),
                        audit_id: None,
                        plan_id: None,
                    },
                )?;
                crate::goal_management::sync_goal_work_item(
                    &self.management_store,
                    crate::goal_management::GoalWorkItemUpdate {
                        slug: &goal_id,
                        title: &title,
                        project_id: payload
                            .project_id
                            .as_deref()
                            .unwrap_or(crate::goals::DEFAULT_UNSCOPED_PROJECT_ID),
                        phase: Some("clarifying"),
                        owner: &message.sender,
                        thread_id: None,
                        priority: crate::goal_management::priority_from_goal_priority(40),
                        status: symbiotic_control_plane::WorkItemStatus::Running,
                        observed_at: now as i64,
                    },
                );

                let event =
                    MatrixEventEnvelope::new(Kind::Message, Status::Working, now, body_trimmed)
                        .with_thread(&goal_id)
                        .with_detail_field("goal_id", &*goal_id)
                        .with_detail_field("title", &*title)
                        .with_detail_field("template", inquisition_template);

                return Ok(vec![RoutedMatrixEnvelope {
                    room_id: message.room_id.clone(),
                    envelope: event,
                }]);
            }

            let event = MatrixEventEnvelope::new(
                Kind::Message,
                Status::Working,
                now,
                "Message received in goals room",
            )
            .with_detail_field("room", &*message.room_id)
            .with_detail_field("sender", &*message.sender);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        if role.is_none() && is_goal_room(&message.room_id) {
            return if let Some(template) = parse_goal_template(&message.body) {
                // Check if this goal is already active (dedup)
                if let Some(_existing_job_id) =
                    self.find_active_goal_workflow(&message.room_id, &template)?
                {
                    let event = MatrixEventEnvelope::new(
                        Kind::Message,
                        Status::Working,
                        now,
                        "Goal is already running",
                    )
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field("template", &*template);
                    return Ok(vec![RoutedMatrixEnvelope {
                        room_id: message.room_id.clone(),
                        envelope: event,
                    }]);
                }

                let (job_id, goal_id) =
                    self.queue_workflow_run_for_goal(&template, &message.room_id, &message.sender)?;
                append_goal_log(
                    &self.config.goal_log_file,
                    GoalLogEntry {
                        ts: now,
                        event: "goal.started",
                        workflow_job_id: &job_id,
                        goal_room: Some(&message.room_id),
                        goal_sender: Some(&message.sender),
                        template: &template,
                        detail: "queued",
                    },
                )?;
                upsert_goal_state(
                    &self.config.goal_state_file,
                    GoalState {
                        goal_room: message.room_id.clone(),
                        thread_id: None,
                        project_id: crate::goals::default_unscoped_project_id(),
                        template: template.clone(),
                        status: "queued".to_string(),
                        last_job_id: job_id.clone(),
                        last_run_id: None,
                        owner: Some(message.sender.clone()),
                        updated_at: now,
                        complexity: None,
                        pipeline_stage: None,
                        audit_id: None,
                        plan_id: None,
                    },
                )?;
                let event = MatrixEventEnvelope::new(
                    Kind::Message,
                    Status::Working,
                    now,
                    "Goal run accepted",
                )
                .with_detail_field("room", &*message.room_id)
                .with_detail_field("sender", &*message.sender)
                .with_detail_field("template", template)
                .with_detail_field("goal_id", goal_id);
                Ok(vec![RoutedMatrixEnvelope {
                    room_id: message.room_id.clone(),
                    envelope: event,
                }])
            } else {
                let event = MatrixEventEnvelope::new(
                    Kind::Message,
                    Status::Fail,
                    now,
                    "Goal message must include workflow template",
                )
                .with_detail_field("room", &*message.room_id)
                .with_detail_field("hint", "use: run <template>");
                Ok(vec![RoutedMatrixEnvelope {
                    room_id: message.room_id.clone(),
                    envelope: event,
                }])
            };
        }

        if role == Some(RoomRole::Status) || (role.is_none() && is_status_room(&message.room_id)) {
            let envelope = self.build_snapshot_envelope(now, None)?;
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope,
            }]);
        }

        if role == Some(RoomRole::Alerts) || (role.is_none() && is_alerts_room(&message.room_id)) {
            let event = MatrixEventEnvelope::state("alert.received", now, "Alert message received")
                .with_detail_field("room", &*message.room_id)
                .with_detail_field("sender", &*message.sender);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: message.room_id.clone(),
                envelope: event,
            }]);
        }

        warn!(
            "route: UNMATCHED room_id={} role={:?} — falling through to 'Unsupported room route'",
            message.room_id, role
        );
        let event =
            MatrixEventEnvelope::new(Kind::Message, Status::Fail, now, "Unsupported room route")
                .with_detail_field("room", &*message.room_id);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: message.room_id.clone(),
            envelope: event,
        }])
    }

    /// Handle a user message by classifying it with [`UxClassifier`] and routing
    /// to the appropriate handler: Quick → LLM chat reply, ShortTask → LLM task reply,
    /// Goal → deliberation pipeline, Intake → intake handler, Routing/FollowUp → quick reply (phase 4+).
    ///
    /// Emits a `classification.result` event first so the app can show
    /// lightweight pills ("Quick reply", "Goal created", etc.) before the
    /// actual handler response arrives.
    fn handle_classified_message(
        &self,
        message: &MatrixMessage,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        use symbiotic_agents::llm::{ChatMessage, LlmClient};

        let body_trimmed = message.body.trim();
        let (mut ux_class, mut confidence) = UxClassifier::classify(body_trimmed, &[]);

        info!(
            "ux_classifier: class={} confidence={:.2} body_len={} room={}",
            ux_class,
            confidence,
            body_trimmed.len(),
            message.room_id
        );

        // --- LLM fallback for ambiguous classification ---
        // When the rule-based classifier returns low confidence (the default 0.6
        // case), ask the LLM to disambiguate between quick/short_task/goal.
        if confidence < 0.75 {
            let llm = self.make_llm_client(symbiotic_core::Sensitivity::Shareable);
            let classify_messages = vec![
                ChatMessage {
                    role: "system".to_string(),
                    content: UX_CLASSIFIER_LLM_PROMPT.to_string(),
                },
                ChatMessage {
                    role: "user".to_string(),
                    content: body_trimmed.to_string(),
                },
            ];

            let llm_result = match tokio::runtime::Handle::try_current() {
                Ok(handle) => std::thread::scope(|s| {
                    s.spawn(move || handle.block_on(llm.chat(&classify_messages, false)))
                        .join()
                        .expect("LLM classify thread should not panic")
                }),
                Err(_) => Err(anyhow::anyhow!(
                    "no tokio runtime available for classification"
                )),
            };

            match llm_result {
                Ok(response) => {
                    if let Some(llm_class) = parse_llm_classification(&response) {
                        info!(
                            "ux_classifier_llm: class={} body_len={} (was rule={})",
                            llm_class,
                            body_trimmed.len(),
                            ux_class
                        );
                        ux_class = llm_class;
                        confidence = 0.85;
                    } else {
                        warn!(
                            "ux_classifier_llm: unparseable response={:?}, keeping rule-based class={}",
                            response, ux_class
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "ux_classifier_llm: LLM call failed: {e}, keeping rule-based class={}",
                        ux_class
                    );
                }
            }
        }

        // --- Emit classification feedback event ---
        let mut classification_event = MatrixEventEnvelope::state(
            "classification.result",
            now,
            &format!("class={} confidence={:.2}", ux_class, confidence),
        )
        .with_detail_field("room", &*message.room_id)
        .with_detail_field("sender", &*message.sender)
        .with_detail_field("class", ux_class.label())
        .with_detail_field("confidence", format!("{confidence:.2}"));

        // --- TopicRouter: suggest existing threads that match this message ---
        // Only useful for messages that might be follow-ups (Quick, ShortTask,
        // FollowUp). Skipped when there are no active threads.
        if let Ok(tm_guard) = self.thread_manager.lock() {
            if let Some(ref tm) = *tm_guard {
                let active_entries = tm.active_threads();
                if !active_entries.is_empty() {
                    let active_pairs: Vec<(String, String)> = active_entries
                        .iter()
                        .map(|e| (e.thread_id.clone(), e.title.clone()))
                        .collect();
                    let suggestions = TopicRouter::find_matches(body_trimmed, &active_pairs);
                    let above_threshold: Vec<_> = suggestions
                        .into_iter()
                        .filter(|s| s.confidence > 0.4)
                        .collect();
                    if !above_threshold.is_empty() {
                        // Serialize as JSON array for the app to parse.
                        let routing_json: Vec<serde_json::Value> = above_threshold
                            .iter()
                            .map(|s| {
                                serde_json::json!({
                                    "thread_id": s.thread_id,
                                    "thread_title": s.thread_title,
                                    "confidence": s.confidence,
                                    "reason": s.reason,
                                })
                            })
                            .collect();
                        if let Ok(json_str) = serde_json::to_string(&routing_json) {
                            classification_event = classification_event
                                .with_detail_field("routing_suggestions", json_str);
                        }
                        info!(
                            "topic_router: {} suggestion(s) above threshold for body_len={}",
                            above_threshold.len(),
                            body_trimmed.len(),
                        );
                    }
                }
            }
        }

        // For Quick/ShortTask, tag the classification event with _stream
        // thread_id so the app knows the response belongs in the stream.
        // For Goal, the thread_id is set later once the goal_id is known.
        match ux_class {
            UxClass::Quick | UxClass::Routing | UxClass::FollowUp { .. } | UxClass::ShortTask => {
                classification_event = classification_event.with_thread("_stream");
            }
            _ => {}
        }

        let classification_routed = RoutedMatrixEnvelope {
            room_id: message.room_id.clone(),
            envelope: classification_event,
        };

        match ux_class {
            UxClass::Quick | UxClass::Routing | UxClass::FollowUp { .. } => {
                // Direct LLM response — no agent, no queue.
                let llm = self.make_llm_client(symbiotic_core::Sensitivity::Shareable);
                let messages = vec![
                    ChatMessage {
                        role: "system".to_string(),
                        content: BRAIN_BOOTSTRAP_SYSTEM_PROMPT.to_string(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: body_trimmed.to_string(),
                    },
                ];

                let result = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => std::thread::scope(|s| {
                        s.spawn(move || handle.block_on(llm.chat(&messages, false)))
                            .join()
                            .expect("LLM chat thread should not panic")
                    }),
                    Err(_) => Err(anyhow::anyhow!("no tokio runtime available for chat reply")),
                };

                match result {
                    Ok(response) => {
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            &response,
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("classification", "quick")
                        .with_thread("_stream");
                        Ok(vec![
                            classification_routed,
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                        ])
                    }
                    Err(e) => {
                        warn!("chat.reply LLM error: {e}");
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Fail,
                            now,
                            &format!("LLM error: {e}"),
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("classification", "quick")
                        .with_thread("_stream");
                        Ok(vec![
                            classification_routed,
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                        ])
                    }
                }
            }

            UxClass::ShortTask => {
                // Single-agent task — same LLM call pattern with a task-oriented prompt.
                let llm = self.make_llm_client(symbiotic_core::Sensitivity::Shareable);
                let messages = vec![
                    ChatMessage {
                        role: "system".to_string(),
                        content: SHORT_TASK_SYSTEM_PROMPT.to_string(),
                    },
                    ChatMessage {
                        role: "user".to_string(),
                        content: body_trimmed.to_string(),
                    },
                ];

                let result = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => std::thread::scope(|s| {
                        s.spawn(move || handle.block_on(llm.chat(&messages, false)))
                            .join()
                            .expect("LLM chat thread should not panic")
                    }),
                    Err(_) => Err(anyhow::anyhow!("no tokio runtime available for task reply")),
                };

                match result {
                    Ok(response) => {
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Success,
                            now,
                            &response,
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("classification", "short_task")
                        .with_thread("_stream");
                        Ok(vec![
                            classification_routed,
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                        ])
                    }
                    Err(e) => {
                        warn!("task.result LLM error: {e}");
                        let event = MatrixEventEnvelope::new(
                            Kind::Message,
                            Status::Fail,
                            now,
                            &format!("LLM error: {e}"),
                        )
                        .with_detail_field("room", &*message.room_id)
                        .with_detail_field("sender", &*message.sender)
                        .with_detail_field("classification", "short_task")
                        .with_thread("_stream");
                        Ok(vec![
                            classification_routed,
                            RoutedMatrixEnvelope {
                                room_id: message.room_id.clone(),
                                envelope: event,
                            },
                        ])
                    }
                }
            }

            UxClass::Goal => {
                // Route to the deliberation pipeline (same as GoalDeliberate).
                let event_result = self.process_goal_through_pipeline(
                    body_trimmed,
                    &message.room_id,
                    &message.sender,
                    now,
                )?;

                // Use classify() for clean body text — avoids leaking raw
                // detail strings like "complexity=simple — starting inquisition".
                let (kind, status, body) = event_result.classify();
                let mut envelope = MatrixEventEnvelope::new(kind, status, now, &body)
                    .with_detail_field("room", &*message.room_id)
                    .with_detail_field("sender", &*message.sender)
                    .with_detail_field("description", body_trimmed)
                    .with_detail_field("title", body_trimmed.chars().take(80).collect::<String>())
                    .with_detail_field("template", "deliberation")
                    .with_detail_field("pipeline_status", event_result.status.as_str())
                    .with_detail_field("classification", "goal");

                if let Some(ref gid) = event_result.goal_id {
                    // Queue thread room creation for the async pump loop.
                    let slug = ThreadManager::generate_slug(body_trimmed);
                    let title = ThreadManager::generate_title(body_trimmed);
                    if let Ok(mut pending) = self.pending_room_creations.lock() {
                        pending.push(RoomCreationRequest {
                            thread_slug: slug,
                            thread_title: title,
                            goal_id: gid.clone(),
                        });
                    }

                    envelope = envelope.with_detail_field("goal_id", &**gid);
                    // Legacy compatibility: use goal_id as the initial thread
                    // identity when a dedicated thread surface has not been
                    // created yet (same as GoalDeliberate path).
                    envelope = envelope.with_thread(gid);
                    // Also tag the classification event with the goal's
                    // thread_id so the app can route the user's message out
                    // of the Stream thread and into the goal thread.
                    let goal_classification = RoutedMatrixEnvelope {
                        room_id: classification_routed.room_id.clone(),
                        envelope: classification_routed
                            .envelope
                            .clone()
                            .with_thread(gid)
                            .with_detail_field("goal_id", &**gid),
                    };
                    Ok(vec![
                        goal_classification,
                        RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        },
                    ])
                } else {
                    Ok(vec![
                        classification_routed,
                        RoutedMatrixEnvelope {
                            room_id: message.room_id.clone(),
                            envelope,
                        },
                    ])
                }
            }

            UxClass::Intake => {
                // Route to the intake handler.
                let reply = self.handle_intake_message(body_trimmed)?;
                let accepted_count = reply.result.summary.ingested
                    + reply.result.summary.duplicates
                    + reply.result.summary.secure_routed;
                let intake_status = if accepted_count > 0 {
                    Status::Working
                } else {
                    Status::Fail
                };
                let mut event =
                    MatrixEventEnvelope::new(Kind::Message, intake_status, now, &reply.body)
                        .with_detail_field("classification", "intake");
                event = event
                    .with_detail_field("total", reply.result.summary.total)
                    .with_detail_field("accepted", accepted_count)
                    .with_detail_field("ingested", reply.result.summary.ingested)
                    .with_detail_field("duplicates", reply.result.summary.duplicates)
                    .with_detail_field("invalid", reply.result.summary.invalid)
                    .with_detail_field("failed", reply.result.summary.failed);
                if let Some(first_item) = reply.result.items.first() {
                    if let Some(ref url) = first_item.normalized_url {
                        event = event.with_detail_field("url", url.as_str());
                    }
                }
                Ok(vec![
                    classification_routed,
                    RoutedMatrixEnvelope {
                        room_id: message.room_id.clone(),
                        envelope: event,
                    },
                ])
            }
        }
    }

    pub async fn pump_transport_once<T: MatrixTransport + ?Sized>(
        &self,
        transport: &T,
        now: u64,
    ) -> Result<usize> {
        let mut processed = 0usize;
        while let Some(message) = transport.pop_incoming().await {
            let outgoing = self.route_matrix_message_with_targets(&message, now)?;
            for routed in outgoing {
                self.send_matrix_event(transport, &routed.room_id, routed.envelope, now)
                    .await?;
            }
            processed += 1;
        }
        Ok(processed)
    }

    /// Build the Brain Bootstrap greeting envelope if it has never been sent.
    ///
    /// The greeting is sent exactly once per daemon data directory by writing a
    /// marker file at `{data_dir}/brain/greeting_sent`. Subsequent daemon
    /// restarts will see the marker and skip the greeting.
    ///
    /// Returns `None` when:
    /// - The greeting has already been sent (marker file exists).
    /// - No stream room is configured (cannot target a room).
    pub fn maybe_build_greeting(&self, now: u64) -> Option<RoutedMatrixEnvelope> {
        let stream_room = self.config.room_roles.stream.as_deref()?;
        let marker_dir = self.config.data_dir.join("brain");
        let marker_file = marker_dir.join("greeting_sent");

        if marker_file.exists() {
            return None;
        }

        // Write the marker file *before* returning the envelope so that even
        // if the send fails we don't spam the user on every restart.
        if let Err(e) = std::fs::create_dir_all(&marker_dir) {
            warn!("brain_bootstrap: failed to create marker dir: {e}");
        }
        if let Err(e) = std::fs::write(&marker_file, format!("{now}")) {
            warn!("brain_bootstrap: failed to write greeting marker: {e}");
        }

        let envelope = MatrixEventEnvelope::new(
            Kind::Message,
            Status::Success,
            now,
            BRAIN_BOOTSTRAP_GREETING,
        )
        .with_detail_field("classification", "greeting")
        .with_detail_field("room", stream_room);

        log::info!("brain_bootstrap: sending initial greeting to stream room {stream_room}");

        Some(RoutedMatrixEnvelope {
            room_id: stream_room.to_string(),
            envelope,
        })
    }

    pub async fn send_matrix_event<T: MatrixTransport + ?Sized>(
        &self,
        transport: &T,
        room_id: &str,
        envelope: MatrixEventEnvelope,
        now: u64,
    ) -> Result<()> {
        // --- Tier 3 phone-only filter (send path) ---
        // When `tier3_phone_only` is enabled and the envelope is tagged as
        // Private, replace the full content with a metadata-only placeholder.
        // The actual data stays on the phone; the VPS daemon only sees the stub.
        let envelope = if self.config.tier3_phone_only && envelope.is_private() {
            debug!(
                "tier3_phone_only: redacting Private event to placeholder kind={:?} thread={:?} room_id={}",
                envelope.sym.k, envelope.sym.t, room_id
            );
            envelope.redact_to_placeholder()
        } else {
            envelope
        };

        envelope.validate()?;
        transport.send_outgoing(room_id, envelope.clone()).await?;
        self.send_thread_observability_summary_if_needed(transport, room_id, &envelope, now)
            .await?;
        if let Some(push_event) = self.maybe_emit_push_event(&envelope, now)? {
            transport
                .send_outgoing(&push_event.room_id, push_event.envelope)
                .await?;
        }
        Ok(())
    }

    /// Process any pending thread room creation requests.
    ///
    /// Called from the async pump loop after processing incoming messages.
    /// Creates Matrix rooms via `ensure_single_room`, registers them in the
    /// `ThreadManager`, and emits `routing.created` events to `#stream`.
    pub async fn process_pending_room_creations<
        T: symbiotic_matrix::transport::MatrixTransport + ?Sized,
    >(
        &self,
        _transport: &T,
        now: u64,
    ) -> anyhow::Result<Vec<crate::events::RoutedMatrixEnvelope>> {
        let requests: Vec<RoomCreationRequest> = {
            let mut pending = self
                .pending_room_creations
                .lock()
                .map_err(|_| anyhow::anyhow!("pending_room_creations lock"))?;
            pending.drain(..).collect()
        };

        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let (homeserver, access_token, server_name) = match (
            self.config.matrix_homeserver.as_deref(),
            self.config.matrix_access_token.as_deref(),
            self.config.matrix_server_name.as_deref(),
        ) {
            (Some(h), Some(t), Some(s)) => (h, t, s),
            _ => {
                tracing::warn!(
                    "process_pending_room_creations: no Matrix credentials configured, \
                     {} request(s) skipped",
                    requests.len()
                );
                return Ok(Vec::new());
            }
        };

        let mut routed_events = Vec::new();
        let stream_room = self
            .resolve_room(crate::routing::RoomRole::Stream)
            .to_string();

        for req in requests {
            let alias = format!("thread-{}", req.thread_slug);
            match symbiotic_matrix::registration::ensure_single_room(
                homeserver,
                access_token,
                server_name,
                &alias,
            )
            .await
            {
                Ok(created) => {
                    tracing::info!(
                        alias = %alias,
                        room_id = %created.room_id,
                        already_existed = created.already_existed,
                        "thread room created"
                    );
                    // Register in ThreadManager
                    if let Ok(mut tm_guard) = self.thread_manager.lock() {
                        if let Some(ref mut mgr) = *tm_guard {
                            let event = mgr.create_thread(
                                &req.thread_slug,
                                &req.thread_title,
                                &created.room_id,
                                now,
                            );
                            let _ = mgr.save();
                            // Emit routing.created to #stream
                            let envelope = event.to_envelope(now);
                            routed_events.push(crate::events::RoutedMatrixEnvelope {
                                room_id: stream_room.clone(),
                                envelope,
                            });
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        alias = %alias,
                        error = %e,
                        "thread room creation failed"
                    );
                }
            }
        }

        Ok(routed_events)
    }

    /// Check whether an incoming Matrix event should be filtered out
    /// because it is tagged as Private and `tier3_phone_only` is enabled.
    ///
    /// Returns `true` when the event should be **skipped** (not processed).
    pub fn should_skip_tier3_event(&self, body: &str) -> bool {
        if !self.config.tier3_phone_only {
            return false;
        }
        // Try parsing as a Symbiotic event envelope to check the sensitivity tag.
        let Ok(parsed) = MatrixEventEnvelope::parse_strict(body) else {
            return false;
        };
        parsed.is_private()
    }

    pub fn register_push_device(
        &self,
        device_id: &str,
        token: &str,
        platform: &str,
        now: u64,
    ) -> Result<PushDevice> {
        self.push_registry.register(device_id, token, platform, now)
    }

    pub fn list_push_devices(&self) -> Result<Vec<PushDevice>> {
        self.push_registry.list()
    }

    fn maybe_emit_push_event(
        &self,
        envelope: &MatrixEventEnvelope,
        now: u64,
    ) -> Result<Option<RoutedMatrixEnvelope>> {
        let priority = match push_priority_for_event(envelope) {
            Some(priority) => priority,
            None => return Ok(None),
        };
        let devices = self.push_registry.list()?;
        if devices.is_empty() {
            return Ok(None);
        }

        let mut failures = Vec::new();
        let envelope_thread = envelope.sym.t.clone().unwrap_or_default();
        let envelope_kind = format!("{:?}", envelope.sym.k);
        let envelope_status = envelope
            .sym
            .s
            .map(|s| format!("{:?}", s))
            .unwrap_or_default();
        for device in &devices {
            let badge = self.push_registry.increment_badge(&device.device_id);
            let notification = PushNotification {
                notification_id: format!(
                    "push_{:x}",
                    simple_hash(&format!(
                        "{}:{}:{}:{}",
                        device.device_id, envelope_kind, envelope_thread, now
                    ))
                ),
                device_id: device.device_id.clone(),
                token_hash: device.token_hash.clone(),
                encrypted_token: device.encrypted_token.clone(),
                platform: device.platform.clone(),
                priority: priority.as_str().to_string(),
                title: push_title_for_event(envelope),
                body: envelope.body.clone(),
                rid: envelope_thread.clone(),
                event_type: envelope_kind.clone(),
                event_status: envelope_status.clone(),
                ts: now,
                thread_id: envelope.sym.t.clone(),
                badge: Some(badge),
            };
            if let Err(err) = self.push_provider.send(&notification) {
                let _ = append_push_telemetry(
                    &self.config.push_telemetry_file,
                    &notification,
                    "failed",
                    Some(&err.to_string()),
                );
                failures.push(err.to_string());
            } else {
                let _ = append_push_telemetry(
                    &self.config.push_telemetry_file,
                    &notification,
                    "sent",
                    None,
                );
            }
        }

        let mut event = if failures.is_empty() {
            MatrixEventEnvelope::state("push.sent", now, "Push notification sent")
        } else {
            MatrixEventEnvelope::state("push.failed", now, "Push notification failed")
                .with_detail_field("error", failures.join("; "))
        };
        event = event
            .with_detail_field("priority", priority.as_str())
            .with_detail_field("event_kind", &*envelope_kind)
            .with_detail_field("event_status", &*envelope_status)
            .with_detail_field("device_count", devices.len());

        Ok(Some(RoutedMatrixEnvelope {
            room_id: self.resolve_room(RoomRole::Alerts).to_string(),
            envelope: event,
        }))
    }

    /// Handle the `vault.key_rotation.start` command.
    ///
    /// Generates a new age X25519 keypair, re-encrypts all blobs in the blob
    /// store with the new key, writes the new key to the key file, and emits
    /// progress / completed / failed events back to the control room.
    ///
    /// Returns multiple `RoutedMatrixEnvelope`s: a progress event before
    /// rotation begins, then a completed or failed event when done.
    fn handle_key_rotation(
        &self,
        room_id: &str,
        sender: &str,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let _run_id = format!(
            "keyrot_{:x}",
            simple_hash(&format!("{room_id}:{sender}:{now}"))
        );

        // 1. Verify blob store is configured.
        let blob_store = match self.encrypted_blob_store.as_ref() {
            Some(store) => store,
            None => {
                let event = MatrixEventEnvelope::state(
                    "vault.key_rotation.failed",
                    now,
                    "Blob store not configured",
                )
                .with_detail_field("room", room_id)
                .with_detail_field("sender", sender)
                .with_detail_field("reason", "no blob store key file configured");
                return Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }]);
            }
        };

        // 2. Read the old identity from the key file.
        let key_file = match self.config.blob_store_key_file.as_ref() {
            Some(path) => path.clone(),
            None => {
                let event = MatrixEventEnvelope::state(
                    "vault.key_rotation.failed",
                    now,
                    "Key file path not configured",
                )
                .with_detail_field("room", room_id)
                .with_detail_field("sender", sender)
                .with_detail_field("reason", "blob_store_key_file not set");
                return Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }]);
            }
        };

        let old_key_data = match std::fs::read_to_string(&key_file) {
            Ok(data) => data,
            Err(e) => {
                let event = MatrixEventEnvelope::state(
                    "vault.key_rotation.failed",
                    now,
                    "Failed to read current key file",
                )
                .with_detail_field("room", room_id)
                .with_detail_field("sender", sender)
                .with_detail_field("reason", format!("read error: {e}"));
                return Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }]);
            }
        };

        let old_identity: AgeIdentity = match old_key_data.trim().parse() {
            Ok(id) => id,
            Err(e) => {
                let event = MatrixEventEnvelope::state(
                    "vault.key_rotation.failed",
                    now,
                    "Invalid age identity in key file",
                )
                .with_detail_field("room", room_id)
                .with_detail_field("sender", sender)
                .with_detail_field("reason", format!("parse error: {e}"));
                return Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }]);
            }
        };

        // Count blobs before rotation for progress reporting.
        let blob_count = blob_store.list(None).map(|list| list.len()).unwrap_or(0);

        // 3. Generate new keypair.
        let new_identity = AgeIdentity::generate();
        let new_recipient = new_identity.to_public();

        let mut envelopes = Vec::new();

        // Emit progress event: rotation starting.
        let progress = MatrixEventEnvelope::state(
            "vault.key_rotation.progress",
            now,
            "Key rotation in progress",
        )
        .with_detail_field("room", room_id)
        .with_detail_field("sender", sender)
        .with_detail_field("total_blobs", blob_count)
        .with_detail_field("phase", "re-encrypting");
        envelopes.push(RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: progress,
        });

        // 4. Rotate keys — re-encrypt all blobs.
        let rotated = match blob_store.rotate_key(&old_identity, &[&new_recipient]) {
            Ok(count) => count,
            Err(e) => {
                let failed = MatrixEventEnvelope::state(
                    "vault.key_rotation.failed",
                    now,
                    "Key rotation failed during re-encryption",
                )
                .with_detail_field("room", room_id)
                .with_detail_field("sender", sender)
                .with_detail_field("reason", format!("rotate error: {e}"))
                .with_detail_field("total_blobs", blob_count);
                envelopes.push(RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: failed,
                });
                return Ok(envelopes);
            }
        };

        // 5. Write the new identity to the key file.
        let new_key_string = new_identity.to_string();
        let new_key_bytes = new_key_string.expose_secret().as_bytes();
        if let Err(e) = std::fs::write(&key_file, new_key_bytes) {
            // Critical: blobs are re-encrypted but key file not updated.
            // The old key is now invalid for the re-encrypted blobs.
            // We MUST log this prominently.
            warn!(
                "key_rotation: CRITICAL — blobs re-encrypted but key file write failed: {}. \
                 The new key is lost. Manual recovery required.",
                e
            );
            let failed = MatrixEventEnvelope::state(
                "vault.key_rotation.failed",
                now,
                "CRITICAL: Blobs re-encrypted but key file write failed",
            )
            .with_detail_field("room", room_id)
            .with_detail_field("sender", sender)
            .with_detail_field("reason", format!("key file write error: {e}"))
            .with_detail_field("rotated_blobs", rotated)
            .with_detail_field("recovery", "manual intervention required — new key lost");
            envelopes.push(RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: failed,
            });
            return Ok(envelopes);
        }

        // Harden key file permissions (best-effort).
        let _ = crate::harden_file_permissions(&key_file, 0o600);

        // 6. Emit completed event.
        let completed = MatrixEventEnvelope::state(
            "vault.key_rotation.completed",
            now,
            "Key rotation completed successfully",
        )
        .with_detail_field("room", room_id)
        .with_detail_field("sender", sender)
        .with_detail_field("rotated_blobs", rotated)
        .with_detail_field("total_blobs", blob_count);
        envelopes.push(RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: completed,
        });

        Ok(envelopes)
    }

    fn escalation_forward_from_failed_event(
        &self,
        message: &MatrixMessage,
        now: u64,
    ) -> Result<Option<RoutedMatrixEnvelope>> {
        // Skip forwarding if the message already comes from the alerts room
        let is_alerts = self.classify_room(&message.room_id) == Some(RoomRole::Alerts)
            || (!self.room_roles.is_configured() && is_alerts_room(&message.room_id));
        if is_alerts {
            return Ok(None);
        }
        let parsed = match MatrixEventEnvelope::parse_strict(&message.body) {
            Ok(parsed) => parsed,
            Err(_) => return Ok(None),
        };
        // In v2, failed events have status == Fail
        if parsed.sym.s != Some(Status::Fail) {
            return Ok(None);
        }
        let source_room = room_alias(&message.room_id).to_string();
        let source_kind = format!("{:?}", parsed.sym.k);
        let source_thread = parsed.sym.t.clone().unwrap_or_default();

        let alert =
            MatrixEventEnvelope::state("alert.forwarded", now, "Failure event forwarded to alerts")
                .with_detail_field("source_room", source_room)
                .with_detail_field("source_kind", source_kind)
                .with_detail_field("source_thread", source_thread)
                .with_detail_field("source_sender", message.sender.as_str());
        Ok(Some(RoutedMatrixEnvelope {
            room_id: self.resolve_room(RoomRole::Alerts).to_string(),
            envelope: alert,
        }))
    }

    // -----------------------------------------------------------------------
    //  Credential event detection (guard against UI leakage)
    // -----------------------------------------------------------------------

    /// Detect whether a message body is a credential command that should be
    /// routed to the credentials room. Returns the command type string
    /// (e.g. "credential.submit") when detected, `None` otherwise.
    ///
    /// Checks for:
    /// 1. v2 sym.c command format with `"c": "credential.*"` or `"c": "api_credential.*"`
    /// 2. Symbiotic event envelopes with `"a": "credential.*"` — envelope format
    fn detect_credential_command(body: &str) -> Option<&'static str> {
        let trimmed = body.trim();
        if !trimmed.starts_with('{') {
            return None;
        }
        let parsed: serde_json::Value = serde_json::from_str(trimmed).ok()?;
        let obj = parsed.as_object()?;

        // Check v2 sym.c command format: { "msgtype": "sym.c", "sym": { "c": "credential.*" } }
        if let Some(sym) = obj.get("sym").and_then(|v| v.as_object()) {
            if let Some(cmd) = sym.get("c").and_then(|v| v.as_str()) {
                if cmd.starts_with("credential.") || cmd.starts_with("api_credential.") {
                    return match cmd {
                        "credential.submit" | "api_credential.submit" => Some("credential.submit"),
                        "credential.query" | "api_credential.query" => Some("credential.query"),
                        "credential.remove" | "api_credential.remove" => Some("credential.remove"),
                        "credential.authenticate" => Some("credential.authenticate"),
                        "credential.approve" => Some("credential.approve"),
                        "credential.deny" => Some("credential.deny"),
                        "credential.respond" => Some("credential.respond"),
                        "credential.approval_policy.list" => {
                            Some("credential.approval_policy.list")
                        }
                        "credential.approval_policy.revoke" => {
                            Some("credential.approval_policy.revoke")
                        }
                        _ => Some("credential.unknown"),
                    };
                }
            }
            // Also check envelope format: { "sym": { "a": "credential.*" } }
            if let Some(action) = sym.get("a").and_then(|v| v.as_str()) {
                if action.starts_with("credential.") {
                    return Some("credential.envelope");
                }
            }
        }

        None
    }

    // -----------------------------------------------------------------------
    //  API Credential Handlers (key=value format from Flutter app)
    // -----------------------------------------------------------------------

    /// Compute a masked suffix (last 4 characters) for display purposes.
    fn masked_suffix(value: &str) -> String {
        let len = value.len();
        if len >= 4 {
            value[len - 4..].to_string()
        } else {
            "*".repeat(4)
        }
    }

    /// Metadata sidecar key for an API credential.
    fn meta_key(key: &str) -> String {
        format!("_meta:{key}")
    }

    /// Store or update credential metadata as a JSON sidecar entry in the vault.
    fn store_api_credential_meta(
        &self,
        key: &str,
        status: &str,
        masked_suffix: &str,
        last_verified: Option<u64>,
    ) -> Result<()> {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "status".to_string(),
            serde_json::Value::String(status.to_string()),
        );
        meta.insert(
            "masked_suffix".to_string(),
            serde_json::Value::String(masked_suffix.to_string()),
        );
        if let Some(ts) = last_verified {
            meta.insert(
                "last_verified".to_string(),
                serde_json::Value::String(ts.to_string()),
            );
        }
        let meta_json = serde_json::Value::Object(meta).to_string();
        let meta_record = credential_gateway::CredentialRecord {
            service: Self::meta_key(key),
            username: key.to_string(),
            secret: meta_json,
            totp_secret: None,
        };
        self.credential_vault.put(meta_record)
    }

    /// Load credential metadata from the vault sidecar.
    fn load_api_credential_meta(
        &self,
        key: &str,
    ) -> Result<Option<serde_json::Map<String, serde_json::Value>>> {
        let meta_key = Self::meta_key(key);
        match self.credential_vault.get(&meta_key)? {
            Some(record) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(&record.secret).unwrap_or_default();
                Ok(parsed.as_object().cloned())
            }
            None => Ok(None),
        }
    }

    /// Handle `credential.submit` for API credentials (key/value format).
    fn handle_api_credential_submit(
        &self,
        room_id: &str,
        _sender: &str,
        key: String,
        value: String,
        validate: bool,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        use crate::credential_validator::CredentialValidationResult;

        let _rid = format!("cred_{:x}", simple_hash(&format!("{key}:{now}")));
        let masked = Self::masked_suffix(&value);

        // Optionally validate the credential using blocking HTTP.
        let validation_result = if validate {
            self.credential_validator.validate(&key, &value)
        } else {
            CredentialValidationResult::Skipped
        };

        let (status, last_verified, reason) = match &validation_result {
            CredentialValidationResult::Valid => ("valid", Some(now), None),
            CredentialValidationResult::Invalid { reason } => {
                ("unverified", None, Some(reason.clone()))
            }
            CredentialValidationResult::Unreachable { reason } => {
                ("unverified", None, Some(reason.clone()))
            }
            CredentialValidationResult::Skipped => ("unverified", None, None),
        };

        // Store the credential in the vault (bypassing hostname normalization).
        // Clone value before moving into the record — needed for provider registration.
        let api_key_value = value.clone();
        let record = credential_gateway::CredentialRecord {
            service: key.clone(),
            username: key.clone(),
            secret: value,
            totp_secret: None,
        };
        match self.credential_vault.put(record) {
            Ok(()) => {
                // Store metadata sidecar.
                if let Err(e) = self.store_api_credential_meta(&key, status, &masked, last_verified)
                {
                    warn!("failed to store credential metadata for {key}: {e}");
                }

                // --- Dynamic provider registration ---
                // If this API key corresponds to a known provider that isn't yet
                // registered, create and register it so it's immediately usable
                // without restarting the daemon.
                self.maybe_register_provider(&key, &api_key_value);

                let mut event =
                    MatrixEventEnvelope::state("credential.status", now, "Credential stored")
                        .with_detail_field("key", &*key)
                        .with_detail_field("status", status);

                if let Some(ts) = last_verified {
                    event = event.with_detail_field("last_verified", ts.to_string());
                }
                if let Some(reason) = reason {
                    event = event.with_detail_field("reason", reason);
                }

                Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }])
            }
            Err(e) => {
                warn!("api credential.submit failed for {key}: {e}");
                let event = MatrixEventEnvelope::state(
                    "credential.status",
                    now,
                    "Failed to store credential",
                )
                .with_detail_field("key", &*key)
                .with_detail_field("reason", e.to_string());
                Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }])
            }
        }
    }

    /// Dynamically register a completion provider when an API key is submitted.
    ///
    /// If the key name maps to a known provider that isn't already registered,
    /// creates the provider and registers it. If no default completion provider
    /// is set, this provider becomes the default.
    fn maybe_register_provider(&self, key: &str, api_key: &str) {
        use std::sync::Arc;
        use symbiotic_providers::{
            AnthropicProvider, GenericOpenAiCompatProvider, OpenAiCompletionProvider, ProviderAuth,
            ProviderCapability, RegisteredProvider,
        };

        let (name, provider): (
            &str,
            Option<Arc<dyn symbiotic_providers::CompletionProvider>>,
        ) = match key {
            "GEMINI_API_KEY" => {
                let model = self
                    .config
                    .gemini_model
                    .clone()
                    .unwrap_or_else(|| "gemini-2.5-flash".to_string());
                (
                    "gemini",
                    Some(Arc::new(GenericOpenAiCompatProvider::gemini(
                        ProviderAuth::ApiKey(api_key.to_string()),
                        model,
                    ))),
                )
            }
            "ANTHROPIC_API_KEY" => (
                "anthropic",
                Some(Arc::new(AnthropicProvider::new(
                    ProviderAuth::ApiKey(api_key.to_string()),
                    "claude-sonnet-4-6".to_string(),
                ))),
            ),
            "OPENAI_API_KEY" => (
                "openai",
                Some(Arc::new(OpenAiCompletionProvider::new(
                    ProviderAuth::ApiKey(api_key.to_string()),
                    "gpt-4.1".to_string(),
                ))),
            ),
            "OPENROUTER_API_KEY" => (
                "openrouter",
                Some(Arc::new(GenericOpenAiCompatProvider::openrouter(
                    ProviderAuth::ApiKey(api_key.to_string()),
                    "anthropic/claude-sonnet-4".to_string(),
                ))),
            ),
            _ => return, // Unknown key — no provider to register.
        };

        let provider = match provider {
            Some(p) => p,
            None => return,
        };

        let registry_lock = self.provider_router.registry();
        let mut registry = match registry_lock.write() {
            Ok(r) => r,
            Err(e) => {
                warn!("failed to acquire provider registry write lock: {e}");
                return;
            }
        };

        // Only register if not already present (don't clobber an existing provider
        // that may have been configured with different settings at startup).
        if registry.get(name).is_some() {
            tracing::info!(
                "provider '{name}' already registered, skipping dynamic registration for {key}"
            );
            return;
        }

        tracing::info!("dynamically registering provider '{name}' from {key}");
        let base: Arc<dyn symbiotic_providers::ModelProvider> = provider.clone();
        registry.register(RegisteredProvider {
            base,
            completion: Some(provider),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });

        // If no default completion provider is set, make this one the default.
        if registry
            .default_for(ProviderCapability::Completion)
            .is_none()
        {
            tracing::info!("setting '{name}' as default completion provider");
            let _ = registry.set_default(ProviderCapability::Completion, name);
        }
    }

    /// Handle `credential.query` for API credentials (keys format).
    fn handle_api_credential_query(
        &self,
        room_id: &str,
        _sender: &str,
        keys: Vec<String>,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let _rid = format!(
            "qry_{:x}",
            simple_hash(&format!("{}:{now}", keys.join(",")))
        );
        let total = keys.len();
        let mut configured: usize = 0;
        let mut missing: usize = 0;
        let mut events = Vec::with_capacity(total + 1);

        for key in &keys {
            match self.credential_vault.get(key) {
                Ok(Some(_record)) => {
                    configured += 1;
                    // Load metadata sidecar for status details.
                    let meta = self.load_api_credential_meta(key).unwrap_or(None);
                    let status = meta
                        .as_ref()
                        .and_then(|m| m.get("status"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("unverified");
                    let masked_suffix = meta
                        .as_ref()
                        .and_then(|m| m.get("masked_suffix"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("****");
                    let last_verified = meta
                        .as_ref()
                        .and_then(|m| m.get("last_verified"))
                        .and_then(|v| v.as_str());

                    let mut event = MatrixEventEnvelope::state(
                        "credential.query.result",
                        now,
                        "Credential found",
                    )
                    .with_detail_field("key", key.as_str())
                    .with_detail_field("status", status)
                    .with_detail_field("masked_suffix", masked_suffix);

                    if let Some(ts) = last_verified {
                        event = event.with_detail_field("last_verified", ts);
                    }

                    events.push(RoutedMatrixEnvelope {
                        room_id: room_id.to_string(),
                        envelope: event,
                    });
                }
                Ok(None) => {
                    missing += 1;
                    let event = MatrixEventEnvelope::state(
                        "credential.query.result",
                        now,
                        "Credential missing",
                    )
                    .with_detail_field("key", key.as_str())
                    .with_detail_field("status", "missing");

                    events.push(RoutedMatrixEnvelope {
                        room_id: room_id.to_string(),
                        envelope: event,
                    });
                }
                Err(e) => {
                    missing += 1;
                    warn!("credential.query failed for {key}: {e}");
                    let event = MatrixEventEnvelope::state(
                        "credential.query.result",
                        now,
                        "Failed to query credential",
                    )
                    .with_detail_field("key", key.as_str())
                    .with_detail_field("status", "missing")
                    .with_detail_field("reason", e.to_string());

                    events.push(RoutedMatrixEnvelope {
                        room_id: room_id.to_string(),
                        envelope: event,
                    });
                }
            }
        }

        // Final summary event.
        let done_event =
            MatrixEventEnvelope::state("credential.query.done", now, "Credential query complete")
                .with_detail_field("total", total.to_string())
                .with_detail_field("configured", configured.to_string())
                .with_detail_field("missing", missing.to_string());

        events.push(RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: done_event,
        });

        Ok(events)
    }

    /// Handle `credential.remove` for API credentials (key format).
    fn handle_api_credential_remove(
        &self,
        room_id: &str,
        _sender: &str,
        key: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let _rid = format!("cred_{:x}", simple_hash(&format!("{key}:{now}")));

        match self.credential_vault.delete(&key) {
            Ok(found) => {
                // Also delete metadata sidecar.
                let meta_key = Self::meta_key(&key);
                if let Err(e) = self.credential_vault.delete(&meta_key) {
                    warn!("failed to delete credential metadata for {key}: {e}");
                }

                if found {
                    let event = MatrixEventEnvelope::new(
                        Kind::Message,
                        Status::Success,
                        now,
                        "Credential removed",
                    )
                    .with_detail_field("key", &*key);
                    Ok(vec![RoutedMatrixEnvelope {
                        room_id: room_id.to_string(),
                        envelope: event,
                    }])
                } else {
                    let event = MatrixEventEnvelope::new(
                        Kind::Message,
                        Status::Success,
                        now,
                        "Credential not found (already removed)",
                    )
                    .with_detail_field("key", &*key);
                    Ok(vec![RoutedMatrixEnvelope {
                        room_id: room_id.to_string(),
                        envelope: event,
                    }])
                }
            }
            Err(e) => {
                warn!("api credential.remove failed for {key}: {e}");
                let event = MatrixEventEnvelope::new(
                    Kind::Message,
                    Status::Fail,
                    now,
                    "Failed to remove credential",
                )
                .with_detail_field("key", &*key)
                .with_detail_field("reason", e.to_string());
                Ok(vec![RoutedMatrixEnvelope {
                    room_id: room_id.to_string(),
                    envelope: event,
                }])
            }
        }
    }

    /// Handle `credential.authenticate` — browser-automated domain login.
    ///
    /// Compatibility ingress into the shared auth-job lifecycle.
    /// A manual credentials-room invocation is treated as an explicit user
    /// approval, so the auth sandbox executes immediately after the job is
    /// created instead of waiting for a separate `credential.approve`.
    fn handle_credential_authenticate(
        &self,
        room_id: &str,
        sender: &str,
        domain: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).authenticate_room(room_id, sender, domain, now)
    }

    fn open_recall_probe_store(&self) -> Result<RecallProbeStore> {
        Ok(RecallProbeStore::open(
            self.config.data_dir.join("runtime/recall-probes.db"),
        )?)
    }

    fn handle_recall_probe_run(
        &self,
        room_id: &str,
        top_k: usize,
        max_subjects: usize,
        max_queries_per_subject: usize,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        self.execute_recall_probe_run(room_id, top_k, max_subjects, max_queries_per_subject, now)
    }

    pub fn run_periodic_recall_probe_cycle(&self, now: u64) -> Result<Vec<RoutedMatrixEnvelope>> {
        let room_id = self.resolve_room(RoomRole::Status).to_string();
        self.execute_periodic_recall_probe_run(
            &room_id,
            self.config.recall_probe_top_k,
            self.config.recall_probe_max_subjects_per_run,
            self.config.recall_probe_max_queries_per_subject,
            now,
        )
    }

    fn execute_recall_probe_run(
        &self,
        room_id: &str,
        top_k: usize,
        max_subjects: usize,
        max_queries_per_subject: usize,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let memory_db_path = self.config.data_dir.join("memory.db");
        let run = run_recall_probe_batch(
            &self.recall_gateway,
            &self.archive_store,
            &self.vault_store,
            Some(memory_db_path.as_path()),
            &probe_store,
            top_k,
            max_subjects,
            max_queries_per_subject,
        )?;
        let body = format!(
            "Recall probe run completed: {}/{} subjects reachable",
            run.matched_count, run.subject_count
        );
        let event = MatrixEventEnvelope::state("recall.probe.completed", now, &body)
            .with_detail_field("run_id", run.id.as_str())
            .with_detail_field("cohort", run.cohort.as_deref().unwrap_or("ad_hoc"))
            .with_detail_field("top_k", run.top_k)
            .with_detail_field("max_subjects", max_subjects)
            .with_detail_field("max_queries_per_subject", max_queries_per_subject)
            .with_detail_field("subject_count", run.subject_count)
            .with_detail_field("matched_subject_count", run.matched_count)
            .with_detail_field(
                "unmatched_subject_count",
                run.subject_count.saturating_sub(run.matched_count),
            );
        let mut envelopes = vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }];
        for escalation in collect_recall_probe_escalations(&probe_store, &run.id)? {
            let proposal_id = proposals::generate_recall_probe_proposal_id(
                escalation.target_kind.as_str(),
                &escalation.target_id,
            );
            let description = format!(
                "{}:{} has been unreachable through the Recall Gateway for {} consecutive probe runs.",
                escalation.target_kind.as_str(),
                escalation.target_id,
                escalation.consecutive_failures
            );
            let suggestion = format!(
                "Review retrieval coverage and apply the indicated remediation: {}",
                Self::format_recall_probe_flags(&escalation.remediation_flags)
            );
            self.proposal_store.insert(proposals::PendingProposal {
                id: proposal_id.clone(),
                source: proposals::ProposalSource::RecallProbe,
                description: description.clone(),
                suggestion: suggestion.clone(),
                thread_id: None,
            });
            let flags_json = serde_json::to_string(
                &escalation
                    .remediation_flags
                    .iter()
                    .map(|flag| Self::recall_probe_flag_label(*flag))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| "[]".to_string());
            let proposal_event = MatrixEventEnvelope::new(
                Kind::Notification,
                Status::Success,
                now,
                &format!("{description}\n\nSuggestion: {suggestion}"),
            )
            .with_choices(vec![
                format!("proposal.approve {proposal_id}"),
                format!("proposal.dismiss {proposal_id}"),
            ])
            .with_detail_field("proposal_id", proposal_id.as_str())
            .with_detail_field("proposal_source", "recall_probe")
            .with_detail_field("target_kind", escalation.target_kind.as_str())
            .with_detail_field("target_id", escalation.target_id.as_str())
            .with_detail_field("consecutive_failures", escalation.consecutive_failures)
            .with_detail_field("remediation_flags", flags_json);
            envelopes.push(RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: proposal_event,
            });
        }
        Ok(envelopes)
    }

    fn execute_periodic_recall_probe_run(
        &self,
        room_id: &str,
        top_k: usize,
        max_subjects: usize,
        max_queries_per_subject: usize,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let memory_db_path = self.config.data_dir.join("memory.db");
        let (subjects, baseline_missing_count, baseline_reseeded) = self
            .resolve_periodic_recall_probe_subjects(
                &probe_store,
                memory_db_path.as_path(),
                max_subjects,
                now,
            )?;
        let run = run_recall_probe_batch_for_subjects(
            &self.recall_gateway,
            &probe_store,
            top_k,
            max_queries_per_subject,
            Some(PERIODIC_RECALL_PROBE_COHORT.to_string()),
            subjects,
        )?;
        let body = format!(
            "Recall probe run completed: {}/{} subjects reachable",
            run.matched_count, run.subject_count
        );
        let event = MatrixEventEnvelope::state("recall.probe.completed", now, &body)
            .with_detail_field("run_id", run.id.as_str())
            .with_detail_field("cohort", PERIODIC_RECALL_PROBE_COHORT)
            .with_detail_field("top_k", run.top_k)
            .with_detail_field("max_subjects", max_subjects)
            .with_detail_field("max_queries_per_subject", max_queries_per_subject)
            .with_detail_field("subject_count", run.subject_count)
            .with_detail_field("matched_subject_count", run.matched_count)
            .with_detail_field(
                "unmatched_subject_count",
                run.subject_count.saturating_sub(run.matched_count),
            )
            .with_detail_field("baseline_missing_subject_count", baseline_missing_count)
            .with_detail_field("baseline_reseeded", baseline_reseeded);
        let mut envelopes = vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }];
        for escalation in collect_recall_probe_escalations(&probe_store, &run.id)? {
            let proposal_id = proposals::generate_recall_probe_proposal_id(
                escalation.target_kind.as_str(),
                &escalation.target_id,
            );
            let description = format!(
                "{}:{} has been unreachable through the Recall Gateway for {} consecutive probe runs.",
                escalation.target_kind.as_str(),
                escalation.target_id,
                escalation.consecutive_failures
            );
            let suggestion = format!(
                "Review retrieval coverage and apply the indicated remediation: {}",
                Self::format_recall_probe_flags(&escalation.remediation_flags)
            );
            self.proposal_store.insert(proposals::PendingProposal {
                id: proposal_id.clone(),
                source: proposals::ProposalSource::RecallProbe,
                description: description.clone(),
                suggestion: suggestion.clone(),
                thread_id: None,
            });
            let flags_json = serde_json::to_string(
                &escalation
                    .remediation_flags
                    .iter()
                    .map(|flag| Self::recall_probe_flag_label(*flag))
                    .collect::<Vec<_>>(),
            )
            .unwrap_or_else(|_| "[]".to_string());
            let envelope = MatrixEventEnvelope::state(
                "proposal.created",
                now,
                &format!("Proposal created: {description}"),
            )
            .with_detail_field("proposal_id", proposal_id)
            .with_detail_field("source", "recall_probe")
            .with_detail_field("description", description)
            .with_detail_field("suggestion", suggestion)
            .with_detail_field("target_kind", escalation.target_kind.as_str())
            .with_detail_field("target_id", escalation.target_id.as_str())
            .with_detail_field("consecutive_failures", escalation.consecutive_failures)
            .with_detail_field("remediation_flags", flags_json);
            envelopes.push(RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope,
            });
        }
        Ok(envelopes)
    }

    fn resolve_periodic_recall_probe_subjects(
        &self,
        probe_store: &RecallProbeStore,
        memory_db_path: &std::path::Path,
        max_subjects: usize,
        now: u64,
    ) -> Result<(Vec<RecallProbeSubject>, usize, bool)> {
        let live_subjects = build_probe_subjects(
            &self.archive_store,
            &self.vault_store,
            Some(memory_db_path),
            max_subjects,
        )?;
        let baseline_targets = probe_store.baseline_targets(PERIODIC_RECALL_PROBE_COHORT)?;
        if baseline_targets.is_empty() {
            let seeded_subjects = live_subjects
                .into_iter()
                .take(max_subjects)
                .collect::<Vec<_>>();
            self.persist_recall_probe_baseline_targets(probe_store, &seeded_subjects, now)?;
            return Ok((seeded_subjects, 0, false));
        }

        let scoped_targets = baseline_targets
            .into_iter()
            .take(max_subjects)
            .collect::<Vec<_>>();
        let mut live_by_target = live_subjects
            .iter()
            .cloned()
            .map(|subject| ((subject.target_kind, subject.target_id.clone()), subject))
            .collect::<HashMap<_, _>>();

        let mut selected = Vec::new();
        let mut missing_count = 0usize;
        for target in &scoped_targets {
            let key = (target.target_kind, target.target_id.clone());
            if let Some(subject) = live_by_target.remove(&key) {
                selected.push(subject);
            } else {
                missing_count += 1;
            }
        }

        if selected.is_empty() {
            let reseeded_subjects = live_subjects
                .into_iter()
                .take(max_subjects)
                .collect::<Vec<_>>();
            self.persist_recall_probe_baseline_targets(probe_store, &reseeded_subjects, now)?;
            return Ok((reseeded_subjects, missing_count, true));
        }

        Ok((selected, missing_count, false))
    }

    fn persist_recall_probe_baseline_targets(
        &self,
        probe_store: &RecallProbeStore,
        subjects: &[RecallProbeSubject],
        now: u64,
    ) -> Result<()> {
        let targets = subjects
            .iter()
            .enumerate()
            .map(|(position, subject)| RecallProbeBaselineTarget {
                cohort: PERIODIC_RECALL_PROBE_COHORT.to_string(),
                position,
                target_kind: subject.target_kind,
                target_id: subject.target_id.clone(),
                created_at: now,
            })
            .collect::<Vec<_>>();
        probe_store.replace_baseline_targets(PERIODIC_RECALL_PROBE_COHORT, &targets)?;
        Ok(())
    }

    fn handle_recall_probe_status(
        &self,
        room_id: &str,
        run_id: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let Some(run) = probe_store.run(&run_id)? else {
            let event = MatrixEventEnvelope::state(
                "recall.probe.missing",
                now,
                "Recall probe run not found",
            )
            .with_detail_field("run_id", run_id);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: event,
            }]);
        };

        let results = probe_store.results_for_run(&run_id)?;
        let mut target_matches = BTreeMap::<(String, String), bool>::new();
        for result in &results {
            let key = (
                result.target_kind.as_str().to_string(),
                result.target_id.clone(),
            );
            target_matches
                .entry(key)
                .and_modify(|matched| *matched |= result.matched)
                .or_insert(result.matched);
        }
        let unmatched_targets = target_matches
            .iter()
            .filter(|(_, matched)| !**matched)
            .map(|((kind, id), _)| format!("{kind}:{id}"))
            .take(10)
            .collect::<Vec<_>>();
        let body = format!(
            "Recall probe run {}: {}/{} subjects reachable",
            run.id, run.matched_count, run.subject_count
        );
        let unmatched_targets_json =
            serde_json::to_string(&unmatched_targets).unwrap_or_else(|_| "[]".to_string());
        let event = MatrixEventEnvelope::state("recall.probe.status", now, &body)
            .with_detail_field("run_id", run.id.as_str())
            .with_detail_field("started_at", run.started_at)
            .with_detail_field("finished_at", run.finished_at)
            .with_detail_field("top_k", run.top_k)
            .with_detail_field("subject_count", run.subject_count)
            .with_detail_field("matched_subject_count", run.matched_count)
            .with_detail_field("query_count", results.len())
            .with_detail_field(
                "unmatched_subject_count",
                run.subject_count.saturating_sub(run.matched_count),
            )
            .with_detail_field("unmatched_target_ids", unmatched_targets_json);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    fn handle_recall_probe_summary(
        &self,
        room_id: &str,
        target_kind: RecallProbeTargetKind,
        target_id: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let Some(summary) = probe_store.summary_for(target_kind, &target_id)? else {
            let event = MatrixEventEnvelope::state(
                "recall.probe.summary.missing",
                now,
                "Recall probe summary not found",
            )
            .with_detail_field("target_kind", target_kind.as_str())
            .with_detail_field("target_id", target_id.as_str());
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: event,
            }]);
        };

        let latest_results = probe_store.results_for_target_in_run(
            &summary.last_run_id,
            summary.target_kind,
            &summary.target_id,
        )?;
        let remediation_flags = Self::recall_probe_flag_labels_from_results(&latest_results);
        let failed_queries = latest_results
            .iter()
            .filter(|result| !result.matched)
            .map(|result| result.query.clone())
            .take(3)
            .collect::<Vec<_>>();
        let matched_query_count = latest_results
            .iter()
            .filter(|result| result.matched)
            .count();

        let body = format!(
            "Recall probe summary for {}:{} is {}",
            summary.target_kind.as_str(),
            summary.target_id,
            summary.status.as_str()
        );
        let event = MatrixEventEnvelope::state("recall.probe.summary", now, &body)
            .with_detail_field("target_kind", summary.target_kind.as_str())
            .with_detail_field("target_id", summary.target_id.as_str())
            .with_detail_field("last_checked_at", summary.last_checked_at)
            .with_detail_field("last_run_id", summary.last_run_id.as_str())
            .with_detail_field("success_rate", summary.success_rate)
            .with_detail_field("consecutive_failures", summary.consecutive_failures)
            .with_detail_field("status", summary.status.as_str())
            .with_detail_field("query_count", latest_results.len())
            .with_detail_field("matched_query_count", matched_query_count)
            .with_detail_field(
                "remediation_flags",
                serde_json::to_string(&remediation_flags).unwrap_or_else(|_| "[]".to_string()),
            )
            .with_detail_field(
                "failed_queries",
                serde_json::to_string(&failed_queries).unwrap_or_else(|_| "[]".to_string()),
            );
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    fn handle_recall_probe_health(
        &self,
        room_id: &str,
        limit: usize,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let summaries = probe_store.list_summaries(limit)?;
        let body = if summaries.is_empty() {
            "No recall probe summaries recorded yet".to_string()
        } else {
            format!("Recall probe health: {} tracked targets", summaries.len())
        };
        let summary_rows = summaries
            .iter()
            .map(|summary| {
                let latest_results = probe_store
                    .results_for_target_in_run(
                        &summary.last_run_id,
                        summary.target_kind,
                        &summary.target_id,
                    )
                    .unwrap_or_default();
                let remediation_flags =
                    Self::recall_probe_flag_labels_from_results(&latest_results);
                let failed_queries = latest_results
                    .iter()
                    .filter(|result| !result.matched)
                    .map(|result| result.query.clone())
                    .take(3)
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "target_kind": summary.target_kind.as_str(),
                    "target_id": summary.target_id,
                    "status": summary.status.as_str(),
                    "success_rate": summary.success_rate,
                    "consecutive_failures": summary.consecutive_failures,
                    "last_checked_at": summary.last_checked_at,
                    "last_run_id": summary.last_run_id,
                    "remediation_flags": remediation_flags,
                    "failed_queries": failed_queries,
                })
            })
            .collect::<Vec<_>>();
        let status_counts = summaries.iter().fold(
            BTreeMap::<&'static str, usize>::new(),
            |mut counts, summary| {
                *counts.entry(summary.status.as_str()).or_insert(0) += 1;
                counts
            },
        );
        let event = MatrixEventEnvelope::state("recall.probe.health", now, &body)
            .with_detail_field("limit", limit)
            .with_detail_field("tracked_target_count", summaries.len())
            .with_detail_field(
                "status_counts",
                serde_json::to_string(&status_counts).unwrap_or_else(|_| "{}".to_string()),
            )
            .with_detail_field(
                "summaries",
                serde_json::to_string(&summary_rows).unwrap_or_else(|_| "[]".to_string()),
            );
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    fn handle_recall_probe_regressions(
        &self,
        room_id: &str,
        run_id: String,
        limit: usize,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let probe_store = self.open_recall_probe_store()?;
        let Some(current_run) = probe_store.run(&run_id)? else {
            let event = MatrixEventEnvelope::state(
                "recall.probe.missing",
                now,
                "Recall probe run not found",
            )
            .with_detail_field("run_id", run_id);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: event,
            }]);
        };

        let Some(baseline_run_id) = probe_store.previous_completed_run_id(&run_id)? else {
            let event = MatrixEventEnvelope::state(
                "recall.probe.regressions.missing_baseline",
                now,
                "No previous completed recall probe run available for comparison",
            )
            .with_detail_field("current_run_id", run_id.as_str())
            .with_detail_field("limit", limit);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: event,
            }]);
        };

        let baseline_outcomes = probe_store.outcomes_for_run(&baseline_run_id)?;
        let current_outcomes = probe_store.outcomes_for_run(&run_id)?;
        let baseline_map = baseline_outcomes
            .into_iter()
            .map(|outcome| {
                (
                    (
                        outcome.target_kind.as_str().to_string(),
                        outcome.target_id.clone(),
                    ),
                    outcome,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let current_map = current_outcomes
            .into_iter()
            .map(|outcome| {
                (
                    (
                        outcome.target_kind.as_str().to_string(),
                        outcome.target_id.clone(),
                    ),
                    outcome,
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut regressions = Vec::new();
        let mut improvements = Vec::new();
        let mut stable_unreachable_count = 0usize;
        let mut stable_reachable_count = 0usize;
        let mut newly_tracked_count = 0usize;
        let mut dropped_target_count = 0usize;

        for (key, current) in &current_map {
            let Some(previous) = baseline_map.get(key) else {
                newly_tracked_count += 1;
                continue;
            };
            if previous.matched && !current.matched {
                regressions.push(Self::recall_probe_outcome_delta(previous, current));
            } else if !previous.matched && current.matched {
                improvements.push(Self::recall_probe_outcome_delta(previous, current));
            } else if current.matched {
                stable_reachable_count += 1;
            } else {
                stable_unreachable_count += 1;
            }
        }

        for key in baseline_map.keys() {
            if !current_map.contains_key(key) {
                dropped_target_count += 1;
            }
        }

        let body = format!(
            "Recall probe regressions for {} vs {}: {} regressions, {} improvements",
            run_id,
            baseline_run_id,
            regressions.len(),
            improvements.len()
        );
        let event = MatrixEventEnvelope::state("recall.probe.regressions", now, &body)
            .with_detail_field("current_run_id", run_id.as_str())
            .with_detail_field("baseline_run_id", baseline_run_id.as_str())
            .with_detail_field("limit", limit)
            .with_detail_field("current_subject_count", current_run.subject_count)
            .with_detail_field("baseline_subject_count", baseline_map.len())
            .with_detail_field("regression_count", regressions.len())
            .with_detail_field("improvement_count", improvements.len())
            .with_detail_field("stable_reachable_count", stable_reachable_count)
            .with_detail_field("stable_unreachable_count", stable_unreachable_count)
            .with_detail_field("newly_tracked_count", newly_tracked_count)
            .with_detail_field("dropped_target_count", dropped_target_count)
            .with_detail_field(
                "regressions",
                serde_json::to_string(&regressions.into_iter().take(limit).collect::<Vec<_>>())
                    .unwrap_or_else(|_| "[]".to_string()),
            )
            .with_detail_field(
                "improvements",
                serde_json::to_string(&improvements.into_iter().take(limit).collect::<Vec<_>>())
                    .unwrap_or_else(|_| "[]".to_string()),
            );
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    fn recall_probe_flag_label(flag: RecallRemediationFlag) -> &'static str {
        match flag {
            RecallRemediationFlag::ReembedCandidate => "reembed_candidate",
            RecallRemediationFlag::KeywordAugmentationCandidate => "keyword_augmentation_candidate",
            RecallRemediationFlag::DerivedEdgeCandidate => "derived_edge_candidate",
            RecallRemediationFlag::ManualReviewRequired => "manual_review_required",
        }
    }

    fn format_recall_probe_flags(flags: &[RecallRemediationFlag]) -> String {
        let mut labels = flags
            .iter()
            .map(|flag| match flag {
                RecallRemediationFlag::ReembedCandidate => "re-embed the target",
                RecallRemediationFlag::KeywordAugmentationCandidate => "augment retrieval keywords",
                RecallRemediationFlag::DerivedEdgeCandidate => {
                    "inspect retrieval-only derived-edge candidates"
                }
                RecallRemediationFlag::ManualReviewRequired => "perform manual memory review",
            })
            .collect::<Vec<_>>();
        labels.sort();
        labels.dedup();
        labels.join(", ")
    }

    fn recall_probe_flag_labels_from_results(
        results: &[symbiotic_memory::recall_probes::RecallProbeResult],
    ) -> Vec<String> {
        let mut labels = BTreeSet::new();
        for result in results {
            for flag in &result.remediation_flags {
                labels.insert(Self::recall_probe_flag_label(*flag).to_string());
            }
        }
        labels.into_iter().collect()
    }

    fn recall_probe_outcome_delta(
        previous: &RecallProbeRunOutcome,
        current: &RecallProbeRunOutcome,
    ) -> serde_json::Value {
        serde_json::json!({
            "target_kind": current.target_kind.as_str(),
            "target_id": current.target_id,
            "previous_matched": previous.matched,
            "current_matched": current.matched,
            "previous_best_rank": previous.best_rank,
            "current_best_rank": current.best_rank,
        })
    }

    fn handle_credential_approve(
        &self,
        room_id: &str,
        sender: &str,
        request_id: String,
        remember_for_secs: Option<u64>,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).approve_room(
            room_id,
            sender,
            request_id,
            remember_for_secs,
            now,
        )
    }

    fn handle_credential_deny(
        &self,
        room_id: &str,
        _sender: &str,
        request_id: String,
        reason: Option<String>,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).deny_room(room_id, request_id, reason, now)
    }

    fn handle_credential_respond(
        &self,
        room_id: &str,
        _sender: &str,
        request_id: String,
        value: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).respond_room(room_id, request_id, value, now)
    }

    fn handle_credential_approval_policy_list(
        &self,
        room_id: &str,
        include_inactive: bool,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).list_policies(room_id, include_inactive, now)
    }

    fn handle_credential_approval_policy_revoke(
        &self,
        room_id: &str,
        policy_id: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        AuthOrchestrator::new(self).revoke_policy(room_id, policy_id, now)
    }

    // ── Self-improvement proposal handlers ───────────────────────────────

    /// Approve a pending proposal and convert it to a goal in the deliberation
    /// pipeline.
    fn handle_proposal_approve(
        &self,
        proposal_id: &str,
        room_id: &str,
        sender: &str,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let Some(proposal) = self.proposal_store.take(proposal_id) else {
            let event = MatrixEventEnvelope::new(
                Kind::Message,
                Status::Fail,
                now,
                "Proposal not found or already handled",
            )
            .with_detail_field("proposal_id", proposal_id);
            return Ok(vec![RoutedMatrixEnvelope {
                room_id: room_id.to_string(),
                envelope: event,
            }]);
        };

        tracing::info!(
            proposal_id = proposal_id,
            source = proposal.source.as_str(),
            "proposal approved — converting to goal"
        );

        // Format the proposal as a goal description and run the pipeline.
        let goal_description = proposals::format_proposal_as_goal(&proposal);
        let goal_room = proposal.thread_id.as_deref().unwrap_or(room_id);

        let goal_event =
            self.process_goal_through_pipeline(&goal_description, goal_room, sender, now)?;

        let ts = symbiotic_queue::now_unix();
        let envelope = goal_event.to_envelope(ts);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: goal_room.to_string(),
            envelope,
        }])
    }

    /// Dismiss a pending proposal without executing it.
    fn handle_proposal_dismiss(
        &self,
        proposal_id: &str,
        room_id: &str,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let existed = self.proposal_store.take(proposal_id).is_some();
        let body = if existed {
            tracing::info!(proposal_id = proposal_id, "proposal dismissed");
            "Proposal dismissed"
        } else {
            "Proposal not found or already handled"
        };
        let status = if existed {
            Status::Success
        } else {
            Status::Fail
        };
        let event = MatrixEventEnvelope::new(Kind::Message, status, now, body)
            .with_detail_field("proposal_id", proposal_id);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }
}

fn serialize_mutations(mutations: &[VaultMutation]) -> Vec<serde_json::Value> {
    mutations
        .iter()
        .map(|mutation| {
            let kind = match &mutation.kind {
                MutationKind::FactAdded(fact) => serde_json::json!({
                    "kind": "fact_added",
                    "fact": fact,
                }),
                MutationKind::FactArchived { fact, reason } => serde_json::json!({
                    "kind": "fact_archived",
                    "fact": fact,
                    "reason": reason,
                }),
                MutationKind::RelationshipAdded { rel_type, target } => serde_json::json!({
                    "kind": "relationship_added",
                    "rel_type": rel_type,
                    "target": target,
                }),
                MutationKind::RelationshipRemoved {
                    rel_type,
                    target,
                    reason,
                } => serde_json::json!({
                    "kind": "relationship_removed",
                    "rel_type": rel_type,
                    "target": target,
                    "reason": reason,
                }),
                MutationKind::RelationshipReplaced {
                    rel_type,
                    old_target,
                    new_target,
                    reason,
                } => serde_json::json!({
                    "kind": "relationship_replaced",
                    "rel_type": rel_type,
                    "old_target": old_target,
                    "new_target": new_target,
                    "reason": reason,
                }),
                MutationKind::EntityCreated => serde_json::json!({
                    "kind": "entity_created",
                }),
            };
            serde_json::json!({
                "file_path": mutation.file_path,
                "entity_id": mutation.entity_id,
                "mutation": kind,
            })
        })
        .collect()
}
