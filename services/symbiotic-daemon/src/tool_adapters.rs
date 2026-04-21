//! Adapter types bridging daemon stores to agent tool backend traits.
//!
//! Each adapter wraps one of the daemon's concrete store types and implements
//! the corresponding backend trait from `symbiotic_agents::builtin_tools`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use symbiotic_agents::builtin_tools::{
    ArchiveBackend, CapabilityChecker, DispatchAgentBackend, DispatchedAgentResult,
    QueueBackend as AgentQueueBackend, RecallBackend, RecallItem,
};
use symbiotic_archive::{ArchiveSensitivity, FileArchiveStore, StoreRequest};
use symbiotic_context::vector_index::VectorIndex;
use symbiotic_core::Sensitivity;
use symbiotic_providers::ProviderRouter;
use symbiotic_queue::{EnqueueRequest, QueueBackend};
use symbiotic_trust::{AccessBroker, AccessRequest, AgentTrustLevel};

// ---------------------------------------------------------------------------
// DaemonRecallBackend
// ---------------------------------------------------------------------------

/// Bridges the daemon's `FileArchiveStore` to the agent `RecallBackend` trait.
///
/// When a shared `VectorIndex` and `ProviderRouter` are available, queries are
/// executed as semantic search: the query text is embedded via the router, then
/// cosine-similarity search is run against the vector index. Matching entry IDs
/// are looked up in the `FileArchiveStore` to build the result set.
///
/// Falls back to simple case-insensitive substring matching when:
/// - No vector index or provider router was provided
/// - The vector index is empty (no embeddings yet)
/// - Embedding the query fails (provider unavailable, network error, etc.)
pub struct DaemonRecallBackend {
    archive_store: Arc<FileArchiveStore>,
    vector_index: Option<Arc<Mutex<VectorIndex>>>,
    provider_router: Option<Arc<ProviderRouter>>,
}

impl DaemonRecallBackend {
    /// Create a recall backend with text-matching only (no semantic search).
    pub fn new(archive_store: Arc<FileArchiveStore>) -> Self {
        Self {
            archive_store,
            vector_index: None,
            provider_router: None,
        }
    }

    /// Create a recall backend with semantic search via vector index.
    pub fn with_vector_search(
        archive_store: Arc<FileArchiveStore>,
        vector_index: Arc<Mutex<VectorIndex>>,
        provider_router: Arc<ProviderRouter>,
    ) -> Self {
        Self {
            archive_store,
            vector_index: Some(vector_index),
            provider_router: Some(provider_router),
        }
    }
}

#[async_trait]
impl RecallBackend for DaemonRecallBackend {
    async fn query(&self, query: &str, max_items: usize) -> Result<Vec<RecallItem>> {
        // Attempt semantic search when vector index and provider router are available.
        if let (Some(vi), Some(router)) = (&self.vector_index, &self.provider_router) {
            let is_empty = vi.lock().map(|idx| idx.is_empty()).unwrap_or(true);

            if !is_empty {
                // Embed the query text.
                match router.embed(query, Sensitivity::Shareable).await {
                    Ok(embed_result) => {
                        let vi = vi.clone();
                        let store = self.archive_store.clone();
                        let embedding = embed_result.embedding;
                        let max = max_items;

                        return tokio::task::spawn_blocking(move || {
                            let search_results = {
                                let index = vi
                                    .lock()
                                    .map_err(|_| anyhow!("vector index lock poisoned"))?;
                                index.search(
                                    &embedding,
                                    Sensitivity::Private, // include all sensitivity levels
                                    max,
                                )
                            };

                            let mut items = Vec::with_capacity(search_results.len());
                            for result in search_results {
                                if let Ok(Some(doc)) = store.get(&result.entry_id) {
                                    // Return up to ~16 KB of content so a
                                    // specialist agent can perform a real
                                    // analysis from one recall hit. At
                                    // 1500 chars the snippet was basically
                                    // the tl;dr and auditors reported
                                    // "context missing for deep analysis".
                                    let snippet =
                                        doc.content.chars().take(16_000).collect::<String>();
                                    items.push(RecallItem {
                                        id: doc.record_id,
                                        title: doc.title,
                                        snippet,
                                        score: result.similarity,
                                    });
                                }
                            }
                            Ok(items)
                        })
                        .await?;
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "semantic recall: embedding query failed, falling back to text search"
                        );
                        // Fall through to text matching below.
                    }
                }
            }
        }

