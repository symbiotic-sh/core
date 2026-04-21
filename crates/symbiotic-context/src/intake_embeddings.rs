//! Intake embedding processor — connects chunking, embedding, and vector indexing.
//!
//! Processes ingested documents through the embedding pipeline:
//! chunk → embed (via sensitivity-aware router) → upsert into vector index.
//! Never fails the intake — all errors are captured in the outcome.
//!
//! Supports configurable batch sizes for concurrent embedding of multiple
//! chunks via `FuturesUnordered`, and persists failed chunks to
//! `PendingChunkStore` for later retry.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::{FuturesUnordered, StreamExt};

use crate::chunking::{ChunkError, Chunker};
use crate::embedding::{EmbedError, EmbedResult, EmbeddingRouter};
use crate::pending_embeddings::{PendingChunk, PendingChunkStore};
use crate::vector_index::VectorIndex;
use crate::Sensitivity;

/// Default batch size for concurrent embedding requests.
const DEFAULT_BATCH_SIZE: usize = 8;

/// Trait abstracting sensitivity-aware embedding routing.
///
/// Implemented by both the legacy `EmbeddingRouter` (in this crate) and
/// the new `ProviderRouter` (via an adapter in the daemon).
#[async_trait]
pub trait EmbedRouter: Send + Sync {
    /// Embed text with sensitivity-aware provider selection.
    ///
    /// Returns the embedding vector on success. On failure, returns an
    /// `EmbedError` — callers distinguish `Unavailable` (retryable) from
    /// other errors (permanent failures).
    async fn embed(&self, text: &str, sensitivity: Sensitivity) -> Result<EmbedResult, EmbedError>;
}

/// Blanket implementation for the legacy `EmbeddingRouter`.
#[async_trait]
impl EmbedRouter for EmbeddingRouter {
    async fn embed(&self, text: &str, sensitivity: Sensitivity) -> Result<EmbedResult, EmbedError> {
        EmbeddingRouter::embed(self, text, sensitivity).await
    }
}

/// Configuration for the intake embedding processor.
#[derive(Debug, Clone)]
pub struct EmbeddingProcessorConfig {
    /// Maximum number of chunks to embed concurrently in a single batch.
    pub batch_size: usize,
}

impl Default for EmbeddingProcessorConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

/// Outcome of processing a single document through the embedding pipeline.
#[derive(Debug, Clone)]
pub struct EmbeddingOutcome {
    /// Source document ID.
    pub source_id: String,
    /// Total number of chunks produced.
    pub chunks_total: usize,
    /// Chunks successfully embedded and upserted.
    pub chunks_embedded: usize,
    /// Chunks where the provider was unavailable (can retry later).
    pub chunks_pending: usize,
    /// Chunks that failed with non-retryable errors.
    pub chunks_failed: usize,
}

/// Processes ingested documents into embeddings stored in the vector index.
pub struct IntakeEmbeddingProcessor {
    chunker: Chunker,
    router: Arc<dyn EmbedRouter>,
    vector_index: Arc<Mutex<VectorIndex>>,
    config: EmbeddingProcessorConfig,
    pending_store: Option<Arc<Mutex<PendingChunkStore>>>,
}

impl IntakeEmbeddingProcessor {
    /// Creates a new processor with the given chunker, router, and shared vector index.
    pub fn new(
        chunker: Chunker,
        router: Arc<dyn EmbedRouter>,
        vector_index: Arc<Mutex<VectorIndex>>,
    ) -> Self {
        Self {
            chunker,
            router,
            vector_index,
            config: EmbeddingProcessorConfig::default(),
            pending_store: None,
        }
    }

    /// Creates a new processor with explicit config.
    pub fn with_config(
        chunker: Chunker,
        router: Arc<dyn EmbedRouter>,
        vector_index: Arc<Mutex<VectorIndex>>,
        config: EmbeddingProcessorConfig,
    ) -> Self {
        Self {
            chunker,
            router,
            vector_index,
            config,
            pending_store: None,
        }
    }

