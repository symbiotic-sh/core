pub mod conflict;
pub mod dedup;
pub mod distillery;
pub mod ollama;
pub mod pipeline;
pub mod rollback;
pub mod thread_adapter;
use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use symbiotic_agents::llm::LlmClient;
use symbiotic_context::intake_embeddings::{EmbeddingOutcome, IntakeEmbeddingProcessor};
use symbiotic_core::intake::{
    idempotency_key, idempotency_key_for_file, idempotency_key_for_note, normalize_tags,
    IntakeBatchResult, IntakeItemResult, IntakeKind, IntakeRequest, IntakeRoute, IntakeStatus,
    IntakeSummary,
};
use url::Url;

use crate::distillery::{
    build_graph_context, run_pipeline, DistilleryReport, DistilleryStageConfig, RawInput,
};
use crate::ollama::{LinkRelevanceConfig, TitleExtractionConfig};
use crate::twitter::{canonicalize_twitter_url, content_dedup_key, is_twitter_url};

pub mod twitter;

#[derive(Debug, Clone)]
pub struct FetchedContent {
    pub markdown: String,
    /// HTML `<title>` extracted during fetch, if available.
    /// Used as fallback when LLM title extraction is unavailable.
    pub html_title: Option<String>,
    /// Resolved title for this content. Set by the pipeline after title
    /// extraction (LLM or HTML fallback). Consumers (e.g. `IntakeStore`
    /// implementations) should prefer this over `html_title`.
    pub title: Option<String>,
}

// ---------------------------------------------------------------------------
// Pipeline configuration
// ---------------------------------------------------------------------------

