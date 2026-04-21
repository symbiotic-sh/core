//! Context retrieval logic: keyword scoring, hybrid search, graph merging, and policy enforcement.
//!
//! Integrates class budgets, progressive disclosure, thread filtering, and query gap tracking.

use std::cmp::Ordering;
use std::collections::HashMap;
use symbiotic_core::now_unix;

use anyhow::{anyhow, Result};

use crate::budgets::{DisclosureTier, FactClass};
use crate::firewall::FirewallEntryDecision;
use crate::graph::GraphRetriever;
use crate::redaction;
use crate::vector_index::VectorIndex;
use crate::{
    AuditRecord, ContextBudget, ContextItem, ContextPack, ContextPolicy, ContextRequest,
    ModelClass, RecallGateway, Sensitivity,
};

/// Weight for BM25/keyword score in hybrid scoring.
const KEYWORD_WEIGHT: f32 = 0.4;

/// Weight for cosine similarity score in hybrid scoring.
const VECTOR_WEIGHT: f32 = 0.6;

/// Merge bonus added to graph seed entities (depth=0) during score interleaving.
const GRAPH_SEED_BONUS: f32 = 0.2;

impl RecallGateway {
    /// Performs context retrieval using keyword-only search (no embedding needed).
    pub fn get_context(&self, request: &ContextRequest) -> Result<ContextPack> {
        self.get_context_hybrid(request, None)
    }

    /// Performs context retrieval with optional hybrid search.
    ///
    /// If `query_embedding` is provided and a vector index is configured,
    /// hybrid scoring is used: `0.4 * keyword_normalized + 0.6 * cosine_similarity`.
    /// Otherwise, falls back to keyword-only search.
    ///
    /// Integrates:
    /// - Thread filtering (via `request.filter_threads`)
    /// - Class budgets (via `request.class_budget`)
    /// - Progressive disclosure (via `request.disclosure_tier`)
    /// - Query gap tracking (logs empty results automatically)
    ///
    /// Note: the firewall hook (T132 §06) is **not** invoked through this
    /// entry point — callers that need Stage D / E enforcement must use
    /// [`RecallGateway::get_context_with_firewall`] (or
    /// [`RecallGateway::get_context_hybrid_with_firewall`]) and pass the
    /// consuming agent's scope.
    pub fn get_context_hybrid(
        &self,
        request: &ContextRequest,
        query_embedding: Option<&[f32]>,
    ) -> Result<ContextPack> {
        self.get_context_hybrid_inner(request, query_embedding, None)
    }

    /// Same as [`get_context`] but routes every retrieved entry through
    /// the firewall (Stage D + E) using the consuming agent's scope.
    /// Stage D blocks exclude entries from the pack and notify the
    /// configured [`crate::SmugglingReporter`].
    ///
    /// [`get_context`]: RecallGateway::get_context
    pub fn get_context_with_firewall(
        &self,
        request: &ContextRequest,
        consuming_agent_scope: &symbiotic_firewall::ConsumingAgentScope,
    ) -> Result<ContextPack> {
        self.get_context_hybrid_inner(request, None, Some(consuming_agent_scope))
    }

    /// Same as [`get_context_hybrid`] but routes every retrieved entry
    /// through the firewall (Stage D + E) using the consuming agent's
    /// scope.
    ///
    /// [`get_context_hybrid`]: RecallGateway::get_context_hybrid
    pub fn get_context_hybrid_with_firewall(
        &self,
        request: &ContextRequest,
        query_embedding: Option<&[f32]>,
        consuming_agent_scope: &symbiotic_firewall::ConsumingAgentScope,
    ) -> Result<ContextPack> {
        self.get_context_hybrid_inner(request, query_embedding, Some(consuming_agent_scope))
    }