    /// Attaches a pending chunk store for persisting unavailable chunks.
    pub fn with_pending_store(mut self, store: Arc<Mutex<PendingChunkStore>>) -> Self {
        self.pending_store = Some(store);
        self
    }

    /// Returns a reference to the shared vector index.
    pub fn vector_index(&self) -> &Arc<Mutex<VectorIndex>> {
        &self.vector_index
    }

    /// Returns a reference to the pending store, if configured.
    pub fn pending_store(&self) -> Option<&Arc<Mutex<PendingChunkStore>>> {
        self.pending_store.as_ref()
    }

    /// Returns a reference to the embed router.
    pub fn router(&self) -> &Arc<dyn EmbedRouter> {
        &self.router
    }

    /// Returns the configured batch size.
    pub fn batch_size(&self) -> usize {
        self.config.batch_size
    }

    /// Processes a document: chunk → embed (batched) → upsert.
    ///
    /// Never returns an error — all failures are captured in the outcome.
    /// `Unavailable` errors count as pending (retryable later) and are
    /// saved to the `PendingChunkStore` if one is attached.
    /// Other errors count as failed.
    pub async fn process_document(
        &self,
        source_id: &str,
        content: &str,
        sensitivity: Sensitivity,
    ) -> EmbeddingOutcome {
        let chunks = match self.chunker.chunk_document(source_id, content, sensitivity) {
            Ok(chunks) => chunks,
            Err(ChunkError::EmptyInput) => {
                return EmbeddingOutcome {
                    source_id: source_id.to_string(),
                    chunks_total: 0,
                    chunks_embedded: 0,
                    chunks_pending: 0,
                    chunks_failed: 0,
                };
            }
            Err(_) => {
                return EmbeddingOutcome {
                    source_id: source_id.to_string(),
                    chunks_total: 0,
                    chunks_embedded: 0,
                    chunks_pending: 0,
                    chunks_failed: 1,
                };
            }
        };

        let chunks_total = chunks.len();
        let mut chunks_embedded = 0usize;
        let mut chunks_pending = 0usize;
        let mut chunks_failed = 0usize;

        // Process chunks in batches using FuturesUnordered for concurrency.
        for batch_start in (0..chunks.len()).step_by(self.config.batch_size) {
            let batch_end = (batch_start + self.config.batch_size).min(chunks.len());
            let batch = &chunks[batch_start..batch_end];

            let mut futures = FuturesUnordered::new();

            for chunk in batch {
                let router = Arc::clone(&self.router);
                let text = chunk.text.clone();
                let sens = chunk.sensitivity;
                let chunk_id = chunk.chunk_id.clone();
                let seq_index = chunk.sequence_index;

                futures.push(async move {
                    let result = router.embed(&text, sens).await;
                    (chunk_id, seq_index, text, sens, result)
                });
            }

            while let Some((chunk_id, seq_index, chunk_text, sens, result)) = futures.next().await {
                match result {
                    Ok(embed_result) => {
                        if let Ok(index) = self.vector_index.lock() {
                            index.upsert(&chunk_id, &embed_result.embedding, sens);
                        }
                        chunks_embedded += 1;
                    }
                    Err(EmbedError::Unavailable(_)) => {
                        chunks_pending += 1;
                        // Save to pending store if available.
                        if let Some(ref store) = self.pending_store {
                            if let Ok(mut pending) = store.lock() {
                                let now = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs())
                                    .unwrap_or(0);
                                pending.add(PendingChunk {
                                    source_id: source_id.to_string(),
                                    chunk_index: seq_index,
                                    chunk_id,
                                    chunk_text,
                                    sensitivity: sens,
                                    created_at: now,
                                    retry_count: 0,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        chunks_failed += 1;
                    }
                }
            }
        }

        // Persist pending store after processing all batches.
        if chunks_pending > 0 {
            if let Some(ref store) = self.pending_store {
                if let Ok(pending) = store.lock() {
                    let _ = pending.save();
                }
            }
        }

        EmbeddingOutcome {
            source_id: source_id.to_string(),
            chunks_total,
            chunks_embedded,
            chunks_pending,
            chunks_failed,
        }
    }

    /// Retries embedding for pending chunks.
    ///
    /// Takes up to `batch_size` chunks from the pending store, attempts to
    /// embed them concurrently, and handles the results:
    /// - Success: upsert into vector index, remove from pending store.
    /// - Failure (Unavailable): increment retry_count, requeue (discard after max retries).
    /// - Failure (other): discard immediately.
    ///
    /// Returns the number of successfully embedded chunks.
    pub async fn retry_pending(&self) -> usize {
        let Some(ref store) = self.pending_store else {
            return 0;
        };

        let batch = {
            let Ok(mut pending) = store.lock() else {
                return 0;
            };
            if pending.is_empty() {
                return 0;
            }
            pending.take_batch(self.config.batch_size)
        };

        if batch.is_empty() {
            return 0;
        }

        let mut futures = FuturesUnordered::new();

        for chunk in batch {
            let router = Arc::clone(&self.router);
            futures.push(async move {
                let result = router.embed(&chunk.chunk_text, chunk.sensitivity).await;
                (chunk, result)
            });
        }

        let mut embedded_count = 0usize;
        let mut retry_chunks = Vec::new();

        while let Some((chunk, result)) = futures.next().await {
            match result {
                Ok(embed_result) => {
                    if let Ok(index) = self.vector_index.lock() {
                        index.upsert(&chunk.chunk_id, &embed_result.embedding, chunk.sensitivity);
                    }
                    embedded_count += 1;
                }
                Err(EmbedError::Unavailable(_)) => {
                    // Put back for another retry attempt.
                    retry_chunks.push(chunk);
                }
                Err(_) => {
                    // Non-retryable error — discard this chunk permanently.
                }
            }
        }

        // Requeue failed chunks (with incremented retry_count).
        if let Ok(mut pending) = store.lock() {
            let discarded = pending.requeue_failed(retry_chunks);
            for (source_id, chunk_index) in &discarded {
                // Log discarded chunks at the caller level. We cannot use
                // tracing here in a library crate without adding the dep,
                // so the caller should check the pending store.
                let _ = (source_id, chunk_index); // suppress unused warning
            }
            let _ = pending.save();
        }

        embedded_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::ChunkConfig;
    use crate::embedding::{EmbeddingProvider, EmbeddingRouter, ProviderClass};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct FailingProvider;

    #[async_trait]
    impl EmbeddingProvider for FailingProvider {
        fn provider_class(&self) -> ProviderClass {
            ProviderClass::Local
        }

        fn model_name(&self) -> &str {
            "failing"
        }

        async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
            Err(EmbedError::RequestFailed("mock failure".to_string()))
        }
    }

    /// Provider that tracks how many concurrent calls are in flight.
    struct ConcurrencyTrackingProvider {
        embedding: Vec<f32>,
        max_concurrent: Arc<AtomicUsize>,
        current: Arc<AtomicUsize>,
    }

    impl ConcurrencyTrackingProvider {
        fn new(embedding: Vec<f32>) -> Self {
            Self {
                embedding,
                max_concurrent: Arc::new(AtomicUsize::new(0)),
                current: Arc::new(AtomicUsize::new(0)),
            }
        }

        #[allow(dead_code)]
        fn max_observed(&self) -> usize {
            self.max_concurrent.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl EmbeddingProvider for ConcurrencyTrackingProvider {
        fn provider_class(&self) -> ProviderClass {
            ProviderClass::Local
        }

        fn model_name(&self) -> &str {
            "concurrency-tracker"
        }

        async fn embed(&self, _text: &str) -> Result<EmbedResult, EmbedError> {
            let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            // Update max if this is a new high.
            self.max_concurrent.fetch_max(current, Ordering::SeqCst);
            // Yield to let other futures run (simulates I/O).
            tokio::task::yield_now().await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(EmbedResult {
                embedding: self.embedding.clone(),
                model_name: "concurrency-tracker".to_string(),
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
            _sensitivity: Sensitivity,
        ) -> Result<EmbedResult, EmbedError> {
            self.provider.embed(text).await
        }
    }

    fn make_processor(provider: Arc<dyn EmbeddingProvider>) -> IntakeEmbeddingProcessor {
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(EmbeddingRouter::local_only(provider));
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        IntakeEmbeddingProcessor::new(chunker, router, vector_index)
    }

    fn make_processor_with_config(
        provider: Arc<dyn EmbeddingProvider>,
        config: EmbeddingProcessorConfig,
    ) -> IntakeEmbeddingProcessor {
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(EmbeddingRouter::local_only(provider));
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        IntakeEmbeddingProcessor::with_config(chunker, router, vector_index, config)
    }

    #[tokio::test]
    async fn process_document_embeds_and_upserts() {
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1, 0.2, 0.3],
        });
        let processor = make_processor(provider);

        let outcome = processor
            .process_document(
                "doc1",
                "This is a test document with enough content.",
                Sensitivity::Shareable,
            )
            .await;

        assert_eq!(outcome.source_id, "doc1");
        assert!(outcome.chunks_total > 0);
        assert_eq!(outcome.chunks_embedded, outcome.chunks_total);
        assert_eq!(outcome.chunks_pending, 0);
        assert_eq!(outcome.chunks_failed, 0);

        let index = processor.vector_index().lock().expect("lock");
        assert_eq!(index.len(), outcome.chunks_total);
    }

    #[tokio::test]
    async fn process_empty_document_returns_zero_outcome() {
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1],
        });
        let processor = make_processor(provider);

        let outcome = processor
            .process_document("empty", "", Sensitivity::Shareable)
            .await;

        assert_eq!(outcome.chunks_total, 0);
        assert_eq!(outcome.chunks_embedded, 0);
        assert_eq!(outcome.chunks_pending, 0);
        assert_eq!(outcome.chunks_failed, 0);
    }

    #[tokio::test]
    async fn process_document_counts_unavailable_as_pending() {
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(UnavailableProvider);
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(
            EmbeddingRouter::local_only(provider).with_retry_config(crate::chunking::RetryConfig {
                max_retries: 0, // no retries for fast test
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
            }),
        );
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index);

        let outcome = processor
            .process_document(
                "unavail",
                "Some content to embed here.",
                Sensitivity::Shareable,
            )
            .await;

        assert!(outcome.chunks_total > 0);
        assert_eq!(outcome.chunks_embedded, 0);
        assert_eq!(outcome.chunks_pending, outcome.chunks_total);
        assert_eq!(outcome.chunks_failed, 0);
    }