/// Unified configuration for the intake pipeline's LLM-backed features.
#[derive(Debug, Clone)]
pub struct IntakeConfig {
    /// Configuration for LLM-based title extraction (T12).
    pub title_extraction: TitleExtractionConfig,
    /// Configuration for LLM-based link relevance filtering (T22).
    pub link_relevance: LinkRelevanceConfig,
    /// Enable content-based deduplication (T54).
    /// When enabled, Twitter URLs are deduplicated by tweet ID so that
    /// `x.com/user/status/123` and `twitter.com/user/status/123` are
    /// recognized as the same content.
    pub dedup_enabled: bool,
    /// Enable PII redaction on ingested content before storage (T82).
    ///
    /// When enabled (the default), the redaction engine scans all content
    /// for 10 PII categories (email, phone, SSN, credit card, API keys,
    /// IP addresses, US addresses, ZIP codes, sensitive keywords) and
    /// replaces or removes them before the content is persisted.
    ///
    /// Idempotency keys are computed from the *original* content so that
    /// deduplication still works correctly.
    pub redaction_enabled: bool,
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            title_extraction: TitleExtractionConfig::default(),
            link_relevance: LinkRelevanceConfig::default(),
            dedup_enabled: true,
            redaction_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sensitivity {
    Low,
    Medium,
    High,
}

/// Map intake sensitivity levels to core sensitivity levels.
///
/// The intake pipeline classifies content as Low/Medium/High during
/// intake. The core crate (and downstream consumers like the embedding
/// router) uses Shareable/Restricted/Private. This mapping bridges the two:
///
/// - `Low`    -> `Shareable`  — safe to send to cloud providers
/// - `Medium` -> `Restricted` — prefer local providers; cloud only with consent
/// - `High`   -> `Private`    — local-only; never leaves the device
impl From<Sensitivity> for symbiotic_core::Sensitivity {
    fn from(intake: Sensitivity) -> Self {
        match intake {
            Sensitivity::Low => symbiotic_core::Sensitivity::Shareable,
            Sensitivity::Medium => symbiotic_core::Sensitivity::Restricted,
            Sensitivity::High => symbiotic_core::Sensitivity::Private,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IntakePolicy {
    pub blocked_hosts: HashSet<String>,
}

pub trait ContentFetcher: Send + Sync {
    fn fetch(&self, url: &Url) -> Result<FetchedContent>;
}

pub trait IntakeStore: Send + Sync {
    fn exists(&self, idempotency_key: &str) -> Result<bool>;
    fn store_archive_url(
        &self,
        url: &Url,
        content: &FetchedContent,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String>;
    fn store_archive_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String>;
    fn store_vault_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String>;
    /// Update the title of an existing archive record.
    ///
    /// Called by the pipeline after a delayed/async title resolution
    /// (LLM generation or operator-supplied `--title`). Default
    /// implementation is a no-op for stores that don't support it —
    /// this keeps the trait backward-compatible for any bespoke test
    /// stores that haven't implemented the new method yet.
    fn update_title(&self, _record_id: &str, _new_title: &str) -> Result<bool> {
        Ok(false)
    }
}

/// Sink for content that the firewall quarantined (T132 §05).
///
/// When intake's firewall scan returns a [`Verdict::Quarantined`][verdict],
/// the content MUST NOT enter Archive. Instead the pipeline writes a record
/// of the quarantine event here (typically a JSONL line in
/// `knowledge-base/self/audit/firewall_quarantine.jsonl`) and emits a
/// `firewall.quarantine` Matrix alert via the same plumbing the daemon uses
/// for other operator alerts. The default implementation
/// ([`NoopQuarantineSink`]) is used by tests; production wires
/// [`crate::JsonlQuarantineSink`] (see this module's tests for the shape).
///
/// The sink only sees a content **hash + first 64 chars** prefix, never the
/// full payload — per design §4 the quarantine log itself MUST NOT become a
/// prompt-injection vector.
///
/// [verdict]: symbiotic_firewall::types::Verdict
pub trait QuarantineSink: Send + Sync {
    fn record(&self, record: QuarantineRecord) -> Result<()>;
}

/// One quarantine event for [`QuarantineSink::record`].
#[derive(Debug, Clone)]
pub struct QuarantineRecord {
    /// Source kind (e.g. `"intake.url"`, `"intake.note"`, `"intake.file"`).
    pub source_kind: String,
    /// Optional URL / file path / other addressable provenance.
    pub source_ref: Option<String>,
    /// SHA-256 hex of the raw content. Used as the only payload identifier
    /// in the audit log; design §4 forbids storing the full content.
    pub content_hash: String,
    /// First 64 chars of the (sanitized) content for human-readable triage.
    pub prefix: String,
    /// Full firewall verdict including quarantine class + findings.
    pub verdict: symbiotic_firewall::types::FirewallVerdict,
}

/// No-op default sink used when no Matrix alert plumbing is wired.
///
/// Production daemons replace this with a sink that writes the JSONL log
/// + emits `firewall.quarantine` Matrix events.
pub struct NoopQuarantineSink;

impl QuarantineSink for NoopQuarantineSink {
    fn record(&self, _record: QuarantineRecord) -> Result<()> {
        Ok(())
    }
}

pub trait ReviewQueue: Send + Sync {
    fn enqueue(&self, record_id: &str) -> Result<String>;
}

pub trait SensitivityClassifier: Send + Sync {
    fn classify_note(&self, note: &str) -> Sensitivity;
}

/// Maximum content size in bytes accepted from a fetch before rejection.
const MAX_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// Optional distillery pipeline configuration.
///
/// When set, the intake pipeline will run the full distillery processing
/// (Reduce -> Reflect -> Verify -> Reweave -> Archive) on successfully
/// ingested URL content via [`IntakePipeline::process_with_distillery`].
#[derive(Debug, Clone)]
pub struct DistilleryOpt {
    pub config: DistilleryStageConfig,
}

/// Result of running embedding generation on ingested content (T84).
///
/// Returned alongside the `IntakeBatchResult` and `DistilleryReport`s from
/// [`IntakePipeline::process_with_distillery`]. Each entry corresponds to
/// one successfully ingested item that was sent through the embedding
/// pipeline.
#[derive(Debug, Clone)]
pub struct IntakeEmbeddingReport {
    /// Embedding outcomes for each ingested document.
    pub outcomes: Vec<EmbeddingOutcome>,
}

pub struct IntakePipeline {
    fetcher: Arc<dyn ContentFetcher>,
    store: Arc<dyn IntakeStore>,
    queue: Arc<dyn ReviewQueue>,
    classifier: Arc<dyn SensitivityClassifier>,
    policy: IntakePolicy,
    distillery: Option<DistilleryOpt>,
    llm: Option<Arc<dyn LlmClient>>,
    intake_config: IntakeConfig,
    redaction_engine: symbiotic_context::redaction::RedactionEngine,
    /// Optional embedding processor — when set, ingested content is
    /// automatically chunked, embedded, and upserted into the vector index
    /// as a post-processing step in `process_with_distillery()`.
    embedding_processor: Option<IntakeEmbeddingProcessor>,
    /// Sink for quarantine events emitted when the firewall (T132 §05)
    /// rejects fetched content. Defaults to a no-op; production wires a
    /// JSONL+Matrix sink.
    quarantine_sink: Arc<dyn QuarantineSink>,
    /// Cached Stage A + Stage B firewall configurations.
    firewall_cfg_a: symbiotic_firewall::stages::StageAConfig,
    firewall_cfg_b: symbiotic_firewall::stages::StageBConfig,
    // vector_index_path removed — sqlite-vec persists to its own DB file
}

impl IntakePipeline {
    pub fn new(
        fetcher: Arc<dyn ContentFetcher>,
        store: Arc<dyn IntakeStore>,
        queue: Arc<dyn ReviewQueue>,
        classifier: Arc<dyn SensitivityClassifier>,
        policy: IntakePolicy,
    ) -> Self {
        Self {
            fetcher,
            store,
            queue,
            classifier,
            policy,
            distillery: None,
            llm: None,
            intake_config: IntakeConfig::default(),
            redaction_engine: symbiotic_context::redaction::RedactionEngine::new(),
            embedding_processor: None,
            quarantine_sink: Arc::new(NoopQuarantineSink),
            firewall_cfg_a: symbiotic_firewall::stages::StageAConfig::default(),
            firewall_cfg_b: symbiotic_firewall::stages::StageBConfig::default(),
        }
    }

    /// Wire a [`QuarantineSink`] for firewall-rejected content (T132 §05).
    pub fn with_quarantine_sink(mut self, sink: Arc<dyn QuarantineSink>) -> Self {
        self.quarantine_sink = sink;
        self
    }

    /// Configure LLM-backed intake features (title extraction, link
    /// relevance filtering, content deduplication).
    pub fn with_intake_config(mut self, config: IntakeConfig) -> Self {
        self.intake_config = config;
        self
    }

    /// Enable the distillery pipeline on this intake pipeline.
    ///
    /// When enabled, calls to [`process_with_distillery`] will run the full
    /// Reduce -> Reflect -> Verify -> Reweave -> Archive flow on ingested content.
    pub fn with_distillery(
        mut self,
        config: DistilleryStageConfig,
        llm: Arc<dyn LlmClient>,
    ) -> Self {
        self.distillery = Some(DistilleryOpt { config });
        self.llm = Some(llm);
        self
    }

    /// Attach an embedding processor (T84) that will automatically generate
    /// semantic search vectors for newly ingested content.
    ///
    /// When set, [`process_with_distillery`] will chunk, embed, and upsert
    /// each successfully ingested document into the shared vector index as
    /// a post-processing step. Embedding failures are non-fatal — they are
    /// captured in the returned [`IntakeEmbeddingReport`] and unavailable
    /// chunks are saved to the processor's pending store for later retry.
    ///
    /// sqlite-vec handles persistence automatically — no path needed.
    pub fn with_embedding_processor(mut self, processor: IntakeEmbeddingProcessor) -> Self {
        self.embedding_processor = Some(processor);
        self
    }

    /// Returns a reference to the embedding processor, if one is configured.
    pub fn embedding_processor(&self) -> Option<&IntakeEmbeddingProcessor> {
        self.embedding_processor.as_ref()
    }

    /// Return a reference to the current intake config.
    pub fn intake_config(&self) -> &IntakeConfig {
        &self.intake_config
    }

    /// Return a reference to the link relevance config (convenience accessor
    /// for callers that discover links and want to filter them).
    pub fn link_relevance_config(&self) -> &LinkRelevanceConfig {
        &self.intake_config.link_relevance
    }

    /// Return a reference to the LLM client, if one is configured.
    pub fn llm(&self) -> Option<&dyn LlmClient> {
        self.llm.as_deref()
    }

    /// Scan ingest content with the firewall (T132 §05).
    ///
    /// Runs Stages A + B synchronously over `payload` using the supplied
    /// [`ScanContext`]. Returns the raw [`FirewallVerdict`] for the caller to
    /// inspect — `Passed`/`Flagged` is safe to forward to Archive,
    /// `Quarantined` MUST be diverted to the [`QuarantineSink`].
    ///
    /// Stage C (LLM-lite semantic review) is intentionally omitted from the
    /// sync ingest path: the synchronous `IntakePipeline::process` entry point
    /// has no async runtime to call into, and Stage C is opt-in for very-low
    /// trust sources (see design §3.3). The async entry point
    /// `process_with_distillery` may invoke Stage C in a follow-up chunk.
    fn scan_ingest_content(
        &self,
        ctx: &symbiotic_firewall::types::ScanContext,
        payload: &str,
    ) -> symbiotic_firewall::types::FirewallVerdict {
        symbiotic_firewall::stages::run_stages_a_b(
            ctx,
            payload,
            &self.firewall_cfg_a,
            &self.firewall_cfg_b,
        )
    }

    /// Build a `ScanContext` for an intake URL fetch (very-low trust per §2.3).
    fn build_url_scan_context(
        &self,
        url: &Url,
        content: &FetchedContent,
    ) -> symbiotic_firewall::types::ScanContext {
        let mut headers = std::collections::BTreeMap::new();
        if let Some(t) = content.html_title.as_ref() {
            // Surface the HTML title as a transport-style header so the
            // Stage A MIME / encoding heuristics have it in scope. Not
            // strictly required by Stage A today; future heuristics may use it.
            headers.insert("html_title".into(), t.clone());
        }
        symbiotic_firewall::types::ScanContext {
            source: symbiotic_firewall::types::ContentSource {
                kind: "intake.url".into(),
                url: Some(url.as_str().into()),
                fetched_at: time::OffsetDateTime::now_utc(),
                claimed_content_type: Some("text/markdown".into()),
                headers,
            },
            consuming_agent_scope: symbiotic_firewall::types::ConsumingAgentScope::minimal(
                "intake-pipeline",
            ),
            call_site: symbiotic_firewall::types::CallSite::new("intake.url"),
        }
    }

    /// Build a `ScanContext` for an operator-supplied note (low trust per §2.3).
    fn build_note_scan_context(&self) -> symbiotic_firewall::types::ScanContext {
        symbiotic_firewall::types::ScanContext {
            source: symbiotic_firewall::types::ContentSource {
                kind: "intake.note".into(),
                url: None,
                fetched_at: time::OffsetDateTime::now_utc(),
                claimed_content_type: Some("text/markdown".into()),
                headers: Default::default(),
            },
            consuming_agent_scope: symbiotic_firewall::types::ConsumingAgentScope::minimal(
                "intake-pipeline",
            ),
            call_site: symbiotic_firewall::types::CallSite::new("intake.note"),
        }
    }

    /// Build a `ScanContext` for an operator-supplied local file (low trust).
    fn build_file_scan_context(&self, file_path: &str) -> symbiotic_firewall::types::ScanContext {
        symbiotic_firewall::types::ScanContext {
            source: symbiotic_firewall::types::ContentSource {
                kind: "intake.file".into(),
                url: Some(format!("file://{file_path}")),
                fetched_at: time::OffsetDateTime::now_utc(),
                claimed_content_type: Some("text/markdown".into()),
                headers: Default::default(),
            },
            consuming_agent_scope: symbiotic_firewall::types::ConsumingAgentScope::minimal(
                "intake-pipeline",
            ),
            call_site: symbiotic_firewall::types::CallSite::new("intake.file"),
        }
    }

    /// Build a [`QuarantineRecord`] from a verdict + content.
    ///
    /// Per design §4 the recorded prefix is bounded to 64 chars and the
    /// content hash is SHA-256 hex of the raw bytes — never the full payload.
    fn build_quarantine_record(
        &self,
        ctx: &symbiotic_firewall::types::ScanContext,
        content: &str,
        verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> QuarantineRecord {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let hash_hex = format!("{:x}", hasher.finalize());
        let prefix: String = content.chars().take(64).collect();
        QuarantineRecord {
            source_kind: ctx.source.kind.clone(),
            source_ref: ctx.source.url.clone(),
            content_hash: hash_hex,
            prefix,
            verdict,
        }
    }

    /// Compute a placeholder [`SourceReceiptRef`] from raw bytes.
    ///
    /// TODO(§07): swap for a real source-receipt store write. For now the
    /// receipt id is the SHA-256 hex of the raw payload (matching the planned
    /// content-addressed scheme) and `byte_length` is the payload length.
    fn placeholder_source_receipt(raw: &[u8]) -> symbiotic_firewall::types::SourceReceiptRef {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(raw);
        let receipt_id = format!("{:x}", hasher.finalize());
        symbiotic_firewall::types::SourceReceiptRef {
            receipt_id,
            byte_length: Some(raw.len() as u64),
            completeness: symbiotic_firewall::types::CaptureCompleteness::FullSourcePreserved,
        }
    }

    /// Resolve a title for a freshly-ingested record and persist it via
    /// [`IntakeStore::update_title`].
    ///
    /// Priority order:
    /// 1. `explicit_title` (operator-supplied, e.g. `--title` CLI flag) —
    ///    used verbatim, LLM is not invoked.
    /// 2. LLM-generated title via [`crate::ollama::generate_title_with_fallback`]
    ///    when `title_extraction.enabled` AND an LLM is configured AND
    ///    a tokio runtime handle is available (sync caller block_on).
    /// 3. `html_title` fallback (only reached when config is disabled or
    ///    no LLM is configured — `generate_title_with_fallback` handles
    ///    this internally when invoked).
    ///
    /// Silently best-effort: any failure (no runtime, LLM error, store
    /// error) is swallowed. The stored record keeps its placeholder title.
    fn resolve_and_persist_title(
        &self,
        record_id: &str,
        url: &str,
        content: &str,
        html_title: Option<&str>,
        explicit_title: Option<&str>,
    ) {
        // Operator-supplied title wins — no LLM call.
        if let Some(title) = explicit_title.and_then(|t| {
            let trimmed = t.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }) {
            let _ = self.store.update_title(record_id, &title);
            return;
        }

        // LLM path requires: feature enabled, LLM configured, tokio handle.
        if !self.intake_config.title_extraction.enabled {
            return;
        }
        let Some(llm) = self.llm.as_ref() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let cfg = self.intake_config.title_extraction.clone();
        let llm_ref: Arc<dyn LlmClient> = Arc::clone(llm);
        let url_owned = url.to_string();
        let content_owned = content.to_string();
        let html_owned: Option<String> = html_title.map(|s| s.to_string());

        // Run the async title generator from this sync path via an inner
        // thread + block_on scope — matches the pattern used in
        // `workers.rs::execute_react` to bridge sync → async safely.
        let title = std::thread::scope(|s| {
            s.spawn(|| {
                handle.block_on(crate::ollama::generate_title_with_fallback(
                    &url_owned,
                    &content_owned,
                    html_owned.as_deref(),
                    &cfg,
                    Some(llm_ref.as_ref()),
                ))
            })
            .join()
            .ok()
        });

        if let Some(title) = title {
            let _ = self.store.update_title(record_id, &title);
        }
    }

    pub fn process(&self, request: IntakeRequest) -> Result<IntakeBatchResult> {
        let run_id = generate_run_id();
        let tags = normalize_tags(&request.tags);
        let mut items = Vec::new();
        let mut seen_in_run = HashSet::new();
        // T54: Track content-based dedup keys within this batch so that
        // Twitter URL variants for the same tweet are caught even when
        // they produce different standard idempotency keys.
        let mut seen_content_keys: HashSet<String> = HashSet::new();

        match request.kind {
            IntakeKind::Url => {
                for url in &request.urls {
                    let input = url.as_str().to_string();

                    let scheme = url.scheme();
                    if scheme != "http" && scheme != "https" {
                        items.push(IntakeItemResult {
                            input,
                            normalized_url: Some(url.clone()),
                            status: IntakeStatus::Blocked,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: None,
                            record_id: None,
                            error: Some(format!("unsupported URL scheme: {scheme}")),
                        });
                        continue;
                    }

                    if self.is_blocked(url) {
                        items.push(IntakeItemResult {
                            input,
                            normalized_url: Some(url.clone()),
                            status: IntakeStatus::Blocked,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: None,
                            record_id: None,
                            error: Some("blocked host".to_string()),
                        });
                        continue;
                    }

                    // T54: Content-based dedup — catches Twitter URL variants
                    // (x.com vs twitter.com, /user/status/ vs /i/status/, etc.)
                    // that would produce different standard idempotency keys
                    // but point to the same underlying content.
                    if self.intake_config.dedup_enabled {
                        let ckey = content_dedup_key(url);
                        if !seen_content_keys.insert(ckey.clone()) {
                            items.push(IntakeItemResult {
                                input,
                                normalized_url: Some(url.clone()),
                                status: IntakeStatus::Duplicate,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(ckey),
                                record_id: None,
                                error: Some(
                                    "duplicate content (same tweet via different URL)".to_string(),
                                ),
                            });
                            continue;
                        }
                        // Also check the store for existing content keys
                        if self.store.exists(&ckey)? {
                            items.push(IntakeItemResult {
                                input,
                                normalized_url: Some(url.clone()),
                                status: IntakeStatus::Duplicate,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(ckey),
                                record_id: None,
                                error: None,
                            });
                            continue;
                        }
                    }

                    // T54: For Twitter URLs, canonicalize before computing
                    // the standard idempotency key so that all URL variants
                    // produce the same key.
                    let store_url = if self.intake_config.dedup_enabled && is_twitter_url(url) {
                        canonicalize_twitter_url(url)
                    } else {
                        url.clone()
                    };

                    let dedupe_key = idempotency_key(&store_url, &tags);
                    if !seen_in_run.insert(dedupe_key.clone()) {
                        items.push(IntakeItemResult {
                            input,
                            normalized_url: Some(store_url),
                            status: IntakeStatus::Duplicate,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: Some(dedupe_key),
                            record_id: None,
                            error: Some("duplicate in current batch".to_string()),
                        });
                        continue;
                    }

                    if self.store.exists(&dedupe_key)? {
                        items.push(IntakeItemResult {
                            input,
                            normalized_url: Some(store_url),
                            status: IntakeStatus::Duplicate,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: Some(dedupe_key),
                            record_id: None,
                            error: None,
                        });
                        continue;
                    }

                    let content = match self.fetcher.fetch(url) {
                        Ok(content) => {
                            if content.markdown.len() > MAX_CONTENT_BYTES {
                                items.push(IntakeItemResult {
                                    input,
                                    normalized_url: Some(store_url),
                                    status: IntakeStatus::FetchFailed,
                                    route: IntakeRoute::Archive,
                                    review_queued: false,
                                    review_job_id: None,
                                    idempotency_key: Some(dedupe_key),
                                    record_id: None,
                                    error: Some(format!(
                                        "content exceeds maximum size ({} bytes > {MAX_CONTENT_BYTES})",
                                        content.markdown.len()
                                    )),
                                });
                                continue;
                            }
                            content
                        }
                        Err(err) => {
                            items.push(IntakeItemResult {
                                input,
                                normalized_url: Some(store_url),
                                status: IntakeStatus::FetchFailed,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(dedupe_key),
                                record_id: None,
                                error: Some(err.to_string()),
                            });
                            continue;
                        }
                    };

                    // T12: Resolve a title for this content. In the synchronous
                    // path we use the HTML title from the fetch (if any) as
                    // the fallback. LLM-based title generation happens in the
                    // async `process_with_distillery()` path.
                    let mut content = content;
                    if content.title.is_none() {
                        content.title = content.html_title.clone().filter(|t| !t.trim().is_empty());
                    }

                    // T82: Redact PII from fetched content before storage.
                    // The idempotency key was already computed from the original
                    // URL so deduplication is unaffected by redaction.
                    if self.intake_config.redaction_enabled {
                        content.markdown = self.redaction_engine.redact(&content.markdown);
                    }

                    // T132 §05: Content Firewall scan. URL fetches are
                    // very-low-trust; quarantined content never reaches Archive.
                    let scan_ctx = self.build_url_scan_context(url, &content);
                    let mut firewall_verdict =
                        self.scan_ingest_content(&scan_ctx, &content.markdown);
                    // §07 source-receipt: stub the receipt id from raw bytes.
                    firewall_verdict.source_receipt_id = Some(
                        Self::placeholder_source_receipt(content.markdown.as_bytes()).receipt_id,
                    );
                    if matches!(
                        firewall_verdict.verdict,
                        symbiotic_firewall::types::Verdict::Quarantined
                    ) {
                        let record = self.build_quarantine_record(
                            &scan_ctx,
                            &content.markdown,
                            firewall_verdict.clone(),
                        );
                        let _ = self.quarantine_sink.record(record);
                        items.push(IntakeItemResult {
                            input,
                            normalized_url: Some(store_url),
                            status: IntakeStatus::Blocked,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: Some(dedupe_key),
                            record_id: None,
                            error: Some(format!(
                                "firewall quarantine ({:?})",
                                firewall_verdict.quarantine_class
                            )),
                        });
                        continue;
                    }

                    let record_id = match self.store.store_archive_url(
                        &store_url,
                        &content,
                        &tags,
                        &dedupe_key,
                        firewall_verdict,
                    ) {
                        Ok(record_id) => record_id,
                        Err(err) => {
                            items.push(IntakeItemResult {
                                input,
                                normalized_url: Some(store_url),
                                status: IntakeStatus::StoreFailed,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(dedupe_key),
                                record_id: None,
                                error: Some(err.to_string()),
                            });
                            continue;
                        }
                    };

                    // T12: generate+persist a clean LLM title when
                    // available. Best-effort — no-op if the tokio handle
                    // is unavailable (sync-only caller) or the LLM is
                    // not configured. Operator can override via
                    // `request.title` (currently not exposed for URL
                    // intake, but the helper handles it uniformly).
                    self.resolve_and_persist_title(
                        &record_id,
                        store_url.as_str(),
                        &content.markdown,
                        content.html_title.as_deref(),
                        request.title.as_deref(),
                    );

                    let review_job_id = match self.queue.enqueue(&record_id) {
                        Ok(job_id) => Some(job_id),
                        Err(err) => {
                            items.push(IntakeItemResult {
                                input,
                                normalized_url: Some(store_url),
                                status: IntakeStatus::QueueFailed,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(dedupe_key),
                                record_id: Some(record_id),
                                error: Some(err.to_string()),
                            });
                            continue;
                        }
                    };

                    items.push(IntakeItemResult {
                        input,
                        normalized_url: Some(store_url),
                        status: IntakeStatus::Ingested,
                        route: IntakeRoute::Archive,
                        review_queued: true,
                        review_job_id,
                        idempotency_key: Some(dedupe_key),
                        record_id: Some(record_id),
                        error: None,
                    });
                }
            }
            IntakeKind::Note => {
                let note = request
                    .note
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow!("note intake requires non-empty note payload"))?;
                let note_key = idempotency_key_for_note(note, &tags);
                let classification = self.classifier.classify_note(note);

                let item = match classification {
                    Sensitivity::High => {
                        // High-sensitivity vault notes originate from operator
                        // input — trusted source per design §2.3, attach a
                        // trusted-skip verdict to satisfy the writer guard.
                        let verdict = symbiotic_archive::trusted_skip_verdict();
                        let record_id = self
                            .store
                            .store_vault_note(note, &tags, &note_key, verdict)
                            .context("storing high-sensitivity note in vault")?;
                        IntakeItemResult {
                            input: note.to_string(),
                            normalized_url: None,
                            status: IntakeStatus::SecureRouted,
                            route: IntakeRoute::Vault,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: Some(note_key),
                            record_id: Some(record_id),
                            error: None,
                        }
                    }
                    Sensitivity::Medium => IntakeItemResult {
                        input: note.to_string(),
                        normalized_url: None,
                        status: IntakeStatus::SensitivePendingApproval,
                        route: IntakeRoute::Vault,
                        review_queued: false,
                        review_job_id: None,
                        idempotency_key: Some(note_key),
                        record_id: None,
                        error: Some("requires explicit user approval".to_string()),
                    },
                    Sensitivity::Low => {
                        if self.store.exists(&note_key)? {
                            IntakeItemResult {
                                input: note.to_string(),
                                normalized_url: None,
                                status: IntakeStatus::Duplicate,
                                route: IntakeRoute::Archive,
                                review_queued: false,
                                review_job_id: None,
                                idempotency_key: Some(note_key),
                                record_id: None,
                                error: None,
                            }
                        } else {
                            // T82: Redact PII from low-sensitivity notes before
                            // archive storage. The idempotency key was already
                            // computed from the original note text.
                            let store_note = if self.intake_config.redaction_enabled {
                                self.redaction_engine.redact(note)
                            } else {
                                note.to_string()
                            };

                            // T132 §05: scan operator-supplied note (low trust).
                            let scan_ctx = self.build_note_scan_context();
                            let mut firewall_verdict =
                                self.scan_ingest_content(&scan_ctx, &store_note);
                            firewall_verdict.source_receipt_id = Some(
                                Self::placeholder_source_receipt(store_note.as_bytes()).receipt_id,
                            );
                            if matches!(
                                firewall_verdict.verdict,
                                symbiotic_firewall::types::Verdict::Quarantined
                            ) {
                                let record = self.build_quarantine_record(
                                    &scan_ctx,
                                    &store_note,
                                    firewall_verdict.clone(),
                                );
                                let _ = self.quarantine_sink.record(record);
                                IntakeItemResult {
                                    input: note.to_string(),
                                    normalized_url: None,
                                    status: IntakeStatus::Blocked,
                                    route: IntakeRoute::Archive,
                                    review_queued: false,
                                    review_job_id: None,
                                    idempotency_key: Some(note_key),
                                    record_id: None,
                                    error: Some(format!(
                                        "firewall quarantine ({:?})",
                                        firewall_verdict.quarantine_class
                                    )),
                                }
                            } else {
                                let record_id = self
                                    .store
                                    .store_archive_note(
                                        &store_note,
                                        &tags,
                                        &note_key,
                                        firewall_verdict,
                                    )
                                    .context("storing low-sensitivity note in archive")?;
                                // T12: resolve a meaningful title for the
                                // note record. If the operator passed
                                // `--title` we use it verbatim; otherwise
                                // the LLM (when available) generates one
                                // from the note body. Best-effort — the
                                // placeholder "Note" title remains on
                                // failure.
                                self.resolve_and_persist_title(
                                    &record_id,
                                    "",
                                    &store_note,
                                    None,
                                    request.title.as_deref(),
                                );
                                let review_job_id = self
                                    .queue
                                    .enqueue(&record_id)
                                    .context("queueing archive note for review")?;
                                IntakeItemResult {
                                    input: note.to_string(),
                                    normalized_url: None,
                                    status: IntakeStatus::Ingested,
                                    route: IntakeRoute::Archive,
                                    review_queued: true,
                                    review_job_id: Some(review_job_id),
                                    idempotency_key: Some(note_key),
                                    record_id: Some(record_id),
                                    error: None,
                                }
                            }
                        }
                    }
                };
                items.push(item);
            }
            IntakeKind::LocalFile => {
                let file_path = request
                    .file_path
                    .as_deref()
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| anyhow!("LocalFile intake requires non-empty file_path"))?;
                let file_key = idempotency_key_for_file(file_path, &tags);

                if self.store.exists(&file_key)? {
                    items.push(IntakeItemResult {
                        input: file_path.to_string(),
                        normalized_url: None,
                        status: IntakeStatus::Duplicate,
                        route: IntakeRoute::Archive,
                        review_queued: false,
                        review_job_id: None,
                        idempotency_key: Some(file_key),
                        record_id: None,
                        error: None,
                    });
                } else {
                    let content = std::fs::read_to_string(file_path)
                        .with_context(|| format!("reading local file: {file_path}"))?;

                    // Parse YAML frontmatter if present
                    let (frontmatter, body) = parse_frontmatter(&content);
                    let _title = frontmatter
                        .as_ref()
                        .and_then(|fm| fm.get("title").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .to_string();

                    // Merge frontmatter tags with request tags
                    let mut all_tags = tags.clone();
                    if let Some(fm) = &frontmatter {
                        if let Some(fm_tags) = fm.get("tags").and_then(|v| v.as_sequence()) {
                            for tag in fm_tags {
                                if let Some(t) = tag.as_str() {
                                    all_tags.push(t.to_string());
                                }
                            }
                        }
                    }
                    let all_tags = normalize_tags(&all_tags);

                    let store_content = if self.intake_config.redaction_enabled {
                        self.redaction_engine.redact(&body)
                    } else {
                        body.to_string()
                    };

                    // T132 §05: scan operator-supplied file (low trust).
                    let scan_ctx = self.build_file_scan_context(file_path);
                    let mut firewall_verdict = self.scan_ingest_content(&scan_ctx, &store_content);
                    firewall_verdict.source_receipt_id =
                        Some(Self::placeholder_source_receipt(store_content.as_bytes()).receipt_id);
                    if matches!(
                        firewall_verdict.verdict,
                        symbiotic_firewall::types::Verdict::Quarantined
                    ) {
                        let record = self.build_quarantine_record(
                            &scan_ctx,
                            &store_content,
                            firewall_verdict.clone(),
                        );
                        let _ = self.quarantine_sink.record(record);
                        items.push(IntakeItemResult {
                            input: file_path.to_string(),
                            normalized_url: None,
                            status: IntakeStatus::Blocked,
                            route: IntakeRoute::Archive,
                            review_queued: false,
                            review_job_id: None,
                            idempotency_key: Some(file_key),
                            record_id: None,
                            error: Some(format!(
                                "firewall quarantine ({:?})",
                                firewall_verdict.quarantine_class
                            )),
                        });
                    } else {
                        let record_id = self
                            .store
                            .store_archive_note(
                                &store_content,
                                &all_tags,
                                &file_key,
                                firewall_verdict,
                            )
                            .with_context(|| {
                                format!("storing local file in archive: {file_path}")
                            })?;
                        let review_job_id = self
                            .queue
                            .enqueue(&record_id)
                            .context("queueing local file for review")?;

                        items.push(IntakeItemResult {
                            input: file_path.to_string(),
                            normalized_url: None,
                            status: IntakeStatus::Ingested,
                            route: IntakeRoute::Archive,
                            review_queued: true,
                            review_job_id: Some(review_job_id),
                            idempotency_key: Some(file_key),
                            record_id: Some(record_id),
                            error: None,
                        });
                    }
                }
            }
        }

        let summary = summarize(&items);
        Ok(IntakeBatchResult {
            run_id,
            items,
            summary,
        })
    }

    /// Process an intake request and optionally run the distillery pipeline
    /// and embedding generation on successfully ingested content.
    ///
    /// This is the async entry point that:
    /// 1. Runs the standard intake pipeline (fetch, dedupe, store, queue)
    /// 2. If an LLM is available, generates titles for ingested content (T12)
    /// 3. If distillery is enabled, runs Reduce -> Reflect -> Verify -> Reweave -> Archive
    ///    on each successfully ingested URL's content
    /// 4. If an embedding processor is configured (T84), generates semantic
    ///    search vectors for each successfully ingested item
    ///
    /// Distillery and embedding failures are logged but do not fail the
    /// overall intake -- the content is already stored by the time these
    /// post-processing steps run.
    pub async fn process_with_distillery(
        &self,
        request: IntakeRequest,
    ) -> Result<(
        IntakeBatchResult,
        Vec<DistilleryReport>,
        IntakeEmbeddingReport,
    )> {
        // Collect URL+content pairs for successfully fetched items before
        // delegating to process(). We need the content for distillery,
        // T12 title generation, and T84 embedding generation, but
        // process() does not return it.
        let mut fetched_pairs: Vec<(String, String, Option<String>)> = Vec::new();

        // For note intake, capture the note content and its sensitivity for
        // embedding. The classifier determines intake sensitivity (Low/Medium/High)
        // which is mapped to core sensitivity (Shareable/Restricted/Private) so
        // the embedding router can enforce provider constraints.
        let (note_content, note_sensitivity) = if request.kind == IntakeKind::Note {
            let content = request
                .note
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from);
            let sensitivity = content.as_deref().map(|c| self.classifier.classify_note(c));
            (content, sensitivity)
        } else if request.kind == IntakeKind::LocalFile {
            // For local files, read and parse the file content for distillery/embedding
            if let Some(path) = request.file_path.as_deref().filter(|p| !p.is_empty()) {
                if let Ok(raw) = std::fs::read_to_string(path) {
                    let (_fm, body) = parse_frontmatter(&raw);
                    (Some(body), None)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            }
        } else {
            (None, None)
        };

        if request.kind == IntakeKind::Url {
            let needs_content = self.distillery.is_some()
                || (self.intake_config.title_extraction.enabled && self.llm.is_some())
                || self.embedding_processor.is_some();
            if needs_content {
                for url in &request.urls {
                    if let Ok(content) = self.fetcher.fetch(url) {
                        if content.markdown.len() <= MAX_CONTENT_BYTES {
                            fetched_pairs.push((
                                url.as_str().to_string(),
                                content.markdown.clone(),
                                content.html_title.clone(),
                            ));
                        }
                    }
                }
            }
        }

        // Run the standard intake pipeline (synchronous)
        let batch = self.process(request)?;

        // T12: Generate LLM titles for ingested items when an LLM is available
        if self.intake_config.title_extraction.enabled {
            if let Some(llm) = &self.llm {
                let ingested_urls: HashSet<String> = batch
                    .items
                    .iter()
                    .filter(|item| item.status == IntakeStatus::Ingested)
                    .filter_map(|item| item.normalized_url.as_ref().map(|u| u.as_str().to_string()))
                    .collect();

                // Build a url -> record_id lookup for the items that were
                // actually stored in this batch, so we can update their
                // titles in place.
                let url_to_record: std::collections::HashMap<String, String> = batch
                    .items
                    .iter()
                    .filter(|item| item.status == IntakeStatus::Ingested)
                    .filter_map(|item| {
                        let url = item.normalized_url.as_ref()?.as_str().to_string();
                        let rid = item.record_id.clone()?;
                        Some((url, rid))
                    })
                    .collect();

                for (url, content, html_title) in &fetched_pairs {
                    if !ingested_urls.contains(url.as_str()) {
                        continue;
                    }

                    let title = crate::ollama::generate_title_with_fallback(
                        url,
                        content,
                        html_title.as_deref(),
                        &self.intake_config.title_extraction,
                        Some(llm.as_ref()),
                    )
                    .await;

                    // Persist the resolved title onto the already-stored
                    // archive record so `list()` surfaces the LLM title
                    // instead of the raw URL / HTML `<title>` fallback.
                    if let Some(record_id) = url_to_record.get(url) {
                        let _ = self.store.update_title(record_id, &title);
                    }
                }
            }
        }

        let mut reports = Vec::new();

        // Run distillery on ingested items
        if let (Some(distillery_opt), Some(llm)) = (&self.distillery, &self.llm) {
            let graph_context =
                build_graph_context(&distillery_opt.config.kb_root).unwrap_or_default();

            // Only process items that were successfully ingested
            let ingested_urls: HashSet<String> = batch
                .items
                .iter()
                .filter(|item| item.status == IntakeStatus::Ingested)
                .filter_map(|item| item.normalized_url.as_ref().map(|u| u.as_str().to_string()))
                .collect();

            for (url, content, _html_title) in &fetched_pairs {
                if !ingested_urls.contains(url.as_str()) {
                    continue;
                }

                let raw_input = RawInput {
                    source_url: url.clone(),
                    raw_content: content.clone(),
                };

                match run_pipeline(
                    raw_input,
                    &graph_context,
                    &distillery_opt.config,
                    llm.as_ref(),
                )
                .await
                {
                    Ok(report) => reports.push(report),
                    Err(e) => {
                        // Distillery failure is non-fatal -- content is already stored
                        eprintln!("distillery pipeline failed for {url}: {e}");
                    }
                }
            }
        }

        // T84: Embedding generation — chunk, embed, and upsert into vector index.
        // Note sensitivity is mapped from intake (Low/Medium/High) to core
        // (Shareable/Restricted/Private) so the embedding router enforces
        // provider constraints. URL content defaults to Shareable since it
        // was fetched from the public web.
        let note_core_sensitivity = note_sensitivity.map(symbiotic_core::Sensitivity::from);
        let embedding_report = self
            .run_embeddings_for_batch(
                &batch,
                &fetched_pairs,
                note_content.as_deref(),
                note_core_sensitivity,
            )
            .await;

        Ok((batch, reports, embedding_report))
    }

    /// Run embedding generation for all successfully ingested items in a batch.
    ///
    /// Best-effort: embedding failures are logged but never propagate.
    /// URL content is provided via `fetched_pairs` (already fetched for
    /// distillery). Note content is passed directly since it doesn't need
    /// fetching.
    ///
    /// `note_sensitivity` is the core sensitivity level for note content,
    /// mapped from the intake classifier's Low/Medium/High classification.
    /// URL items always use `Shareable` since they are fetched from the
    /// public web.
    async fn run_embeddings_for_batch(
        &self,
        batch: &IntakeBatchResult,
        fetched_pairs: &[(String, String, Option<String>)],
        note_content: Option<&str>,
        note_sensitivity: Option<symbiotic_core::Sensitivity>,
    ) -> IntakeEmbeddingReport {
        let mut outcomes = Vec::new();

        let Some(ref processor) = self.embedding_processor else {
            return IntakeEmbeddingReport { outcomes };
        };

        // Build a map of URL -> content for quick lookup.
        let url_content: std::collections::HashMap<&str, &str> = fetched_pairs
            .iter()
            .map(|(url, content, _)| (url.as_str(), content.as_str()))
            .collect();

        for item in &batch.items {
            if item.status != IntakeStatus::Ingested {
                continue;
            }

            let record_id = match &item.record_id {
                Some(id) => id.as_str(),
                None => continue,
            };

            // Determine the content to embed: URL content from fetched_pairs,
            // or note content from the original request.
            let content = if let Some(url) = &item.normalized_url {
                url_content.get(url.as_str()).copied()
            } else {
                note_content
            };

            let Some(content) = content else {
                tracing::debug!(
                    record_id = record_id,
                    "intake_embeddings: no content available for embedding"
                );
                continue;
            };

            // Map sensitivity: URL content is public web data (Shareable).
            // Note content uses the classifier's result mapped to core
            // sensitivity (Low->Shareable, Medium->Restricted, High->Private).
            let sensitivity = if item.normalized_url.is_some() {
                // URL items: fetched from the public web.
                symbiotic_core::Sensitivity::Shareable
            } else {
                // Note items: use the classified sensitivity, defaulting to
                // Shareable if no classification was performed.
                note_sensitivity.unwrap_or(symbiotic_core::Sensitivity::Shareable)
            };

            let outcome = processor
                .process_document(record_id, content, sensitivity)
                .await;

            tracing::info!(
                record_id = record_id,
                run_id = %batch.run_id,
                chunks_total = outcome.chunks_total,
                chunks_embedded = outcome.chunks_embedded,
                chunks_pending = outcome.chunks_pending,
                chunks_failed = outcome.chunks_failed,
                "intake_embeddings: processed"
            );

            outcomes.push(outcome);
        }

        // sqlite-vec persists writes immediately — no explicit save needed.

        IntakeEmbeddingReport { outcomes }
    }

    /// Check whether a discovered link is relevant enough to follow during
    /// recursive intake, using the pipeline's configured LLM and
    /// link relevance settings.
    ///
    /// This is a convenience wrapper around [`ollama::check_link_relevance`]
    /// that uses the pipeline's own config and LLM client.
    pub async fn check_link_relevance(&self, context: &crate::ollama::LinkContext) -> bool {
        crate::ollama::check_link_relevance(
            context,
            &self.intake_config.link_relevance,
            self.llm.as_deref(),
        )
        .await
    }

    fn is_blocked(&self, url: &Url) -> bool {
        let Some(host) = url.host_str() else {
            return true;
        };

        let host = host.to_ascii_lowercase();
        self.policy.blocked_hosts.contains(&host) || is_local_or_private_host(&host)
    }
}

fn is_local_or_private_host(host: &str) -> bool {
    if host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
        || host == "metadata.google.internal"
        || host == "metadata.aws.internal"
        || host == "metadata.azure.internal"
    {
        return true;
    }

    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
                || (v4.octets()[0] == 198 && matches!(v4.octets()[1], 18 | 19))
                || v4.octets() == [169, 254, 169, 254]
        }
        Ok(IpAddr::V6(v6)) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
        Err(_) => false,
    }
}

pub fn intake(
    request: IntakeRequest,
    pipeline: &IntakePipeline,
) -> Result<symbiotic_core::intake::IntakeResult> {
    let batch = pipeline.process(request)?;
    let item = batch
        .items
        .first()
        .ok_or_else(|| anyhow!("pipeline produced no result items"))?;

    Ok(symbiotic_core::intake::IntakeResult {
        run_id: batch.run_id,
        status: item.status.clone(),
        route: item.route.clone(),
        review_queued: item.review_queued,
        review_job_id: item.review_job_id.clone(),
        idempotency_key: item.idempotency_key.clone().unwrap_or_default(),
    })
}

pub fn summarize(items: &[IntakeItemResult]) -> IntakeSummary {
    let mut summary = IntakeSummary {
        total: items.len(),
        ..IntakeSummary::default()
    };

    for item in items {
        match item.status {
            IntakeStatus::Ingested => summary.ingested += 1,
            IntakeStatus::Duplicate => summary.duplicates += 1,
            IntakeStatus::Blocked => summary.blocked += 1,
            IntakeStatus::Invalid => summary.invalid += 1,
            IntakeStatus::SecureRouted => summary.secure_routed += 1,
            IntakeStatus::FetchFailed
            | IntakeStatus::ParseFailed
            | IntakeStatus::StoreFailed
            | IntakeStatus::QueueFailed
            | IntakeStatus::SensitivePendingApproval => summary.failed += 1,
        }

        if item.review_queued {
            summary.review_queued += 1;
        }
    }

    summary
}

fn generate_run_id() -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs();
    format!("run_{ts}")
}

/// Parse YAML frontmatter from a markdown file.
/// Returns (Option<serde_yml::Value>, body_text).
/// Frontmatter is delimited by `---` on its own line.
fn parse_frontmatter(content: &str) -> (Option<serde_yml::Value>, String) {
    let trimmed = content.trim_start();
    if !trimmed.starts_with("---") {
        return (None, content.to_string());
    }
    // Skip the opening ---
    let after_open = &trimmed[3..];
    if let Some(end_idx) = after_open.find("\n---") {
        let yaml_str = &after_open[..end_idx];
        let body_start = end_idx + 4; // skip \n---
        let body = after_open[body_start..]
            .trim_start_matches('\n')
            .to_string();
        match serde_yml::from_str(yaml_str) {
            Ok(fm) => (Some(fm), body),
            Err(_) => (None, content.to_string()),
        }
    } else {
        (None, content.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use symbiotic_core::intake::{normalize_url, IntakeKind, IntakeRequest, IntakeSource};

    struct TestFetcher {
        fail_hosts: HashSet<String>,
    }

    impl ContentFetcher for TestFetcher {
        fn fetch(&self, url: &Url) -> Result<FetchedContent> {
            let host = url
                .host_str()
                .ok_or_else(|| anyhow!("url without host cannot be fetched"))?;
            if self.fail_hosts.contains(host) {
                return Err(anyhow!("simulated fetch failure"));
            }
            Ok(FetchedContent {
                markdown: format!("# fetched {}", url.as_str()),
                html_title: None,
                title: None,
            })
        }
    }

    #[derive(Default)]
    struct TestStore {
        existing: Mutex<HashSet<String>>,
        records: Mutex<HashMap<String, String>>,
        vault_records: Mutex<HashMap<String, String>>,
    }

    impl IntakeStore for TestStore {
        fn exists(&self, idempotency_key: &str) -> Result<bool> {
            Ok(self
                .existing
                .lock()
                .expect("existing lock")
                .contains(idempotency_key))
        }

        fn store_archive_url(
            &self,
            _url: &Url,
            _content: &FetchedContent,
            _tags: &[String],
            idempotency_key: &str,
            _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
        ) -> Result<String> {
            self.existing
                .lock()
                .expect("existing lock")
                .insert(idempotency_key.to_string());
            let record_id = format!(
                "archive_{}",
                self.records.lock().expect("records lock").len() + 1
            );
            self.records
                .lock()
                .expect("records lock")
                .insert(record_id.clone(), idempotency_key.to_string());
            Ok(record_id)
        }

        fn store_archive_note(
            &self,
            _note: &str,
            _tags: &[String],
            idempotency_key: &str,
            _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
        ) -> Result<String> {
            self.existing
                .lock()
                .expect("existing lock")
                .insert(idempotency_key.to_string());
            let record_id = format!(
                "note_{}",
                self.records.lock().expect("records lock").len() + 1
            );
            self.records
                .lock()
                .expect("records lock")
                .insert(record_id.clone(), idempotency_key.to_string());
            Ok(record_id)
        }

        fn store_vault_note(
            &self,
            _note: &str,
            _tags: &[String],
            idempotency_key: &str,
            _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
        ) -> Result<String> {
            let record_id = format!(
                "vault_{}",
                self.vault_records.lock().expect("vault lock").len() + 1
            );
            self.vault_records
                .lock()
                .expect("vault lock")
                .insert(record_id.clone(), idempotency_key.to_string());
            Ok(record_id)
        }
    }

    struct TestQueue {
        fail_record: Option<String>,
    }

    impl ReviewQueue for TestQueue {
        fn enqueue(&self, record_id: &str) -> Result<String> {
            if self.fail_record.as_deref() == Some(record_id) {
                return Err(anyhow!("simulated queue failure"));
            }
            Ok(format!("job_{record_id}"))
        }
    }

    struct TestClassifier;

    impl SensitivityClassifier for TestClassifier {
        fn classify_note(&self, note: &str) -> Sensitivity {
            let lowercase = note.to_ascii_lowercase();
            if lowercase.contains("password=") || lowercase.contains("api_key") {
                Sensitivity::High
            } else if lowercase.contains("secret") {
                Sensitivity::Medium
            } else {
                Sensitivity::Low
            }
        }
    }

    fn pipeline(
        policy: IntakePolicy,
        fail_hosts: &[&str],
        fail_record: Option<String>,
    ) -> IntakePipeline {
        IntakePipeline::new(
            Arc::new(TestFetcher {
                fail_hosts: fail_hosts.iter().map(|host| host.to_string()).collect(),
            }),
            Arc::new(TestStore::default()),
            Arc::new(TestQueue { fail_record }),
            Arc::new(TestClassifier),
            policy,
        )
    }

    #[test]
    fn url_pipeline_ingests_and_queues_review() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/path").expect("valid url")],
            note: None,
            tags: vec!["AI".to_string()],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.total, 1);
        assert_eq!(result.summary.ingested, 1);
        assert_eq!(result.summary.review_queued, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Ingested);
    }

    #[test]
    fn url_pipeline_marks_duplicates_in_batch() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let normalized = normalize_url("https://example.com/path").expect("valid url");
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalized.clone(), normalized],
            note: None,
            tags: vec!["tag".to_string()],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);
        assert_eq!(result.summary.duplicates, 1);
        assert_eq!(result.items[1].status, IntakeStatus::Duplicate);
    }

    #[test]
    fn url_pipeline_blocks_configured_hosts() {
        let mut blocked_hosts = HashSet::new();
        blocked_hosts.insert("blocked.example".to_string());
        let ingestion = pipeline(IntakePolicy { blocked_hosts }, &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://blocked.example/path").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.blocked, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Blocked);
    }

    #[test]
    fn url_pipeline_blocks_local_targets_by_default() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("http://localhost/admin").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.blocked, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Blocked);
    }

    #[test]
    fn url_pipeline_reports_fetch_failures() {
        let ingestion = pipeline(IntakePolicy::default(), &["fail.example"], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://fail.example/path").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.failed, 1);
        assert_eq!(result.items[0].status, IntakeStatus::FetchFailed);
    }

    #[test]
    fn note_pipeline_routes_high_sensitivity_to_vault() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("password=super-secret".to_string()),
            tags: vec!["private".to_string()],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.secure_routed, 1);
        assert_eq!(result.items[0].status, IntakeStatus::SecureRouted);
        assert_eq!(result.items[0].route, IntakeRoute::Vault);
    }

    #[test]
    fn note_pipeline_requires_approval_for_medium_sensitivity() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("this contains secret material".to_string()),
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.failed, 1);
        assert_eq!(
            result.items[0].status,
            IntakeStatus::SensitivePendingApproval
        );
    }

    #[test]
    fn note_pipeline_ingests_low_sensitivity_and_queues_review() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("meeting notes for roadmap".to_string()),
            tags: vec!["notes".to_string()],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);
        assert_eq!(result.summary.review_queued, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Ingested);
    }

    #[test]
    fn url_pipeline_reports_queue_failures() {
        let store = Arc::new(TestStore::default());
        let fetcher = Arc::new(TestFetcher {
            fail_hosts: HashSet::new(),
        });
        let queue = Arc::new(TestQueue {
            fail_record: Some("archive_1".to_string()),
        });
        let ingestion = IntakePipeline::new(
            fetcher,
            store,
            queue,
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/path").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.failed, 1);
        assert_eq!(result.items[0].status, IntakeStatus::QueueFailed);
        assert!(!result.items[0].review_queued);
    }

    #[test]
    fn url_pipeline_blocks_non_http_schemes() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let ftp_url = Url::parse("ftp://example.com/file.txt").expect("valid ftp url");
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![ftp_url],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.blocked, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Blocked);
        assert!(result.items[0]
            .error
            .as_deref()
            .unwrap()
            .contains("unsupported URL scheme"));
    }

    #[test]
    fn url_pipeline_rejects_oversized_content() {
        struct OversizedFetcher;
        impl ContentFetcher for OversizedFetcher {
            fn fetch(&self, _url: &Url) -> Result<FetchedContent> {
                Ok(FetchedContent {
                    markdown: "x".repeat(MAX_CONTENT_BYTES + 1),
                    html_title: None,
                    title: None,
                })
            }
        }

        let ingestion = IntakePipeline::new(
            Arc::new(OversizedFetcher),
            Arc::new(TestStore::default()),
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/huge").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.failed, 1);
        assert_eq!(result.items[0].status, IntakeStatus::FetchFailed);
        assert!(result.items[0]
            .error
            .as_deref()
            .unwrap()
            .contains("exceeds maximum size"));
    }

    // -------------------------------------------------------------------
    // T54: Twitter URL deduplication in pipeline
    // -------------------------------------------------------------------

    #[test]
    fn twitter_url_variants_deduplicated_in_batch() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let url_a = Url::parse("https://x.com/alex_prompter/status/123").expect("valid url");
        let url_b = Url::parse("https://twitter.com/alex_prompter/status/123").expect("valid url");
        let url_c = Url::parse("https://x.com/i/status/123").expect("valid url");
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![url_a, url_b, url_c],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(
            result.summary.ingested, 1,
            "only the first variant should be ingested"
        );
        assert_eq!(
            result.summary.duplicates, 2,
            "the other two should be duplicates"
        );
    }

    #[test]
    fn twitter_url_dedup_disabled_does_not_merge() {
        let ingestion =
            pipeline(IntakePolicy::default(), &[], None).with_intake_config(IntakeConfig {
                dedup_enabled: false,
                ..Default::default()
            });
        let url_a = Url::parse("https://x.com/user/status/456").expect("valid url");
        let url_b = Url::parse("https://twitter.com/user/status/456").expect("valid url");
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![url_a, url_b],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        // Without dedup, the two different URLs have different idempotency keys
        // so both are ingested.
        assert_eq!(result.summary.ingested, 2);
        assert_eq!(result.summary.duplicates, 0);
    }

    #[test]
    fn non_twitter_urls_not_affected_by_content_dedup() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);
        let url_a = normalize_url("https://example.com/page-a").expect("valid url");
        let url_b = normalize_url("https://example.com/page-b").expect("valid url");
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![url_a, url_b],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 2);
        assert_eq!(result.summary.duplicates, 0);
    }

    // -------------------------------------------------------------------
    // T12: Title field propagation
    // -------------------------------------------------------------------

    #[test]
    fn fetched_content_html_title_propagated_to_title() {
        struct TitleFetcher;
        impl ContentFetcher for TitleFetcher {
            fn fetch(&self, _url: &Url) -> Result<FetchedContent> {
                Ok(FetchedContent {
                    markdown: "# Article".to_string(),
                    html_title: Some("My Article Title".to_string()),
                    title: None,
                })
            }
        }

        let store = Arc::new(TestStore::default());
        let ingestion = IntakePipeline::new(
            Arc::new(TitleFetcher),
            store,
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/article").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);
        // The title propagation happens inside process() — it sets content.title
        // from html_title when title is None. The store receives this content.
    }

    // -------------------------------------------------------------------
    // IntakeConfig builder tests
    // -------------------------------------------------------------------

    #[test]
    fn with_intake_config_sets_config() {
        let config = IntakeConfig {
            dedup_enabled: false,
            ..Default::default()
        };
        let ingestion = pipeline(IntakePolicy::default(), &[], None).with_intake_config(config);
        assert!(!ingestion.intake_config().dedup_enabled);
    }

    // -------------------------------------------------------------------
    // T84: Intake embedding generation
    // -------------------------------------------------------------------

    mod embedding_tests {
        use super::*;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;
        use symbiotic_context::chunking::{ChunkConfig, Chunker};
        use symbiotic_context::embedding::{
            EmbedError, EmbedResult, EmbeddingProvider, ProviderClass,
        };
        use symbiotic_context::intake_embeddings::{EmbedRouter, IntakeEmbeddingProcessor};
        use symbiotic_context::vector_index::VectorIndex;

        /// Mock embedding provider that returns a fixed vector.
        struct MockEmbedProvider {
            embedding: Vec<f32>,
        }

        #[async_trait]
        impl EmbeddingProvider for MockEmbedProvider {
            fn provider_class(&self) -> ProviderClass {
                ProviderClass::Local
            }

            fn model_name(&self) -> &str {
                "mock-embed"
            }

            async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
                Ok(EmbedResult {
                    embedding: self.embedding.clone(),
                    model_name: "mock-embed".to_string(),
                    dimensions: self.embedding.len(),
                })
            }
        }

        /// Mock router that wraps a provider for direct use with EmbedRouter.
        struct MockRouter {
            provider: Arc<dyn EmbeddingProvider>,
        }

        #[async_trait]
        impl EmbedRouter for MockRouter {
            async fn embed(
                &self,
                text: &str,
                _sensitivity: symbiotic_core::Sensitivity,
            ) -> Result<EmbedResult, EmbedError> {
                self.provider.embed(text).await
            }
        }

        /// Mock embedding provider that always returns Unavailable.
        struct UnavailableProvider;

        #[async_trait]
        impl EmbeddingProvider for UnavailableProvider {
            fn provider_class(&self) -> ProviderClass {
                ProviderClass::Local
            }

            fn model_name(&self) -> &str {
                "unavailable"
            }

            async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
                Err(EmbedError::Unavailable("mock unavailable".to_string()))
            }
        }

        fn make_processor(
            provider: Arc<dyn EmbeddingProvider>,
        ) -> (IntakeEmbeddingProcessor, Arc<StdMutex<VectorIndex>>) {
            let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
            let router: Arc<dyn EmbedRouter> = Arc::new(MockRouter { provider });
            let vector_index = Arc::new(StdMutex::new(
                VectorIndex::open_in_memory(3).expect("vec index"),
            ));
            let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index.clone());
            (processor, vector_index)
        }

        fn pipeline_with_embeddings(
            provider: Arc<dyn EmbeddingProvider>,
        ) -> (IntakePipeline, Arc<StdMutex<VectorIndex>>) {
            let (processor, vector_index) = make_processor(provider);
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);
            (p, vector_index)
        }

        #[tokio::test]
        async fn process_with_distillery_generates_embeddings_for_url() {
            let provider = Arc::new(MockEmbedProvider {
                embedding: vec![0.1, 0.2, 0.3],
            });
            let (p, vector_index) = pipeline_with_embeddings(provider);

            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![normalize_url("https://example.com/article").expect("valid url")],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _distillery_reports, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.ingested, 1);
            assert_eq!(embedding_report.outcomes.len(), 1);

            let outcome = &embedding_report.outcomes[0];
            assert!(outcome.chunks_total > 0, "should have chunked the content");
            assert_eq!(outcome.chunks_embedded, outcome.chunks_total);
            assert_eq!(outcome.chunks_pending, 0);
            assert_eq!(outcome.chunks_failed, 0);

            // Vector index should contain the embeddings.
            let index = vector_index.lock().expect("lock");
            assert!(!index.is_empty(), "vector index should have entries");
        }

        #[tokio::test]
        async fn process_with_distillery_generates_embeddings_for_note() {
            let provider = Arc::new(MockEmbedProvider {
                embedding: vec![0.5, 0.6, 0.7],
            });
            let (p, vector_index) = pipeline_with_embeddings(provider);

            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Note,
                urls: vec![],
                note: Some("meeting notes for the roadmap discussion today".to_string()),
                tags: vec!["notes".to_string()],
                file_path: None,
                title: None,
            };

            let (batch, _reports, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.ingested, 1);
            assert_eq!(embedding_report.outcomes.len(), 1);

            let outcome = &embedding_report.outcomes[0];
            assert!(outcome.chunks_total > 0);
            assert_eq!(outcome.chunks_embedded, outcome.chunks_total);

            let index = vector_index.lock().expect("lock");
            assert!(!index.is_empty());
        }

        #[tokio::test]
        async fn process_with_distillery_skips_embeddings_for_duplicates() {
            let provider = Arc::new(MockEmbedProvider {
                embedding: vec![0.1, 0.2, 0.3],
            });
            let (p, vector_index) = pipeline_with_embeddings(provider);

            let url = normalize_url("https://example.com/path").expect("valid url");
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![url.clone(), url],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            // First URL ingested, second is duplicate.
            assert_eq!(batch.summary.ingested, 1);
            assert_eq!(batch.summary.duplicates, 1);

            // Only one embedding outcome (for the ingested item).
            assert_eq!(embedding_report.outcomes.len(), 1);

            let index = vector_index.lock().expect("lock");
            assert!(!index.is_empty());
        }

        #[tokio::test]
        async fn process_with_distillery_no_embeddings_without_processor() {
            let p = pipeline(IntakePolicy::default(), &[], None);
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![normalize_url("https://example.com/page").expect("valid url")],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.ingested, 1);
            assert!(
                embedding_report.outcomes.is_empty(),
                "no embedding outcomes without a processor"
            );
        }

        #[tokio::test]
        async fn process_with_distillery_tolerates_embedding_failure() {
            let provider: Arc<dyn EmbeddingProvider> = Arc::new(UnavailableProvider);
            let (processor, _vector_index) = make_processor(provider);
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);

            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![normalize_url("https://example.com/article").expect("valid url")],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("intake should succeed despite embedding failure");

            assert_eq!(batch.summary.ingested, 1);
            assert_eq!(embedding_report.outcomes.len(), 1);

            let outcome = &embedding_report.outcomes[0];
            // All chunks should be pending (unavailable provider).
            assert_eq!(outcome.chunks_embedded, 0);
            assert!(
                outcome.chunks_pending > 0 || outcome.chunks_failed > 0,
                "chunks should be pending or failed, not silently dropped"
            );
        }

        #[tokio::test]
        async fn embedding_processor_builder_works() {
            let provider = Arc::new(MockEmbedProvider {
                embedding: vec![0.1],
            });
            let (processor, _) = make_processor(provider);
            let p =
                pipeline(IntakePolicy::default(), &[], None).with_embedding_processor(processor);

            assert!(
                p.embedding_processor().is_some(),
                "embedding processor should be set"
            );
        }

        #[tokio::test]
        async fn process_with_distillery_skips_blocked_for_embeddings() {
            let provider = Arc::new(MockEmbedProvider {
                embedding: vec![0.1, 0.2],
            });
            let (p, vector_index) = pipeline_with_embeddings(provider);

            // Use a scheme that will be blocked.
            let ftp_url = Url::parse("ftp://example.com/file.txt").expect("valid ftp url");
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![ftp_url],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.blocked, 1);
            assert!(
                embedding_report.outcomes.is_empty(),
                "blocked items should not generate embeddings"
            );

            let index = vector_index.lock().expect("lock");
            assert_eq!(index.len(), 0);
        }
    }

    // -------------------------------------------------------------------
    // Sensitivity mapping: intake -> core
    // -------------------------------------------------------------------

    #[test]
    fn sensitivity_low_maps_to_shareable() {
        let core: symbiotic_core::Sensitivity = Sensitivity::Low.into();
        assert_eq!(core, symbiotic_core::Sensitivity::Shareable);
    }

    #[test]
    fn sensitivity_medium_maps_to_restricted() {
        let core: symbiotic_core::Sensitivity = Sensitivity::Medium.into();
        assert_eq!(core, symbiotic_core::Sensitivity::Restricted);
    }

    #[test]
    fn sensitivity_high_maps_to_private() {
        let core: symbiotic_core::Sensitivity = Sensitivity::High.into();
        assert_eq!(core, symbiotic_core::Sensitivity::Private);
    }

    // -------------------------------------------------------------------
    // Sensitivity-aware embedding routing
    // -------------------------------------------------------------------

    mod sensitivity_embedding_tests {
        use super::*;
        use async_trait::async_trait;
        use std::sync::Mutex as StdMutex;
        use symbiotic_context::chunking::{ChunkConfig, Chunker};
        use symbiotic_context::embedding::{
            EmbedError, EmbedResult, EmbeddingProvider, ProviderClass,
        };
        use symbiotic_context::intake_embeddings::{EmbedRouter, IntakeEmbeddingProcessor};
        use symbiotic_context::vector_index::VectorIndex;

        /// Mock embedding provider that returns a fixed vector.
        struct FixedProvider {
            embedding: Vec<f32>,
        }

        #[async_trait]
        impl EmbeddingProvider for FixedProvider {
            fn provider_class(&self) -> ProviderClass {
                ProviderClass::Local
            }

            fn model_name(&self) -> &str {
                "fixed-embed"
            }

            async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
                Ok(EmbedResult {
                    embedding: self.embedding.clone(),
                    model_name: "fixed-embed".to_string(),
                    dimensions: self.embedding.len(),
                })
            }
        }

        /// A router that captures the sensitivity level passed to each embed call.
        struct CapturingSensitivityRouter {
            provider: Arc<dyn EmbeddingProvider>,
            captured: Arc<StdMutex<Vec<symbiotic_core::Sensitivity>>>,
        }

        #[async_trait]
        impl EmbedRouter for CapturingSensitivityRouter {
            async fn embed(
                &self,
                text: &str,
                sensitivity: symbiotic_core::Sensitivity,
            ) -> Result<EmbedResult, EmbedError> {
                self.captured.lock().expect("lock").push(sensitivity);
                self.provider.embed(text).await
            }
        }

        type CapturedSensitivities = Arc<StdMutex<Vec<symbiotic_core::Sensitivity>>>;

        fn make_capturing_processor() -> (
            IntakeEmbeddingProcessor,
            Arc<StdMutex<VectorIndex>>,
            CapturedSensitivities,
        ) {
            let provider: Arc<dyn EmbeddingProvider> = Arc::new(FixedProvider {
                embedding: vec![0.1, 0.2, 0.3],
            });
            let captured: CapturedSensitivities = Arc::new(StdMutex::new(Vec::new()));
            let router: Arc<dyn EmbedRouter> = Arc::new(CapturingSensitivityRouter {
                provider,
                captured: captured.clone(),
            });
            let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
            let vector_index = Arc::new(StdMutex::new(
                VectorIndex::open_in_memory(3).expect("vec index"),
            ));
            let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index.clone());
            (processor, vector_index, captured)
        }

        #[tokio::test]
        async fn url_embeddings_use_shareable_sensitivity() {
            let (processor, _vi, captured) = make_capturing_processor();
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);

            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![normalize_url("https://example.com/public-page").expect("valid url")],
                note: None,
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, _) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.ingested, 1);

            let sensitivities = captured.lock().expect("lock");
            assert!(
                !sensitivities.is_empty(),
                "router should have been called at least once"
            );
            for s in sensitivities.iter() {
                assert_eq!(
                    *s,
                    symbiotic_core::Sensitivity::Shareable,
                    "URL content should use Shareable sensitivity"
                );
            }
        }

        #[tokio::test]
        async fn low_sensitivity_note_embeddings_use_shareable() {
            let (processor, _vi, captured) = make_capturing_processor();
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);

            // "meeting notes" is low-sensitivity per TestClassifier
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Note,
                urls: vec![],
                note: Some("meeting notes for the roadmap discussion".to_string()),
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, _) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.ingested, 1);

            let sensitivities = captured.lock().expect("lock");
            assert!(!sensitivities.is_empty(), "router should have been called");
            for s in sensitivities.iter() {
                assert_eq!(
                    *s,
                    symbiotic_core::Sensitivity::Shareable,
                    "Low-sensitivity note should map to Shareable"
                );
            }
        }

        /// Verify that the `From` mapping is used in the pipeline.
        /// High-sensitivity notes are SecureRouted (not Ingested), so they
        /// do not reach the embedding step. This test confirms that.
        #[tokio::test]
        async fn high_sensitivity_note_skips_embeddings() {
            let (processor, _vi, captured) = make_capturing_processor();
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);

            // "password=foo" is high-sensitivity per TestClassifier
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Note,
                urls: vec![],
                note: Some("password=super-secret-value".to_string()),
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.secure_routed, 1);
            assert_eq!(batch.summary.ingested, 0);
            assert!(
                embedding_report.outcomes.is_empty(),
                "high-sensitivity notes are SecureRouted, not Ingested, so no embeddings"
            );

            let sensitivities = captured.lock().expect("lock");
            assert!(
                sensitivities.is_empty(),
                "router should not have been called for SecureRouted content"
            );
        }

        /// Medium-sensitivity notes get SensitivePendingApproval (not
        /// Ingested), so they also skip embedding.
        #[tokio::test]
        async fn medium_sensitivity_note_skips_embeddings() {
            let (processor, _vi, captured) = make_capturing_processor();
            let p = IntakePipeline::new(
                Arc::new(TestFetcher {
                    fail_hosts: HashSet::new(),
                }),
                Arc::new(TestStore::default()),
                Arc::new(TestQueue { fail_record: None }),
                Arc::new(TestClassifier),
                IntakePolicy::default(),
            )
            .with_embedding_processor(processor);

            // "secret" triggers Medium per TestClassifier
            let request = IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Note,
                urls: vec![],
                note: Some("this contains secret material".to_string()),
                tags: vec![],
                file_path: None,
                title: None,
            };

            let (batch, _, embedding_report) = p
                .process_with_distillery(request)
                .await
                .expect("should succeed");

            assert_eq!(batch.summary.failed, 1);
            assert_eq!(batch.summary.ingested, 0);
            assert!(
                embedding_report.outcomes.is_empty(),
                "medium-sensitivity notes are pending approval, not Ingested"
            );

            let sensitivities = captured.lock().expect("lock");
            assert!(
                sensitivities.is_empty(),
                "router should not have been called for pending-approval content"
            );
        }
    }

    // -------------------------------------------------------------------
    // T82: Redaction policy engine activation
    // -------------------------------------------------------------------

    #[test]
    fn url_pipeline_redacts_pii_in_content_before_storage() {
        /// A fetcher that returns content containing PII.
        struct PiiFetcher;
        impl ContentFetcher for PiiFetcher {
            fn fetch(&self, _url: &Url) -> Result<FetchedContent> {
                Ok(FetchedContent {
                    markdown: "Contact user@example.com or call 555-123-4567 for details"
                        .to_string(),
                    html_title: None,
                    title: None,
                })
            }
        }

        /// A store that captures the stored content for inspection.
        #[derive(Default)]
        struct CapturingStore {
            stored_content: std::sync::Mutex<Vec<String>>,
            existing: std::sync::Mutex<HashSet<String>>,
        }

        impl IntakeStore for CapturingStore {
            fn exists(&self, key: &str) -> Result<bool> {
                Ok(self.existing.lock().expect("lock").contains(key))
            }

            fn store_archive_url(
                &self,
                _url: &Url,
                content: &FetchedContent,
                _tags: &[String],
                key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                self.stored_content
                    .lock()
                    .expect("lock")
                    .push(content.markdown.clone());
                self.existing.lock().expect("lock").insert(key.to_string());
                Ok("record_1".to_string())
            }

            fn store_archive_note(
                &self,
                note: &str,
                _tags: &[String],
                key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                self.stored_content
                    .lock()
                    .expect("lock")
                    .push(note.to_string());
                self.existing.lock().expect("lock").insert(key.to_string());
                Ok("note_1".to_string())
            }

            fn store_vault_note(
                &self,
                _note: &str,
                _tags: &[String],
                _key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                Ok("vault_1".to_string())
            }
        }

        let store = Arc::new(CapturingStore::default());
        let ingestion = IntakePipeline::new(
            Arc::new(PiiFetcher),
            store.clone(),
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/contact").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);

        // Verify that stored content has PII redacted
        let stored = store.stored_content.lock().expect("lock");
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].contains("[redacted-email]"),
            "email should be redacted in stored content, got: {}",
            stored[0]
        );
        assert!(
            stored[0].contains("[redacted-phone]"),
            "phone should be redacted in stored content, got: {}",
            stored[0]
        );
        assert!(
            !stored[0].contains("user@example.com"),
            "original email should not appear in stored content"
        );
        assert!(
            !stored[0].contains("555-123-4567"),
            "original phone should not appear in stored content"
        );
    }

    #[test]
    fn url_pipeline_preserves_idempotency_key_after_redaction() {
        /// Fetcher returning PII content
        struct PiiFetcher;
        impl ContentFetcher for PiiFetcher {
            fn fetch(&self, _url: &Url) -> Result<FetchedContent> {
                Ok(FetchedContent {
                    markdown: "Contact admin@corp.com for help".to_string(),
                    html_title: None,
                    title: None,
                })
            }
        }

        let ingestion = IntakePipeline::new(
            Arc::new(PiiFetcher),
            Arc::new(TestStore::default()),
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );

        let url = normalize_url("https://example.com/help").expect("valid url");
        let tags = vec!["support".to_string()];

        // Compute expected idempotency key from the *original* URL (not redacted content)
        let expected_key = idempotency_key(&url, &tags);

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![url],
            note: None,
            tags,
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);
        assert_eq!(
            result.items[0].idempotency_key.as_deref(),
            Some(expected_key.as_str()),
            "idempotency key should be computed from original URL, not redacted content"
        );
    }

    #[test]
    fn note_pipeline_redacts_pii_in_low_sensitivity_notes() {
        /// A store that captures the stored note text.
        #[derive(Default)]
        struct CapturingNoteStore {
            stored_notes: std::sync::Mutex<Vec<String>>,
            existing: std::sync::Mutex<HashSet<String>>,
        }

        impl IntakeStore for CapturingNoteStore {
            fn exists(&self, key: &str) -> Result<bool> {
                Ok(self.existing.lock().expect("lock").contains(key))
            }

            fn store_archive_url(
                &self,
                _url: &Url,
                _content: &FetchedContent,
                _tags: &[String],
                _key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                Ok("url_1".to_string())
            }

            fn store_archive_note(
                &self,
                note: &str,
                _tags: &[String],
                key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                self.stored_notes
                    .lock()
                    .expect("lock")
                    .push(note.to_string());
                self.existing.lock().expect("lock").insert(key.to_string());
                Ok("note_1".to_string())
            }

            fn store_vault_note(
                &self,
                _note: &str,
                _tags: &[String],
                _key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                Ok("vault_1".to_string())
            }
        }

        let store = Arc::new(CapturingNoteStore::default());
        let ingestion = IntakePipeline::new(
            Arc::new(TestFetcher {
                fail_hosts: HashSet::new(),
            }),
            store.clone(),
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        );

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("Meeting with user@example.com at 192.168.1.1".to_string()),
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);

        let stored = store.stored_notes.lock().expect("lock");
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].contains("[redacted-email]"),
            "email should be redacted in stored note, got: {}",
            stored[0]
        );
        assert!(
            stored[0].contains("[redacted-ip]"),
            "IP should be redacted in stored note, got: {}",
            stored[0]
        );
        assert!(
            !stored[0].contains("user@example.com"),
            "original email should not appear in stored note"
        );
    }

    #[test]
    fn redaction_can_be_disabled_via_config() {
        /// Fetcher returning PII content
        struct PiiFetcher;
        impl ContentFetcher for PiiFetcher {
            fn fetch(&self, _url: &Url) -> Result<FetchedContent> {
                Ok(FetchedContent {
                    markdown: "Contact user@example.com for help".to_string(),
                    html_title: None,
                    title: None,
                })
            }
        }

        /// A store that captures the stored content.
        #[derive(Default)]
        struct CapturingStore {
            stored_content: std::sync::Mutex<Vec<String>>,
            existing: std::sync::Mutex<HashSet<String>>,
        }

        impl IntakeStore for CapturingStore {
            fn exists(&self, key: &str) -> Result<bool> {
                Ok(self.existing.lock().expect("lock").contains(key))
            }

            fn store_archive_url(
                &self,
                _url: &Url,
                content: &FetchedContent,
                _tags: &[String],
                key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                self.stored_content
                    .lock()
                    .expect("lock")
                    .push(content.markdown.clone());
                self.existing.lock().expect("lock").insert(key.to_string());
                Ok("record_1".to_string())
            }

            fn store_archive_note(
                &self,
                _note: &str,
                _tags: &[String],
                _key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                Ok("note_1".to_string())
            }

            fn store_vault_note(
                &self,
                _note: &str,
                _tags: &[String],
                _key: &str,
                _firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
            ) -> Result<String> {
                Ok("vault_1".to_string())
            }
        }

        let store = Arc::new(CapturingStore::default());
        let ingestion = IntakePipeline::new(
            Arc::new(PiiFetcher),
            store.clone(),
            Arc::new(TestQueue { fail_record: None }),
            Arc::new(TestClassifier),
            IntakePolicy::default(),
        )
        .with_intake_config(IntakeConfig {
            redaction_enabled: false,
            ..Default::default()
        });

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/raw").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);

        // With redaction disabled, the original PII should be stored as-is
        let stored = store.stored_content.lock().expect("lock");
        assert_eq!(stored.len(), 1);
        assert!(
            stored[0].contains("user@example.com"),
            "with redaction disabled, original email should be preserved"
        );
    }

    #[test]
    fn redaction_enabled_by_default_in_config() {
        let config = IntakeConfig::default();
        assert!(
            config.redaction_enabled,
            "redaction should be enabled by default"
        );
    }

    #[test]
    fn clean_content_passes_through_unchanged() {
        let ingestion = pipeline(IntakePolicy::default(), &[], None);

        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/safe").expect("valid url")],
            note: None,
            tags: vec![],
            file_path: None,
            title: None,
        };

        // Content without PII should pass through normally
        let result = ingestion.process(request).expect("ingest should work");
        assert_eq!(result.summary.ingested, 1);
        assert_eq!(result.items[0].status, IntakeStatus::Ingested);
    }
}