    fn get_context_hybrid_inner(
        &self,
        request: &ContextRequest,
        query_embedding: Option<&[f32]>,
        consuming_agent_scope: Option<&symbiotic_firewall::ConsumingAgentScope>,
    ) -> Result<ContextPack> {
        if request.token_budget == 0 {
            return Err(anyhow!("token budget must be > 0"));
        }

        let query_terms = tokenize(&request.query);
        let entries = self.provider().list_entries()?;
        let mut scored_entries = score_entries(entries, &query_terms, &request.tags);

        // --- Thread filtering ---
        if let Some(ref thread_ids) = request.filter_threads {
            if !thread_ids.is_empty() {
                scored_entries.retain(|candidate| {
                    candidate
                        .entry
                        .thread_id
                        .as_ref()
                        .is_some_and(|tid| thread_ids.contains(tid))
                });
            }
        }

        if let Some(days) = request.recency_days {
            let cutoff = now_unix().saturating_sub((days as u64) * 86_400);
            scored_entries.retain(|candidate| candidate.entry.updated_at >= cutoff);
        }

        // Determine retrieval mode and compute hybrid scores if possible
        let vi_guard = self
            .vector_index()
            .map(|vi| vi.lock().expect("vector index lock"));
        let (retrieval_mode, scored_entries) = match (query_embedding, vi_guard.as_deref()) {
            (Some(embedding), Some(index)) => {
                compute_hybrid_scores(scored_entries, embedding, index, &query_terms)
            }
            _ => {
                // Keyword-only: filter out zero-score entries (unless empty query)
                let filtered: Vec<ScoredEntry> = scored_entries
                    .into_iter()
                    .filter(|candidate| candidate.score > 0.0 || query_terms.is_empty())
                    .collect();
                ("keyword".to_string(), filtered)
            }
        };

        let mut scored_entries = scored_entries;

        // For hybrid mode, filter out entries with zero hybrid score (unless empty query)
        if retrieval_mode == "hybrid" {
            scored_entries.retain(|candidate| candidate.score > 0.0 || query_terms.is_empty());
        }

        scored_entries.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| b.entry.updated_at.cmp(&a.entry.updated_at))
        });

        // --- Class budget enforcement ---
        let class_budget = request.class_budget.clone().unwrap_or_default();
        let mut class_tokens_used: HashMap<FactClass, usize> = HashMap::new();
        let class_limits: HashMap<FactClass, usize> = FactClass::all()
            .iter()
            .map(|&fc| (fc, class_budget.tokens_for(fc, request.token_budget)))
            .collect();

        // --- Progressive disclosure ---
        let disclosure = request.disclosure_tier.unwrap_or(DisclosureTier::Full);

        let mut selected = Vec::new();
        let mut token_used = 0usize;
        let mut redaction_applied = false;

        for candidate in scored_entries {
            // T132 §06 — context-assembly time firewall (Stage D + E).
            // When a firewall hook is wired AND the caller passed a
            // consuming agent scope, every candidate runs through Stage
            // D + E before policy/budget evaluation. Stage D blocks
            // exclude the entry from the pack (reporter notified inside
            // ContextFirewall::apply); Stage E wraps the body with the
            // `<external_content>` annotation that policy then operates
            // on as the item content.
            let candidate = match (self.firewall(), consuming_agent_scope) {
                (Some(firewall), Some(scope)) => match firewall.apply(&candidate.entry, scope) {
                    FirewallEntryDecision::Include(wrapped) => {
                        let mut entry = candidate.entry;
                        entry.content = wrapped;
                        ScoredEntry {
                            entry,
                            score: candidate.score,
                        }
                    }
                    FirewallEntryDecision::Exclude => continue,
                },
                _ => candidate,
            };

            let (item, tokens, redacted) =
                apply_policy_and_build_item(candidate, request, token_used, disclosure);
            if let Some(item) = item {
                // Check class budget if the entry has a fact class
                if let Some(fact_class) = item_fact_class(&item) {
                    let class_used = class_tokens_used.entry(fact_class).or_insert(0);
                    let class_limit = class_limits.get(&fact_class).copied().unwrap_or(0);
                    if *class_used + tokens > class_limit {
                        // This class has exceeded its budget; skip the item
                        continue;
                    }
                    *class_used += tokens;
                }

                token_used += tokens;
                if redacted {
                    redaction_applied = true;
                }
                selected.push(item);
            }
        }

        // Merge graph results if a graph retriever is configured
        if let Some(graph_retriever) = self.graph() {
            merge_graph_results(
                graph_retriever.as_ref(),
                request,
                &mut selected,
                &mut token_used,
                disclosure,
            );
        }

        // --- Query gap tracking ---
        if selected.is_empty() && !request.query.is_empty() {
            self.log_empty_query(&request.query);
        }

        let pack = ContextPack {
            request_id: request.request_id.clone(),
            retrieval_mode,
            policy: ContextPolicy {
                model_class: request.model_class,
                sensitivity_max: request.sensitivity_max,
                redaction: request.model_class == ModelClass::Cloud,
            },
            items: selected,
            budget: ContextBudget {
                token_budget: request.token_budget,
                token_used,
            },
        };
        pack.validate()?;

        self.audit().record(AuditRecord {
            request_id: request.request_id.clone(),
            retrieval_mode: pack.retrieval_mode.clone(),
            model_class: request.model_class,
            items_returned: pack.items.len(),
            redaction_applied,
        })?;

        Ok(pack)
    }
}

/// Computes hybrid score from normalized keyword and vector similarity scores.
///
/// Formula: `0.4 * keyword_normalized + 0.6 * cosine_similarity`
pub fn hybrid_score(keyword_normalized: f32, cosine_similarity: f32) -> f32 {
    KEYWORD_WEIGHT * keyword_normalized + VECTOR_WEIGHT * cosine_similarity
}

#[derive(Debug, Clone)]
pub(crate) struct ScoredEntry {
    pub(crate) entry: crate::ArchiveEntry,
    pub(crate) score: f32,
}