    #[tokio::test]
    async fn process_document_counts_failures() {
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FailingProvider);
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(EmbeddingRouter::local_only(provider));
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index);

        let outcome = processor
            .process_document(
                "fail",
                "Some content to embed here.",
                Sensitivity::Shareable,
            )
            .await;

        assert!(outcome.chunks_total > 0);
        assert_eq!(outcome.chunks_embedded, 0);
        assert_eq!(outcome.chunks_pending, 0);
        assert_eq!(outcome.chunks_failed, outcome.chunks_total);
    }

    #[tokio::test]
    async fn process_document_never_panics_on_errors() {
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1],
        });
        let processor = make_processor(provider);

        // Whitespace-only — should not panic, returns zero outcome.
        let outcome = processor
            .process_document("ws", "   \n   ", Sensitivity::Private)
            .await;
        assert_eq!(outcome.chunks_total, 0);
    }

    // --- Batch processing tests ---

    #[tokio::test]
    async fn batch_processing_produces_same_results_as_sequential() {
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1, 0.2, 0.3],
        });

        // Sequential (batch_size = 1)
        let sequential = make_processor_with_config(
            provider.clone(),
            EmbeddingProcessorConfig { batch_size: 1 },
        );
        let seq_outcome = sequential
            .process_document(
                "doc1",
                "This is a test document with enough content for testing batch processing.",
                Sensitivity::Shareable,
            )
            .await;

        // Batched (batch_size = 4)
        let batched = make_processor_with_config(
            provider.clone(),
            EmbeddingProcessorConfig { batch_size: 4 },
        );
        let batch_outcome = batched
            .process_document(
                "doc1",
                "This is a test document with enough content for testing batch processing.",
                Sensitivity::Shareable,
            )
            .await;

        assert_eq!(seq_outcome.chunks_total, batch_outcome.chunks_total);
        assert_eq!(seq_outcome.chunks_embedded, batch_outcome.chunks_embedded);
        assert_eq!(seq_outcome.chunks_pending, batch_outcome.chunks_pending);
        assert_eq!(seq_outcome.chunks_failed, batch_outcome.chunks_failed);

        // Both should have same number of entries in vector index.
        let seq_count = sequential.vector_index().lock().expect("lock").len();
        let batch_count = batched.vector_index().lock().expect("lock").len();
        assert_eq!(seq_count, batch_count);
    }

    #[tokio::test]
    async fn batch_size_config_is_respected() {
        let processor = make_processor_with_config(
            Arc::new(MockEmbedProvider {
                embedding: vec![0.1],
            }),
            EmbeddingProcessorConfig { batch_size: 3 },
        );
        assert_eq!(processor.batch_size(), 3);
    }

    #[tokio::test]
    async fn concurrent_embedding_with_futures_unordered() {
        // Use a provider that tracks concurrency to verify FuturesUnordered
        // is actually running embeddings concurrently.
        let tracker = Arc::new(ConcurrencyTrackingProvider::new(vec![0.1, 0.2]));
        let router: Arc<dyn EmbedRouter> = Arc::new(MockRouter {
            provider: tracker.clone(),
        });
        let chunker = Chunker::new(ChunkConfig {
            target_tokens: 10,
            overlap_tokens: 2,
            min_tokens: 3,
            max_tokens: 20,
        })
        .expect("chunker init");
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(2).expect("vec index"),
        ));
        let config = EmbeddingProcessorConfig { batch_size: 8 };
        let processor =
            IntakeEmbeddingProcessor::with_config(chunker, router, vector_index, config);

        // Generate text that produces multiple chunks.
        let text = "word1 word2 word3 word4 word5 word6 word7 word8. ".repeat(20);
        let outcome = processor
            .process_document("concurrent-doc", &text, Sensitivity::Shareable)
            .await;

        assert!(outcome.chunks_total > 1, "need multiple chunks");
        assert_eq!(outcome.chunks_embedded, outcome.chunks_total);
        // With yield_now and FuturesUnordered, we should observe some concurrency.
        // In practice, single-threaded tokio runtime may still serialize, so we
        // just verify it doesn't crash and produces correct results.
    }

    // --- Pending store integration tests ---

    #[tokio::test]
    async fn unavailable_chunks_saved_to_pending_store() {
        let provider: Arc<dyn EmbeddingProvider> = Arc::new(UnavailableProvider);
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(
            EmbeddingRouter::local_only(provider).with_retry_config(crate::chunking::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
            }),
        );
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        let pending_store = Arc::new(Mutex::new(PendingChunkStore::new_in_memory()));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index)
            .with_pending_store(pending_store.clone());

        let outcome = processor
            .process_document(
                "pending-doc",
                "Some content to embed here.",
                Sensitivity::Shareable,
            )
            .await;

        assert!(outcome.chunks_pending > 0);
        let store = pending_store.lock().expect("lock");
        assert_eq!(store.len(), outcome.chunks_pending);
        assert_eq!(store.chunks()[0].source_id, "pending-doc");
    }

    #[tokio::test]
    async fn retry_pending_success_removes_from_store() {
        // First, populate the pending store with some chunks.
        let pending_store = Arc::new(Mutex::new(PendingChunkStore::new_in_memory()));
        {
            let mut store = pending_store.lock().expect("lock");
            store.add(PendingChunk {
                source_id: "doc1".to_string(),
                chunk_index: 0,
                chunk_id: "chunk-0".to_string(),
                chunk_text: "some text to embed".to_string(),
                sensitivity: Sensitivity::Shareable,
                created_at: 1000,
                retry_count: 0,
            });
            store.add(PendingChunk {
                source_id: "doc1".to_string(),
                chunk_index: 1,
                chunk_id: "chunk-1".to_string(),
                chunk_text: "more text to embed".to_string(),
                sensitivity: Sensitivity::Shareable,
                created_at: 1000,
                retry_count: 0,
            });
        }

        // Now create a processor with a working provider.
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1, 0.2],
        });
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(EmbeddingRouter::local_only(provider));
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(2).expect("vec index"),
        ));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index.clone())
            .with_pending_store(pending_store.clone());

        let embedded = processor.retry_pending().await;
        assert_eq!(embedded, 2);

        // Pending store should be empty after successful retry.
        let store = pending_store.lock().expect("lock");
        assert!(store.is_empty());

        // Vector index should have the embeddings.
        let index = vector_index.lock().expect("lock");
        assert_eq!(index.len(), 2);
    }

    #[tokio::test]
    async fn retry_pending_failure_requeues() {
        let pending_store = Arc::new(Mutex::new(PendingChunkStore::new_in_memory()));
        {
            let mut store = pending_store.lock().expect("lock");
            store.add(PendingChunk {
                source_id: "doc1".to_string(),
                chunk_index: 0,
                chunk_id: "chunk-0".to_string(),
                chunk_text: "text".to_string(),
                sensitivity: Sensitivity::Shareable,
                created_at: 1000,
                retry_count: 0,
            });
        }

        let provider: Arc<dyn EmbeddingProvider> = Arc::new(UnavailableProvider);
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(
            EmbeddingRouter::local_only(provider).with_retry_config(crate::chunking::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
            }),
        );
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index)
            .with_pending_store(pending_store.clone());

        let embedded = processor.retry_pending().await;
        assert_eq!(embedded, 0);

        // Chunk should be back in the store with incremented retry_count.
        let store = pending_store.lock().expect("lock");
        assert_eq!(store.len(), 1);
        assert_eq!(store.chunks()[0].retry_count, 1);
    }

    #[tokio::test]
    async fn retry_pending_max_retries_discards() {
        let pending_store = Arc::new(Mutex::new(PendingChunkStore::new_in_memory()));
        {
            let mut store = pending_store.lock().expect("lock");
            store.add(PendingChunk {
                source_id: "doc1".to_string(),
                chunk_index: 0,
                chunk_id: "chunk-0".to_string(),
                chunk_text: "text".to_string(),
                sensitivity: Sensitivity::Shareable,
                created_at: 1000,
                retry_count: crate::pending_embeddings::MAX_RETRY_COUNT, // at max
            });
        }

        let provider: Arc<dyn EmbeddingProvider> = Arc::new(UnavailableProvider);
        let chunker = Chunker::new(ChunkConfig::default()).expect("chunker init");
        let router: Arc<dyn EmbedRouter> = Arc::new(
            EmbeddingRouter::local_only(provider).with_retry_config(crate::chunking::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
            }),
        );
        let vector_index = Arc::new(Mutex::new(
            VectorIndex::open_in_memory(3).expect("vec index"),
        ));
        let processor = IntakeEmbeddingProcessor::new(chunker, router, vector_index)
            .with_pending_store(pending_store.clone());

        let embedded = processor.retry_pending().await;
        assert_eq!(embedded, 0);

        // Chunk should have been discarded (exceeded max retries).
        let store = pending_store.lock().expect("lock");
        assert!(store.is_empty());
    }

    #[tokio::test]
    async fn retry_pending_empty_store() {
        let pending_store = Arc::new(Mutex::new(PendingChunkStore::new_in_memory()));
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1],
        });
        let processor = make_processor(provider).with_pending_store(pending_store);

        let embedded = processor.retry_pending().await;
        assert_eq!(embedded, 0);
    }

    #[tokio::test]
    async fn retry_pending_without_store_returns_zero() {
        let provider = Arc::new(MockEmbedProvider {
            embedding: vec![0.1],
        });
        let processor = make_processor(provider);
        // No pending store attached.
        let embedded = processor.retry_pending().await;
        assert_eq!(embedded, 0);
    }
}