        // Fallback: simple case-insensitive substring matching.
        let store = self.archive_store.clone();
        let query = query.to_string();

        tokio::task::spawn_blocking(move || {
            let docs = store.list()?;
            let query_lower = query.to_ascii_lowercase();

            let mut scored: Vec<RecallItem> = docs
                .into_iter()
                .filter_map(|doc| {
                    let id_lower = doc.record_id.to_ascii_lowercase();
                    let title_lower = doc.title.to_ascii_lowercase();
                    let content_lower = doc.content.to_ascii_lowercase();

                    // Match on record_id bidirectionally so agents can recall
                    // by exact id (`arc_abc...`) AND by queries that embed
                    // that id inside a longer string (the orchestrator's
                    // typical pattern: "cua (arc_dd77...) and trustgraph
                    // (arc_17f3...) evaluation context"). Without the
                    // reverse check, those longer queries returned nothing
                    // because the id is obviously not a superstring of the
                    // whole query.
                    let id_match = id_lower.contains(&query_lower)
                        || query_lower.contains(&id_lower);
                    let title_match = title_lower.contains(&query_lower)
                        || query_lower.contains(&title_lower);
                    let content_match = content_lower.contains(&query_lower);

                    if !id_match && !title_match && !content_match {
                        return None;
                    }

                    let score = if id_match {
                        1.5
                    } else if title_match {
                        1.0
                    } else {
                        0.5
                    };
                    let snippet = doc.content.chars().take(16_000).collect::<String>();

                    Some(RecallItem {
                        id: doc.record_id,
                        title: doc.title,
                        snippet,
                        score,
                    })
                })
                .collect();

            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(max_items);
            Ok(scored)
        })
        .await?
    }
}

// ---------------------------------------------------------------------------
// DaemonArchiveBackend
// ---------------------------------------------------------------------------

/// Bridges the daemon's `FileArchiveStore` to the agent `ArchiveBackend` trait.
pub struct DaemonArchiveBackend {
    store: Arc<FileArchiveStore>,
}