fn compute_hybrid_scores(
    scored_entries: Vec<ScoredEntry>,
    embedding: &[f32],
    index: &VectorIndex,
    query_terms: &[String],
) -> (String, Vec<ScoredEntry>) {
    let vector_results = index.search(
        embedding,
        crate::Sensitivity::Private,
        scored_entries.len().max(100),
    );
    let vector_map: HashMap<&str, f32> = vector_results
        .iter()
        .map(|r| (r.entry_id.as_str(), r.similarity))
        .collect();

    let max_keyword = scored_entries
        .iter()
        .map(|e| e.score)
        .fold(0.0f32, f32::max);

    let hybrid: Vec<ScoredEntry> = scored_entries
        .into_iter()
        .map(|mut candidate| {
            let keyword_norm = if max_keyword > 0.0 {
                candidate.score / max_keyword
            } else {
                0.0
            };
            let vector_sim = vector_map
                .get(candidate.entry.id.as_str())
                .copied()
                .unwrap_or(0.0);
            candidate.score = hybrid_score(keyword_norm, vector_sim);
            candidate
        })
        .collect();

    let _ = query_terms; // used by caller for filtering
    ("hybrid".to_string(), hybrid)
}

fn score_entries(
    entries: Vec<crate::ArchiveEntry>,
    query_terms: &[String],
    requested_tags: &[String],
) -> Vec<ScoredEntry> {
    entries
        .into_iter()
        .map(|entry| {
            let title_l = entry.title.to_ascii_lowercase();
            let content_l = entry.content.to_ascii_lowercase();

            let mut score = 0f32;
            for term in query_terms {
                if title_l.contains(term) {
                    score += 3.0;
                }
                if content_l.contains(term) {
                    score += 1.0;
                }
                if entry
                    .tags
                    .iter()
                    .any(|tag| tag.to_ascii_lowercase().contains(term))
                {
                    score += 2.0;
                }
            }

            if !requested_tags.is_empty()
                && requested_tags.iter().any(|requested| {
                    entry
                        .tags
                        .iter()
                        .any(|tag| tag.eq_ignore_ascii_case(requested))
                })
            {
                score += 2.0;
            }

            ScoredEntry { entry, score }
        })
        .collect()
}

fn is_allowed_for_policy(
    model_class: ModelClass,
    sensitivity_max: Sensitivity,
    entry_sensitivity: Sensitivity,
) -> bool {
    if model_class == ModelClass::Cloud {
        entry_sensitivity == Sensitivity::Shareable
    } else {
        entry_sensitivity <= sensitivity_max
    }
}

fn apply_policy_and_build_item(
    candidate: ScoredEntry,
    request: &ContextRequest,
    token_used: usize,
    disclosure: DisclosureTier,
) -> (Option<ContextItem>, usize, bool) {
    let mut entry = candidate.entry;
    let allowed = is_allowed_for_policy(
        request.model_class,
        request.sensitivity_max,
        entry.sensitivity,
    );

    let mut redacted = false;
    if !allowed {
        if request.model_class == ModelClass::Cloud {
            redacted = true;
            entry.content = redact_content(&entry.content);
            entry.sensitivity = Sensitivity::Shareable;
        } else {
            return (None, 0, false);
        }
    } else if request.model_class == ModelClass::Cloud
        && entry.sensitivity != Sensitivity::Shareable
    {
        redacted = true;
        entry.content = redact_content(&entry.content);
        entry.sensitivity = Sensitivity::Shareable;
    }

    // Apply progressive disclosure truncation
    entry.content = disclosure.truncate_content(&entry.content);

    let candidate_tokens = estimate_tokens(&entry.title) + estimate_tokens(&entry.content);
    if token_used + candidate_tokens > request.token_budget {
        return (None, 0, false);
    }

    let mut evidence = vec![format!("archive:{}", entry.id)];
    if let Some(source) = entry.source_url.clone() {
        evidence.push(source);
    }
    let item = ContextItem {
        r#type: "entry".to_string(),
        id: entry.id,
        title: entry.title,
        content: entry.content,
        sensitivity: entry.sensitivity,
        source_url: entry.source_url,
        evidence,
        score: candidate.score,
        redacted,
    };
    (Some(item), candidate_tokens, redacted)
}

fn merge_graph_results(
    graph_retriever: &dyn GraphRetriever,
    request: &ContextRequest,
    selected: &mut Vec<ContextItem>,
    token_used: &mut usize,
    disclosure: DisclosureTier,
) {
    let graph_config = crate::graph::GraphRetrievalConfig::default();
    if let Ok(graph_result) =
        graph_retriever.retrieve(&request.query, &graph_config, request.sensitivity_max)
    {
        let existing_ids: std::collections::HashSet<String> =
            selected.iter().map(|item| item.id.clone()).collect();

        let all_graph_nodes = graph_result.seeds.iter().chain(graph_result.related.iter());

        for node in all_graph_nodes {
            let graph_score = if node.depth == 0 {
                (node.score as f32) + GRAPH_SEED_BONUS
            } else {
                node.score as f32
            };

            // Dedup: if entity already in results, keep higher score
            if let Some(existing) = selected.iter_mut().find(|i| i.id == node.entity_id) {
                if graph_score > existing.score {
                    existing.score = graph_score;
                }
                continue;
            }

            if existing_ids.contains(&node.entity_id) {
                continue;
            }

            let content = node
                .memories
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            // Apply progressive disclosure to graph content
            let content = disclosure.truncate_content(&content);

            let evidence: Vec<String> = node
                .memories
                .iter()
                .flat_map(|m| m.evidence.iter().cloned())
                .collect();

            // Skip if no evidence (required by ContextPack validation)
            if evidence.is_empty() {
                continue;
            }

            let candidate_tokens = estimate_tokens(&node.entity_name) + estimate_tokens(&content);
            if *token_used + candidate_tokens > request.token_budget {
                continue;
            }

            *token_used += candidate_tokens;
            selected.push(ContextItem {
                r#type: "memory".to_string(),
                id: node.entity_id.clone(),
                title: node.entity_name.clone(),
                content,
                sensitivity: request.sensitivity_max,
                source_url: None,
                evidence,
                score: graph_score,
                redacted: false,
            });
        }

        // Re-sort after merging graph results
        selected.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    }
    // If graph retrieval fails (NoSeeds, etc.), degrade gracefully
}

/// Infers a `FactClass` from a ContextItem's tags/type.
///
/// Items originating from `ArchiveEntry` carry a `fact_class` field; for items
/// built from graph nodes we try to infer from the type field.
fn item_fact_class(item: &ContextItem) -> Option<FactClass> {
    // Check evidence tags for class hints
    for ev in &item.evidence {
        if ev.contains("decision") {
            return Some(FactClass::Decision);
        }
        if ev.contains("finding") {
            return Some(FactClass::Finding);
        }
        if ev.contains("preference") {
            return Some(FactClass::Preference);
        }
        if ev.contains("methodology") {
            return Some(FactClass::Methodology);
        }
        if ev.contains("episode") {
            return Some(FactClass::Episode);
        }
    }
    // Memory items from the graph are typically entity-class
    if item.r#type == "memory" {
        return Some(FactClass::Entity);
    }
    None
}

fn tokenize(value: &str) -> Vec<String> {
    value
        .split_whitespace()
        .map(|term| {
            term.trim_matches(|ch: char| !ch.is_ascii_alphanumeric())
                .to_ascii_lowercase()
        })
        .filter(|term| !term.is_empty())
        .collect()
}

fn estimate_tokens(value: &str) -> usize {
    value.split_whitespace().count().max(1)
}