impl DaemonArchiveBackend {
    pub fn new(store: Arc<FileArchiveStore>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ArchiveBackend for DaemonArchiveBackend {
    async fn store(&self, title: String, content: String, tags: Vec<String>) -> Result<String> {
        let store = self.store.clone();

        // T132 §05: tool observations enter the Archive via this backend.
        // Per design §2.3 sandboxed tool observations are `Low` trust; we
        // run Stages A+B synchronously inside spawn_blocking and divert
        // quarantined content. The `tool.observation` call site is recorded
        // on the verdict for audit (T120).
        tokio::task::spawn_blocking(move || {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            title.hash(&mut hasher);
            content.hash(&mut hasher);
            symbiotic_core::now_unix().hash(&mut hasher);
            let idempotency_key = format!("agent-{:x}", hasher.finish());

            let scan_ctx = symbiotic_firewall::types::ScanContext {
                source: symbiotic_firewall::types::ContentSource {
                    kind: "tool.observation".into(),
                    url: None,
                    fetched_at: time::OffsetDateTime::now_utc(),
                    claimed_content_type: Some("text/markdown".into()),
                    headers: Default::default(),
                },
                consuming_agent_scope: symbiotic_firewall::types::ConsumingAgentScope::minimal(
                    "tool-observation-handler",
                ),
                call_site: symbiotic_firewall::types::CallSite::new("tool.observation"),
            };
            let cfg_a = symbiotic_firewall::stages::StageAConfig::default();
            let cfg_b = symbiotic_firewall::stages::StageBConfig::default();
            let mut verdict =
                symbiotic_firewall::stages::run_stages_a_b(&scan_ctx, &content, &cfg_a, &cfg_b);
            // §07 source-receipt placeholder.
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(content.as_bytes());
            verdict.source_receipt_id = Some(format!("{:x}", h.finalize()));
            if matches!(
                verdict.verdict,
                symbiotic_firewall::types::Verdict::Quarantined
            ) {
                return Err(anyhow!(
                    "tool observation quarantined by firewall ({:?})",
                    verdict.quarantine_class
                ));
            }

            let request = StoreRequest {
                title_hint: Some(title),
                content,
                source_url: None,
                tags,
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key,
                firewall_verdict: Some(verdict),
            };

            let outcome = store.store(request)?;
            Ok(outcome.record_id)
        })
        .await?
    }
}

// ---------------------------------------------------------------------------
// DaemonQueueBackend
// ---------------------------------------------------------------------------

/// Bridges the daemon's `QueueBackend` (from symbiotic-queue) to the agent's
/// `QueueBackend` trait (from symbiotic-agents). These are different traits with
/// different signatures.
pub struct DaemonQueueBackend {
    queue: Arc<dyn QueueBackend>,
}

impl DaemonQueueBackend {
    pub fn new(queue: Arc<dyn QueueBackend>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl AgentQueueBackend for DaemonQueueBackend {
    async fn enqueue(
        &self,
        job_type: String,
        payload: String,
        idempotency_key: String,
    ) -> Result<String> {
        let queue = self.queue.clone();

        tokio::task::spawn_blocking(move || {
            let request = EnqueueRequest {
                type_name: job_type,
                payload,
                idempotency_key,
                max_attempts: 3,
                next_run_at: symbiotic_core::now_unix(),
                force: false,
            };

            let outcome = queue.enqueue(request)?;
            Ok(outcome.job_id)
        })
        .await?
    }
}

// ---------------------------------------------------------------------------
// DaemonCapabilityChecker
// ---------------------------------------------------------------------------

/// Capability checker that delegates to the `AccessBroker` for real
/// capability-token-based authorization.
///
/// When `check()` is called, the checker searches the broker's token list for
/// a token whose `subject` matches the given `agent_id` and whose `scopes`
/// include the requested scope. If a matching token is found, `evaluate()` is
/// called to enforce expiry, trust level, and one-time consumption rules.
///
/// If no broker is provided (constructed via `DaemonCapabilityChecker::allow_all()`),
/// all operations are permitted — this preserves backward compatibility for
/// tests and trusted-context scenarios.
pub struct DaemonCapabilityChecker {
    broker: Option<Arc<Mutex<AccessBroker>>>,
    goal_scope: Option<String>,
}

impl DaemonCapabilityChecker {
    /// Create a capability checker backed by an `AccessBroker`.
    pub fn new(broker: Arc<Mutex<AccessBroker>>) -> Self {
        Self {
            broker: Some(broker),
            goal_scope: None,
        }
    }

    /// Create a capability checker scoped to a specific goal namespace.
    pub fn for_goal_scope(broker: Arc<Mutex<AccessBroker>>, goal_scope: Option<String>) -> Self {
        Self {
            broker: Some(broker),
            goal_scope,
        }
    }

    /// Create an allow-all checker (no broker). Useful for tests and
    /// trusted daemon contexts where no tokens have been issued yet.
    pub fn allow_all() -> Self {
        Self {
            broker: None,
            goal_scope: None,
        }
    }
}

impl Default for DaemonCapabilityChecker {
    fn default() -> Self {
        Self::allow_all()
    }
}

impl CapabilityChecker for DaemonCapabilityChecker {
    fn check(&self, agent_id: &str, scope: &str) -> Result<()> {
        let broker = match &self.broker {
            Some(b) => b,
            None => return Ok(()), // allow-all mode
        };

        let mut broker = broker
            .lock()
            .map_err(|_| anyhow!("access broker lock poisoned"))?;

        let now = symbiotic_core::now_unix();

        // Map scope prefixes to minimum trust levels.
        let required_level = scope_to_trust_level(scope);

        let matching_token_ids: Vec<String> = broker
            .tokens()
            .iter()
            .filter(|t| {
                t.subject == agent_id && t.scopes.contains(scope) && t.goal_scope == self.goal_scope
            })
            .map(|t| t.token_id.clone())
            .collect();

        if matching_token_ids.is_empty() {
            return Err(anyhow!(
                "no capability token found for agent '{}' with scope '{}' and goal_scope {:?}",
                agent_id,
                scope,
                self.goal_scope
            ));
        }

        let request = AccessRequest {
            subject: agent_id.to_string(),
            required_level,
            scope: scope.to_string(),
            goal_scope: self.goal_scope.clone(),
        };

        let mut last_error = None;
        for token_id in matching_token_ids {
            match broker.evaluate(&token_id, &request, now) {
                Ok(decision) if decision.allowed => return Ok(()),
                Ok(_) => {
                    last_error = Some(anyhow!(
                        "capability denied for agent '{}' scope '{}': denied",
                        agent_id,
                        scope
                    ));
                }
                Err(e) => last_error = Some(e),
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow!(
                "no capability token found for agent '{}' with scope '{}'",
                agent_id,
                scope
            )
        }))
    }
}

// ---------------------------------------------------------------------------
// DaemonDispatchBackend
// ---------------------------------------------------------------------------

/// Bridges the daemon's `AgentExecuteExecutor` to the agent `DispatchAgentBackend`
/// trait so that a running agent can synchronously spawn a sub-agent via the
/// `dispatch_agent` tool. Holds a `Weak<AgentExecuteExecutor>` because the
/// executor stores this backend on itself (cycle avoidance).
///
/// On `dispatch`, the call is routed through `tokio::task::spawn_blocking` to
/// keep the nested `thread::scope + Handle::block_on` inside
/// `AgentExecuteExecutor::execute_react` off the current tokio worker thread —
/// avoids deadlocks when the outer ReAct loop is itself awaited on the
/// runtime's main thread.
pub struct DaemonDispatchBackend {
    executor: std::sync::Weak<crate::workers::AgentExecuteExecutor>,
}

impl DaemonDispatchBackend {
    pub(crate) fn new(executor: std::sync::Weak<crate::workers::AgentExecuteExecutor>) -> Self {
        Self { executor }
    }
}

#[async_trait]
impl DispatchAgentBackend for DaemonDispatchBackend {
    async fn dispatch(&self, role: String, goal: String) -> Result<DispatchedAgentResult> {
        let executor = self
            .executor
            .upgrade()
            .ok_or_else(|| anyhow!("agent executor dropped; cannot dispatch sub-agent"))?;

        let join = tokio::task::spawn_blocking(move || executor.run_agent_goal(&role, &goal)).await;

        let exec_result = join.map_err(|e| anyhow!("dispatch_agent join error: {e}"))?;

        Ok(DispatchedAgentResult {
            output: exec_result.output,
            status: exec_result.status,
        })
    }
}

/// Map a scope string to its minimum required `AgentTrustLevel`.
///
/// Roles declare `required_capabilities` in TOML. `execute_react` now filters
/// every tool by those caps — inventing a new scope therefore requires: (1)
/// add a branch below, (2) associate the scope with the tool(s) in the
/// execute_react filter, (3) add the scope to the `required_capabilities`
/// of roles that should wield those tools.
///
/// Scope prefixes and their trust requirements:
/// - `credential.*`                         → `CredentialAccess`
/// - `action.*` / `queue.*` / `agent.dispatch*` → `ExternalAct`
/// - `archive.*` / `plan.*`                 → `ArchiveWrite`
/// - `user.*`                               → `ReadOnly` (ask-the-user only)
/// - anything else                          → `ReadOnly`
pub(crate) fn scope_to_trust_level(scope: &str) -> AgentTrustLevel {
    if scope.starts_with("credential") {
        AgentTrustLevel::CredentialAccess
    } else if scope.starts_with("action")
        || scope.starts_with("queue")
        || scope.starts_with("agent.dispatch")
    {
        AgentTrustLevel::ExternalAct
    } else if scope.starts_with("archive") || scope.starts_with("plan") {
        AgentTrustLevel::ArchiveWrite
    } else {
        AgentTrustLevel::ReadOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symbiotic_providers::{
        CapabilitySet, EmbedResult, ModelProvider as ProviderModelProvider, PricingInfo,
        ProviderCapability, ProviderClass, ProviderError, RegisteredProvider,
    };
    use symbiotic_trust::CapabilityToken;

    // -----------------------------------------------------------------------
    // Deterministic mock embedding provider for semantic search E2E tests
    // -----------------------------------------------------------------------

    /// Returns deterministic vectors based on keyword detection in input text.
    /// "rust"/"async"/"tokio" → [0.9, 0.1, 0.0] (programming-Rust cluster)
    /// "python"/"decorator"  → [0.0, 0.1, 0.9] (programming-Python cluster)
    /// "kubernetes"/"docker"  → [0.1, 0.9, 0.0] (infrastructure cluster)
    /// default/unknown        → [0.33, 0.33, 0.33] (neutral)
    struct DeterministicEmbeddingProvider;

    impl DeterministicEmbeddingProvider {
        fn vector_for(text: &str) -> Vec<f32> {
            let lower = text.to_ascii_lowercase();
            if lower.contains("rust") || lower.contains("async") || lower.contains("tokio") {
                vec![0.9, 0.1, 0.0]
            } else if lower.contains("python") || lower.contains("decorator") {
                vec![0.0, 0.1, 0.9]
            } else if lower.contains("kubernetes") || lower.contains("docker") {
                vec![0.1, 0.9, 0.0]
            } else {
                vec![0.33, 0.33, 0.33]
            }
        }
    }

    impl ProviderModelProvider for DeterministicEmbeddingProvider {
        fn name(&self) -> &str {
            "deterministic-test"
        }
        fn provider_class(&self) -> ProviderClass {
            ProviderClass::Local
        }
        fn model_name(&self) -> &str {
            "test-embed-3d"
        }
        fn capabilities(&self) -> &CapabilitySet {
            // Leaked to get a &CapabilitySet with 'static lifetime.
            // Fine in test code — the process exits after the test.
            Box::leak(Box::new(CapabilitySet::new(vec![
                ProviderCapability::Embedding,
            ])))
        }
        fn pricing(&self) -> Option<&PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl symbiotic_providers::EmbeddingProvider for DeterministicEmbeddingProvider {
        async fn embed(&self, text: &str) -> Result<EmbedResult, ProviderError> {
            let embedding = Self::vector_for(text);
            let dimensions = embedding.len();
            Ok(EmbedResult {
                embedding,
                model_name: "test-embed-3d".to_string(),
                dimensions,
            })
        }
    }

    /// Helper: build a ProviderRouter with the deterministic embedding provider.
    fn router_with_deterministic_embeddings() -> Arc<ProviderRouter> {
        let mut registry = symbiotic_providers::ProviderRegistry::new();
        let provider = Arc::new(DeterministicEmbeddingProvider);
        let base: Arc<dyn ProviderModelProvider> = provider.clone();
        let embedding: Arc<dyn symbiotic_providers::EmbeddingProvider> = provider;
        registry.register(RegisteredProvider {
            base,
            completion: None,
            embedding: Some(embedding),
            image: None,
            video: None,
            agent: None,
        });
        Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))))
    }

    // -----------------------------------------------------------------------
    // Task A tests: DaemonRecallBackend with vector index
    // -----------------------------------------------------------------------

    #[test]
    fn recall_backend_new_has_no_vector_index() {
        let tmp = std::env::temp_dir().join("recall_backend_no_vi");
        let _ = std::fs::create_dir_all(&tmp);
        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));
        let backend = DaemonRecallBackend::new(store);
        assert!(backend.vector_index.is_none());
        assert!(backend.provider_router.is_none());
    }

    #[test]
    fn recall_backend_with_vector_search_has_index() {
        let tmp = std::env::temp_dir().join("recall_backend_vi");
        let _ = std::fs::create_dir_all(&tmp);
        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));
        let vi = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));

        // Use a minimal provider router (no actual providers needed for this test).
        let registry = symbiotic_providers::ProviderRegistry::new();
        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));

        let backend = DaemonRecallBackend::with_vector_search(store, vi.clone(), router);
        assert!(backend.vector_index.is_some());
        assert!(backend.provider_router.is_some());
    }

    #[tokio::test]
    async fn recall_backend_empty_vector_index_falls_back_to_text_search() {
        let tmp = std::env::temp_dir().join("recall_fallback_empty_vi");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));
        // Seed an entry
        store
            .store(StoreRequest {
                title_hint: Some("Rust patterns".to_string()),
                content: "Guide to Rust design patterns.".to_string(),
                source_url: None,
                tags: vec![],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "test-1".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed entry");

        let vi = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        )); // empty
        let registry = symbiotic_providers::ProviderRegistry::new();
        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));

        let backend = DaemonRecallBackend::with_vector_search(store, vi, router);
        let results = backend.query("Rust", 5).await.expect("query should work");
        assert_eq!(results.len(), 1);
        assert!(results[0].title.contains("Rust"));
    }

    #[tokio::test]
    async fn recall_backend_populated_vector_index_falls_back_without_provider() {
        let tmp = std::env::temp_dir().join("recall_fallback_no_provider");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));

        // Seed two entries
        let outcome1 = store
            .store(StoreRequest {
                title_hint: Some("Rust async".to_string()),
                content: "Async/await in Rust.".to_string(),
                source_url: None,
                tags: vec![],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "sem-1".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed entry 1");

        let outcome2 = store
            .store(StoreRequest {
                title_hint: Some("Python decorators".to_string()),
                content: "Python decorator patterns.".to_string(),
                source_url: None,
                tags: vec![],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "sem-2".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed entry 2");

        let vi = VectorIndex::open_in_memory(3).expect("vec index");
        vi.upsert(
            &outcome1.record_id,
            &[0.9, 0.1, 0.0],
            Sensitivity::Shareable,
        );
        vi.upsert(
            &outcome2.record_id,
            &[0.0, 0.1, 0.9],
            Sensitivity::Shareable,
        );
        let vi = Arc::new(Mutex::new(vi));

        // No embedding providers registered → falls back to text matching.
        let registry = symbiotic_providers::ProviderRegistry::new();
        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));

        let backend = DaemonRecallBackend::with_vector_search(store, vi, router);
        let results = backend.query("Rust", 5).await.expect("query should work");
        assert_eq!(results.len(), 1, "text fallback should find 'Rust async'");
        assert!(results[0].title.contains("Rust"));
    }

    #[tokio::test]
    async fn e2e_semantic_search_ranks_by_cosine_similarity() {
        // Full E2E: store entries → embed → index → query → verify ranking.
        let tmp = std::env::temp_dir().join("e2e_semantic_recall");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));

        // Seed three entries in different semantic domains.
        let rust_entry = store
            .store(StoreRequest {
                title_hint: Some("Rust async patterns".to_string()),
                content: "Guide to async/await and tokio in Rust.".to_string(),
                source_url: None,
                tags: vec!["rust".to_string()],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "e2e-sem-rust".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed Rust entry");

        let python_entry = store
            .store(StoreRequest {
                title_hint: Some("Python decorator guide".to_string()),
                content: "How to use Python decorators effectively.".to_string(),
                source_url: None,
                tags: vec!["python".to_string()],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "e2e-sem-python".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed Python entry");

        let k8s_entry = store
            .store(StoreRequest {
                title_hint: Some("Kubernetes deployment".to_string()),
                content: "Running Docker containers on Kubernetes.".to_string(),
                source_url: None,
                tags: vec!["infra".to_string()],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "e2e-sem-k8s".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed K8s entry");

        // Build vector index with deterministic embeddings matching content.
        let vi = VectorIndex::open_in_memory(3).expect("vec index");
        vi.upsert(
            &rust_entry.record_id,
            &DeterministicEmbeddingProvider::vector_for("Rust async tokio"),
            Sensitivity::Shareable,
        );
        vi.upsert(
            &python_entry.record_id,
            &DeterministicEmbeddingProvider::vector_for("Python decorator"),
            Sensitivity::Shareable,
        );
        vi.upsert(
            &k8s_entry.record_id,
            &DeterministicEmbeddingProvider::vector_for("Kubernetes Docker"),
            Sensitivity::Shareable,
        );
        let vi = Arc::new(Mutex::new(vi));

        let router = router_with_deterministic_embeddings();
        let backend = DaemonRecallBackend::with_vector_search(store, vi, router);

        // Query "async Rust" — should rank the Rust entry highest.
        let results = backend
            .query("async Rust patterns", 10)
            .await
            .expect("semantic query should succeed");

        assert!(!results.is_empty(), "semantic search should return results");
        assert_eq!(
            results[0].id, rust_entry.record_id,
            "Rust entry should rank first for 'async Rust patterns' query"
        );
        assert!(
            results[0].score > 0.9,
            "top result similarity should be >0.9, got {}",
            results[0].score
        );

        // Verify all 3 entries are returned (all have positive similarity in 3D space).
        assert_eq!(
            results.len(),
            3,
            "all three entries should have non-zero similarity"
        );

        // Query "Python decorator" — should rank the Python entry highest.
        let py_results = backend
            .query("Python decorator patterns", 10)
            .await
            .expect("Python query should succeed");

        assert_eq!(
            py_results[0].id, python_entry.record_id,
            "Python entry should rank first for 'Python decorator' query"
        );

        // Query "Docker Kubernetes" — should rank the K8s entry highest.
        let k8s_results = backend
            .query("Docker Kubernetes deployment", 10)
            .await
            .expect("K8s query should succeed");

        assert_eq!(
            k8s_results[0].id, k8s_entry.record_id,
            "K8s entry should rank first for 'Docker Kubernetes' query"
        );
    }

    #[tokio::test]
    async fn e2e_semantic_search_respects_top_k_limit() {
        let tmp = std::env::temp_dir().join("e2e_semantic_topk");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));

        // Seed 5 entries
        let mut record_ids = Vec::new();
        for i in 0..5 {
            let outcome = store
                .store(StoreRequest {
                    title_hint: Some(format!("Rust topic {i}")),
                    content: format!("Rust async programming guide part {i}."),
                    source_url: None,
                    tags: vec![],
                    sensitivity: ArchiveSensitivity::Shareable,
                    idempotency_key: format!("topk-{i}"),
                    firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
                })
                .expect("seed entry");
            record_ids.push(outcome.record_id);
        }

        let vi = VectorIndex::open_in_memory(3).expect("vec index");
        for id in &record_ids {
            vi.upsert(
                id,
                &DeterministicEmbeddingProvider::vector_for("Rust async"),
                Sensitivity::Shareable,
            );
        }
        let vi = Arc::new(Mutex::new(vi));

        let router = router_with_deterministic_embeddings();
        let backend = DaemonRecallBackend::with_vector_search(store, vi, router);

        let results = backend.query("Rust async", 3).await.expect("query");
        assert_eq!(results.len(), 3, "should respect top_k=3 limit");
    }

    #[tokio::test]
    async fn e2e_semantic_search_snippet_truncated_to_200_chars() {
        let tmp = std::env::temp_dir().join("e2e_semantic_snippet");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);

        let store = Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("open store"));

        let long_content = "Rust ".repeat(100); // 500 chars
        let outcome = store
            .store(StoreRequest {
                title_hint: Some("Long Rust article".to_string()),
                content: long_content,
                source_url: None,
                tags: vec![],
                sensitivity: ArchiveSensitivity::Shareable,
                idempotency_key: "snippet-test".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("seed");

        let vi = VectorIndex::open_in_memory(3).expect("vec index");
        vi.upsert(
            &outcome.record_id,
            &DeterministicEmbeddingProvider::vector_for("Rust"),
            Sensitivity::Shareable,
        );
        let vi = Arc::new(Mutex::new(vi));

        let router = router_with_deterministic_embeddings();
        let backend = DaemonRecallBackend::with_vector_search(store, vi, router);

        let results = backend.query("Rust async", 5).await.expect("query");
        assert_eq!(results.len(), 1);
        assert!(
            results[0].snippet.len() <= 200,
            "snippet should be truncated to 200 chars, got {}",
            results[0].snippet.len()
        );
    }

    // -----------------------------------------------------------------------
    // Task B tests: DaemonCapabilityChecker with AccessBroker
    // -----------------------------------------------------------------------

    #[test]
    fn capability_checker_allow_all_permits_everything() {
        let checker = DaemonCapabilityChecker::allow_all();
        assert!(checker.check("agent-1", "archive.read").is_ok());
        assert!(checker.check("agent-1", "credential.read").is_ok());
        assert!(checker.check("agent-1", "action.browser.login").is_ok());
    }

    #[test]
    fn capability_checker_default_is_allow_all() {
        let checker = DaemonCapabilityChecker::default();
        assert!(checker.check("any-agent", "any.scope").is_ok());
    }

    #[test]
    fn capability_checker_denies_when_no_token_exists() {
        let broker = Arc::new(Mutex::new(AccessBroker::new()));
        let checker = DaemonCapabilityChecker::new(broker);

        let err = checker
            .check("agent-1", "archive.read")
            .expect_err("should deny without token");
        assert!(
            err.to_string().contains("no capability token found"),
            "error should mention missing token, got: {}",
            err
        );
    }

    #[test]
    fn capability_checker_grants_with_valid_token() {
        let now = symbiotic_core::now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-agent".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: ["archive.read".to_string(), "archive.write".to_string()]
                .into_iter()
                .collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        let broker = Arc::new(Mutex::new(broker));
        let checker = DaemonCapabilityChecker::new(broker);

        assert!(checker.check("agent-1", "archive.read").is_ok());
        assert!(checker.check("agent-1", "archive.write").is_ok());
    }

    #[test]
    fn capability_checker_denies_scope_not_in_token() {
        let now = symbiotic_core::now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-limited".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        let broker = Arc::new(Mutex::new(broker));
        let checker = DaemonCapabilityChecker::new(broker);

        assert!(checker.check("agent-1", "archive.read").is_ok());
        let err = checker
            .check("agent-1", "queue.submit")
            .expect_err("should deny missing scope");
        assert!(
            err.to_string().contains("no capability token found"),
            "error: {}",
            err
        );
    }

    #[test]
    fn capability_checker_denies_wrong_agent() {
        let now = symbiotic_core::now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-other".to_string(),
            subject: "agent-2".to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        let broker = Arc::new(Mutex::new(broker));
        let checker = DaemonCapabilityChecker::new(broker);

        let err = checker
            .check("agent-1", "archive.read")
            .expect_err("should deny wrong agent");
        assert!(err.to_string().contains("no capability token found"));
    }

    #[test]
    fn capability_checker_denies_expired_token() {
        let now = symbiotic_core::now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "t-expired".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now - 1, // already expired
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        let broker = Arc::new(Mutex::new(broker));
        let checker = DaemonCapabilityChecker::new(broker);

        let err = checker
            .check("agent-1", "archive.read")
            .expect_err("should deny expired token");
        assert!(
            err.to_string().contains("expired"),
            "error should mention expiry, got: {}",
            err
        );
    }

    #[test]
    fn capability_checker_respects_goal_scope() {
        let now = symbiotic_core::now_unix();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: "global-token".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        broker.issue_token(CapabilityToken {
            token_id: "goal-token".to_string(),
            subject: "agent-1".to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: Some("wf-1".to_string()),
        });
        let broker = Arc::new(Mutex::new(broker));

        let scoped =
            DaemonCapabilityChecker::for_goal_scope(Arc::clone(&broker), Some("wf-1".to_string()));
        assert!(
            scoped.check("agent-1", "archive.read").is_ok(),
            "goal-scoped token should authorize matching workflow"
        );

        let wrong_scope = DaemonCapabilityChecker::for_goal_scope(broker, Some("wf-2".to_string()));
        let err = wrong_scope
            .check("agent-1", "archive.read")
            .expect_err("mismatched goal scope should be denied");
        assert!(
            err.to_string().contains("goal_scope"),
            "error should mention goal scope, got: {}",
            err
        );
    }

    #[test]
    fn scope_to_trust_level_maps_correctly() {
        assert_eq!(
            scope_to_trust_level("archive.read"),
            AgentTrustLevel::ArchiveWrite
        );
        assert_eq!(
            scope_to_trust_level("archive.write"),
            AgentTrustLevel::ArchiveWrite
        );
        assert_eq!(
            scope_to_trust_level("credential.read"),
            AgentTrustLevel::CredentialAccess
        );
        assert_eq!(
            scope_to_trust_level("action.browser.login"),
            AgentTrustLevel::ExternalAct
        );
        assert_eq!(
            scope_to_trust_level("queue.submit"),
            AgentTrustLevel::ExternalAct
        );
        assert_eq!(
            scope_to_trust_level("unknown.scope"),
            AgentTrustLevel::ReadOnly
        );
    }
}