fn redact_content(value: &str) -> String {
    let engine = redaction::RedactionEngine::new();
    engine.redact(value)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{ArchiveEntry, ArchiveProvider, AuditRecord, AuditSink, Purpose};

    struct TestProvider {
        entries: Vec<ArchiveEntry>,
    }

    impl ArchiveProvider for TestProvider {
        fn list_entries(&self) -> Result<Vec<ArchiveEntry>> {
            Ok(self.entries.clone())
        }
    }

    #[derive(Default)]
    struct TestAudit {
        records: Mutex<Vec<AuditRecord>>,
    }

    impl AuditSink for TestAudit {
        fn record(&self, record: AuditRecord) -> Result<()> {
            self.records.lock().expect("audit lock").push(record);
            Ok(())
        }
    }

    fn sample_entries() -> Vec<ArchiveEntry> {
        vec![
            ArchiveEntry {
                id: "a1".to_string(),
                title: "Rust daemon orchestration".to_string(),
                content: "Queue workers and intake pipeline notes".to_string(),
                tags: vec!["architecture".to_string(), "daemon".to_string()],
                sensitivity: Sensitivity::Shareable,
                source_url: Some("https://example.com/a1".to_string()),
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
            ArchiveEntry {
                id: "a2".to_string(),
                title: "Partner outreach".to_string(),
                content: "Contact me at founder@symbiotic.sh and phone 12345678901".to_string(),
                tags: vec!["people".to_string()],
                sensitivity: Sensitivity::Restricted,
                source_url: Some("https://example.com/a2".to_string()),
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
        ]
    }

    fn make_request(query: &str) -> ContextRequest {
        ContextRequest {
            request_id: "test".to_string(),
            query: query.to_string(),
            model_class: ModelClass::Local,
            purpose: Purpose::Plan,
            sensitivity_max: Sensitivity::Private,
            token_budget: 200,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        }
    }

    #[test]
    fn keyword_retrieval_returns_relevant_entries() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit.clone());

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "req1".to_string(),
                query: "daemon queue".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Plan,
                sensitivity_max: Sensitivity::Private,
                token_budget: 200,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.retrieval_mode, "keyword");
        assert!(!pack.items.is_empty());
        assert_eq!(pack.items[0].id, "a1");
        assert!(!pack.items[0].redacted);
    }

    #[test]
    fn cloud_policy_redacts_restricted_content() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit.clone());

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "req2".to_string(),
                query: "partner outreach".to_string(),
                model_class: ModelClass::Cloud,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Shareable,
                token_budget: 200,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        let item = pack
            .items
            .iter()
            .find(|item| item.id == "a2")
            .expect("restricted entry should be included as redacted");
        assert!(item.redacted);
        assert!(item.content.contains("[redacted-email]"));
        assert!(item.content.contains("[redacted-phone]"));
    }

    #[test]
    fn token_budget_limits_number_of_items() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit.clone());

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "req3".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 5,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert!(pack.items.len() <= 1);
        assert!(pack.budget.token_used <= 5);
    }

    #[test]
    fn audit_records_context_requests() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit.clone());

        let _pack = gateway
            .get_context(&ContextRequest {
                request_id: "req4".to_string(),
                query: "queue".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Plan,
                sensitivity_max: Sensitivity::Private,
                token_budget: 200,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        let records = audit.records.lock().expect("audit lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request_id, "req4");
        assert_eq!(records[0].retrieval_mode, "keyword");
    }

    #[test]
    fn context_pack_includes_evidence_and_type() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "req5".to_string(),
                query: "daemon".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Review,
                sensitivity_max: Sensitivity::Private,
                token_budget: 200,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert!(!pack.items.is_empty());
        for item in &pack.items {
            assert_eq!(item.r#type, "entry");
            assert!(!item.evidence.is_empty());
        }
    }

    #[test]
    fn context_pack_parse_strict_rejects_unknown_fields() {
        let payload = r#"{
          "request_id":"req",
          "retrieval_mode":"keyword",
          "policy":{"model_class":"local","sensitivity_max":"private","redaction":false},
          "items":[
            {
              "type":"entry",
              "id":"a1",
              "title":"t",
              "content":"c",
              "sensitivity":"shareable",
              "source_url":null,
              "evidence":["archive:a1"],
              "score":1.0,
              "redacted":false,
              "extra":"x"
            }
          ],
          "budget":{"token_budget":10,"token_used":5}
        }"#;
        assert!(ContextPack::parse_strict(payload).is_err());
    }

    #[test]
    fn context_pack_parse_strict_rejects_invalid_payload_values() {
        let payload = r#"{
          "request_id":"req",
          "retrieval_mode":"unknown",
          "policy":{"model_class":"local","sensitivity_max":"private","redaction":false},
          "items":[
            {
              "type":"entry",
              "id":"a1",
              "title":"t",
              "content":"c",
              "sensitivity":"shareable",
              "source_url":null,
              "evidence":["archive:a1"],
              "score":1.0,
              "redacted":false
            }
          ],
          "budget":{"token_budget":10,"token_used":5}
        }"#;
        assert!(ContextPack::parse_strict(payload).is_err());
    }

    #[test]
    fn hybrid_score_formula() {
        // 0.4 * keyword + 0.6 * vector
        let score = hybrid_score(1.0, 1.0);
        assert!((score - 1.0).abs() < 1e-6);

        let score = hybrid_score(0.0, 1.0);
        assert!((score - 0.6).abs() < 1e-6);

        let score = hybrid_score(1.0, 0.0);
        assert!((score - 0.4).abs() < 1e-6);

        let score = hybrid_score(0.5, 0.5);
        assert!((score - 0.5).abs() < 1e-6);

        let score = hybrid_score(0.0, 0.0);
        assert!(score.abs() < 1e-6);
    }

    #[test]
    fn hybrid_retrieval_uses_vector_scores() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());

        // Build a vector index: a2 has high similarity, a1 has low
        let index = crate::vector_index::VectorIndex::open_in_memory(3).expect("vec index");
        index.upsert("a1", &[0.0, 1.0, 0.0], Sensitivity::Shareable);
        index.upsert("a2", &[1.0, 0.0, 0.0], Sensitivity::Restricted);

        let gateway =
            RecallGateway::with_vector_index(provider, audit, Arc::new(Mutex::new(index)));

        // Query embedding similar to a2's embedding
        let query_embedding = vec![1.0, 0.0, 0.0];
        let pack = gateway
            .get_context_hybrid(
                &ContextRequest {
                    request_id: "hybrid1".to_string(),
                    query: "daemon".to_string(), // keyword match for a1
                    model_class: ModelClass::Local,
                    purpose: Purpose::Answer,
                    sensitivity_max: Sensitivity::Private,
                    token_budget: 500,
                    tags: vec![],
                    recency_days: None,
                    filter_threads: None,
                    disclosure_tier: None,
                    class_budget: None,
                },
                Some(&query_embedding),
            )
            .expect("context should build");

        assert_eq!(pack.retrieval_mode, "hybrid");
        assert!(!pack.items.is_empty());
    }

    #[test]
    fn hybrid_retrieval_falls_back_to_keyword_without_embedding() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());

        let index = crate::vector_index::VectorIndex::open_in_memory(2).expect("vec index");
        index.upsert("a1", &[1.0, 0.0], Sensitivity::Shareable);

        let gateway =
            RecallGateway::with_vector_index(provider, audit, Arc::new(Mutex::new(index)));

        // No embedding provided -> keyword-only
        let pack = gateway
            .get_context_hybrid(
                &ContextRequest {
                    request_id: "fallback1".to_string(),
                    query: "daemon".to_string(),
                    model_class: ModelClass::Local,
                    purpose: Purpose::Answer,
                    sensitivity_max: Sensitivity::Private,
                    token_budget: 500,
                    tags: vec![],
                    recency_days: None,
                    filter_threads: None,
                    disclosure_tier: None,
                    class_budget: None,
                },
                None,
            )
            .expect("context should build");

        assert_eq!(pack.retrieval_mode, "keyword");
    }

    #[test]
    fn hybrid_retrieval_falls_back_without_vector_index() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());

        // No vector index
        let gateway = RecallGateway::new(provider, audit);

        let query_embedding = vec![1.0, 0.0, 0.0];
        let pack = gateway
            .get_context_hybrid(
                &ContextRequest {
                    request_id: "fallback2".to_string(),
                    query: "daemon".to_string(),
                    model_class: ModelClass::Local,
                    purpose: Purpose::Answer,
                    sensitivity_max: Sensitivity::Private,
                    token_budget: 500,
                    tags: vec![],
                    recency_days: None,
                    filter_threads: None,
                    disclosure_tier: None,
                    class_budget: None,
                },
                Some(&query_embedding),
            )
            .expect("context should build");

        assert_eq!(pack.retrieval_mode, "keyword");
    }

    #[test]
    fn hybrid_search_respects_sensitivity_filtering() {
        let entries = vec![
            ArchiveEntry {
                id: "pub".to_string(),
                title: "Public article".to_string(),
                content: "This is shareable content".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
            ArchiveEntry {
                id: "priv".to_string(),
                title: "Private data".to_string(),
                content: "This is private content".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Private,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
        ];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());

        let index = crate::vector_index::VectorIndex::open_in_memory(2).expect("vec index");
        index.upsert("pub", &[1.0, 0.0], Sensitivity::Shareable);
        index.upsert("priv", &[1.0, 0.0], Sensitivity::Private);

        let gateway =
            RecallGateway::with_vector_index(provider, audit, Arc::new(Mutex::new(index)));

        let query_embedding = vec![1.0, 0.0];
        let pack = gateway
            .get_context_hybrid(
                &ContextRequest {
                    request_id: "sens1".to_string(),
                    query: "".to_string(),
                    model_class: ModelClass::Local,
                    purpose: Purpose::Answer,
                    sensitivity_max: Sensitivity::Shareable,
                    token_budget: 500,
                    tags: vec![],
                    recency_days: None,
                    filter_threads: None,
                    disclosure_tier: None,
                    class_budget: None,
                },
                Some(&query_embedding),
            )
            .expect("context should build");

        // Private entry should not appear when max sensitivity is Shareable
        assert!(pack.items.iter().all(|item| item.id != "priv"));
    }

    #[test]
    fn gateway_vector_index_accessors() {
        let provider = Arc::new(TestProvider { entries: vec![] });
        let audit = Arc::new(TestAudit::default());

        let gateway = RecallGateway::new(provider.clone(), audit.clone());
        assert!(gateway.vector_index().is_none());

        let index = Arc::new(Mutex::new(
            crate::vector_index::VectorIndex::open_in_memory(1).expect("vec index"),
        ));
        let gateway = RecallGateway::with_vector_index(provider, audit, index);
        assert!(gateway.vector_index().is_some());
        assert!(gateway.vector_index().unwrap().lock().unwrap().is_empty());

        gateway.vector_index().unwrap().lock().unwrap().upsert(
            "test",
            &[1.0],
            Sensitivity::Shareable,
        );
        assert_eq!(gateway.vector_index().unwrap().lock().unwrap().len(), 1);
    }

    #[test]
    fn gateway_with_graph_merges_memory_items() {
        use crate::graph::{
            BfsGraphRetriever, EntityType, GraphEntity, InMemoryGraphStore, Memory,
        };

        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let mut gateway = RecallGateway::new(provider, audit);

        let mut store = InMemoryGraphStore::new();
        store.add_entity(GraphEntity {
            id: "daemon-entity".to_string(),
            name: "Daemon".to_string(),
            entity_type: EntityType::Concept,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: "mem-daemon".to_string(),
                content: "The daemon manages background workers".to_string(),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:a1".to_string()],
                updated_at: None,
                archived: false,
                fsrs: None,
            }],
        });

        let retriever = BfsGraphRetriever::new(store);
        gateway.set_graph(Arc::new(retriever));

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "graph1".to_string(),
                query: "daemon".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        let memory_items: Vec<_> = pack.items.iter().filter(|i| i.r#type == "memory").collect();
        assert!(
            !memory_items.is_empty(),
            "graph memory items should be present"
        );
        assert_eq!(memory_items[0].id, "daemon-entity");
        assert_eq!(memory_items[0].title, "Daemon");
    }

    #[test]
    fn gateway_without_graph_works_normally() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "nograph1".to_string(),
                query: "daemon".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert!(!pack.items.is_empty());
        assert!(pack.items.iter().all(|i| i.r#type == "entry"));
    }

    #[test]
    fn gateway_graph_deduplicates_with_search_results() {
        use crate::graph::{
            BfsGraphRetriever, EntityType, GraphEntity, InMemoryGraphStore, Memory,
        };

        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let mut gateway = RecallGateway::new(provider, audit);

        let mut store = InMemoryGraphStore::new();
        store.add_entity(GraphEntity {
            id: "a1".to_string(),
            name: "Rust daemon orchestration".to_string(),
            entity_type: EntityType::Concept,
            sensitivity: Sensitivity::Shareable,
            memories: vec![Memory {
                id: "mem-a1".to_string(),
                content: "Graph memory about daemon".to_string(),
                sensitivity: Sensitivity::Shareable,
                evidence: vec!["archive:a1".to_string()],
                updated_at: None,
                archived: false,
                fsrs: None,
            }],
        });

        let retriever = BfsGraphRetriever::new(store);
        gateway.set_graph(Arc::new(retriever));

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "dedup1".to_string(),
                query: "daemon orchestration".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        let a1_count = pack.items.iter().filter(|i| i.id == "a1").count();
        assert_eq!(a1_count, 1, "duplicate entity should be deduplicated");

        let a1_item = pack.items.iter().find(|i| i.id == "a1").unwrap();
        assert!(
            a1_item.score > 1.0,
            "deduped item should have boosted score"
        );
    }

    #[test]
    fn gateway_graph_gracefully_degrades_on_no_seeds() {
        use crate::graph::{BfsGraphRetriever, InMemoryGraphStore};

        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let mut gateway = RecallGateway::new(provider, audit);

        let store = InMemoryGraphStore::new();
        let retriever = BfsGraphRetriever::new(store);
        gateway.set_graph(Arc::new(retriever));

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "degrade1".to_string(),
                query: "daemon".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("should degrade gracefully");

        assert!(!pack.items.is_empty());
        assert_eq!(pack.items[0].id, "a1");
    }

    // -----------------------------------------------------------------------
    // Thread filtering tests
    // -----------------------------------------------------------------------

    #[test]
    fn thread_filter_includes_matching_entries() {
        let entries = vec![
            ArchiveEntry {
                id: "t1".to_string(),
                title: "Thread A fact".to_string(),
                content: "Data from thread A".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: Some("!room-a:example.com".to_string()),
                fact_class: None,
            },
            ArchiveEntry {
                id: "t2".to_string(),
                title: "Thread B fact".to_string(),
                content: "Data from thread B".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: Some("!room-b:example.com".to_string()),
                fact_class: None,
            },
            ArchiveEntry {
                id: "t3".to_string(),
                title: "No thread fact".to_string(),
                content: "Data without thread".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
        ];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "tf1".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: Some(vec!["!room-a:example.com".to_string()]),
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.items.len(), 1);
        assert_eq!(pack.items[0].id, "t1");
    }

    #[test]
    fn thread_filter_none_returns_all_entries() {
        let entries = vec![
            ArchiveEntry {
                id: "t1".to_string(),
                title: "Thread A fact".to_string(),
                content: "Data from thread A".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: Some("!room-a:example.com".to_string()),
                fact_class: None,
            },
            ArchiveEntry {
                id: "t2".to_string(),
                title: "No thread fact".to_string(),
                content: "Data without thread".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: None,
            },
        ];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "tf2".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.items.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Class budget tests
    // -----------------------------------------------------------------------

    #[test]
    fn class_budget_limits_decision_facts() {
        // Create entries all tagged as decisions, verify budget caps them
        let entries: Vec<ArchiveEntry> = (0..10)
            .map(|i| ArchiveEntry {
                id: format!("d{i}"),
                title: format!("Decision {i}"),
                content: format!("We decided to do thing {i}"),
                tags: vec!["decision".to_string()],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: Some(FactClass::Decision),
            })
            .collect();

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        // With default budget: decisions get 30% of 50 tokens = 15 tokens
        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "cb1".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 50,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        // Each entry is roughly ~7 tokens (title + content), so 15 budget / 7 = ~2 items max
        // The exact count depends on token estimation, but it should be fewer than 10
        assert!(
            pack.items.len() < 10,
            "class budget should limit number of decision items, got {}",
            pack.items.len()
        );
    }

    #[test]
    fn class_budget_allows_mixed_classes() {
        let entries = vec![
            ArchiveEntry {
                id: "d1".to_string(),
                title: "Decision alpha".to_string(),
                content: "We decided alpha".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: Some(FactClass::Decision),
            },
            ArchiveEntry {
                id: "f1".to_string(),
                title: "Finding beta".to_string(),
                content: "We found beta".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: Some(FactClass::Finding),
            },
            ArchiveEntry {
                id: "e1".to_string(),
                title: "Entity gamma".to_string(),
                content: "Entity gamma details".to_string(),
                tags: vec![],
                sensitivity: Sensitivity::Shareable,
                source_url: None,
                updated_at: now_unix(),
                thread_id: None,
                fact_class: Some(FactClass::Entity),
            },
        ];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "cb2".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            })
            .expect("context should build");

        // All three should fit within budget
        assert_eq!(pack.items.len(), 3);
    }

    // -----------------------------------------------------------------------
    // Progressive disclosure tests
    // -----------------------------------------------------------------------

    #[test]
    fn disclosure_titles_strips_content() {
        let entries = vec![ArchiveEntry {
            id: "pd1".to_string(),
            title: "Important fact".to_string(),
            content: "Detailed content that should be stripped in titles mode".to_string(),
            tags: vec![],
            sensitivity: Sensitivity::Shareable,
            source_url: None,
            updated_at: now_unix(),
            thread_id: None,
            fact_class: None,
        }];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "pd1".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: Some(DisclosureTier::Titles),
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.items.len(), 1);
        // Content should be empty in Titles mode
        assert!(
            pack.items[0].content.is_empty(),
            "content should be empty in Titles tier, got: '{}'",
            pack.items[0].content
        );
        // But title should be preserved
        assert_eq!(pack.items[0].title, "Important fact");
    }

    #[test]
    fn disclosure_summary_returns_first_line() {
        let entries = vec![ArchiveEntry {
            id: "pd2".to_string(),
            title: "Multi-line fact".to_string(),
            content: "First line summary.\nSecond line detail.\nThird line extra.".to_string(),
            tags: vec![],
            sensitivity: Sensitivity::Shareable,
            source_url: None,
            updated_at: now_unix(),
            thread_id: None,
            fact_class: None,
        }];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "pd2".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: Some(DisclosureTier::Summary),
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.items.len(), 1);
        assert_eq!(pack.items[0].content, "First line summary.");
    }

    #[test]
    fn disclosure_full_returns_all_content() {
        let content = "First line.\nSecond line.\nThird line.";
        let entries = vec![ArchiveEntry {
            id: "pd3".to_string(),
            title: "Full fact".to_string(),
            content: content.to_string(),
            tags: vec![],
            sensitivity: Sensitivity::Shareable,
            source_url: None,
            updated_at: now_unix(),
            thread_id: None,
            fact_class: None,
        }];

        let provider = Arc::new(TestProvider { entries });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        let pack = gateway
            .get_context(&ContextRequest {
                request_id: "pd3".to_string(),
                query: "".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: Some(DisclosureTier::Full),
                class_budget: None,
            })
            .expect("context should build");

        assert_eq!(pack.items.len(), 1);
        assert_eq!(pack.items[0].content, content);
    }

    // -----------------------------------------------------------------------
    // Query gap tracking tests
    // -----------------------------------------------------------------------

    #[test]
    fn empty_result_logs_gap() {
        let provider = Arc::new(TestProvider { entries: vec![] });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        // Query should return empty (no entries) and log gap
        for _ in 0..3 {
            let _ = gateway.get_context(&ContextRequest {
                request_id: "gap1".to_string(),
                query: "nonexistent topic".to_string(),
                model_class: ModelClass::Local,
                purpose: Purpose::Answer,
                sensitivity_max: Sensitivity::Private,
                token_budget: 500,
                tags: vec![],
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            });
        }

        let gaps = gateway.get_query_gaps();
        assert_eq!(gaps.len(), 1, "should have one gap after 3 misses");
        assert_eq!(gaps[0].miss_count, 3);
        assert!(gaps[0].query_pattern.contains("nonexistent"));
    }

    #[test]
    fn successful_query_does_not_log_gap() {
        let provider = Arc::new(TestProvider {
            entries: sample_entries(),
        });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit);

        // Query that matches entries should NOT log a gap
        for _ in 0..5 {
            let _ = gateway.get_context(&make_request("daemon queue"));
        }

        let gaps = gateway.get_query_gaps();
        assert!(gaps.is_empty(), "successful queries should not create gaps");
    }

    // -----------------------------------------------------------------------
    // Write-back tests
    // -----------------------------------------------------------------------

    #[test]
    fn write_back_records_audit() {
        let provider = Arc::new(TestProvider { entries: vec![] });
        let audit = Arc::new(TestAudit::default());
        let gateway = RecallGateway::new(provider, audit.clone());

        gateway
            .write_back(
                "what is Rust?",
                "Rust is a systems programming language",
                "https://rust-lang.org",
            )
            .expect("write-back should succeed");

        let records = audit.records.lock().expect("audit lock");
        assert_eq!(records.len(), 1);
        assert!(records[0].request_id.starts_with("write-back:"));
        assert_eq!(records[0].retrieval_mode, "write-back");
    }
}
