//! Usage metering system for AI provider calls.
//!
//! Provides an append-only NDJSON log ([`UsageLog`]) that records every AI
//! request with its token counts, cost, and metadata. Metered wrappers
//! ([`MeteredCompletionProvider`], [`MeteredEmbeddingProvider`]) delegate
//! to an inner provider and transparently log usage after each successful call.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::{
    CapabilitySet, CompletionProvider, CompletionRequest, CompletionResponse, EmbedResult,
    EmbeddingProvider, ModelProvider, PricingInfo, ProviderClass, ProviderError, RequestType,
    UsageRecord,
};

// ---------------------------------------------------------------------------
// UsageFilter
// ---------------------------------------------------------------------------

/// Filter criteria for querying usage records.
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    /// Only include records from this provider name.
    pub provider: Option<String>,
    /// Only include records using this model.
    pub model: Option<String>,
    /// Only include records of this request type.
    pub request_type: Option<RequestType>,
    /// Only include records from this source.
    pub source: Option<String>,
    /// Only include records at or after this Unix timestamp.
    pub since: Option<u64>,
    /// Only include records before this Unix timestamp.
    pub until: Option<u64>,
}

impl UsageFilter {
    /// Check whether a record matches all set filter criteria.
    fn matches(&self, record: &UsageRecord) -> bool {
        if let Some(ref provider) = self.provider {
            if record.provider != *provider {
                return false;
            }
        }
        if let Some(ref model) = self.model {
            if record.model != *model {
                return false;
            }
        }
        if let Some(request_type) = self.request_type {
            if record.request_type != request_type {
                return false;
            }
        }
        if let Some(ref source) = self.source {
            if record.source != *source {
                return false;
            }
        }
        if let Some(since) = self.since {
            if record.timestamp < since {
                return false;
            }
        }
        if let Some(until) = self.until {
            if record.timestamp >= until {
                return false;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// UsageAggregate
// ---------------------------------------------------------------------------

/// Aggregated usage statistics from a set of records.
#[derive(Debug, Clone, Default)]
pub struct UsageAggregate {
    /// Total input tokens across all matched records.
    pub total_input_tokens: u64,
    /// Total output tokens across all matched records.
    pub total_output_tokens: u64,
    /// Total media units across all matched records.
    pub total_media_units: u64,
    /// Total estimated cost in USD (if all records have pricing).
    pub total_cost_usd: Option<f64>,
    /// Number of records matched.
    pub record_count: u64,
}

// ---------------------------------------------------------------------------
// UsageLog
// ---------------------------------------------------------------------------

/// Append-only usage log backed by an NDJSON file.
///
/// Each line in the file is a JSON-serialised [`UsageRecord`]. The writer
/// flushes after every append for durability. Queries re-read from disk
/// each time to avoid stale caches.
pub struct UsageLog {
    path: PathBuf,
    writer: Mutex<BufWriter<std::fs::File>>,
}

impl UsageLog {
    /// Open or create the usage log file in append mode.
    pub fn open(path: &std::path::Path) -> Result<Self, ProviderError> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| ProviderError::ConfigError(format!("cannot open usage log: {e}")))?;

        Ok(Self {
            path: path.to_path_buf(),
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    /// Append a usage record as a JSON line and flush.
    pub fn record(&self, record: &UsageRecord) -> Result<(), ProviderError> {
        let line = serde_json::to_string(record).map_err(|e| {
            ProviderError::ConfigError(format!("cannot serialise usage record: {e}"))
        })?;

        let mut writer = self.writer.lock().map_err(|e| {
            ProviderError::ConfigError(format!("usage log writer lock poisoned: {e}"))
        })?;
        writeln!(writer, "{line}")
            .map_err(|e| ProviderError::ConfigError(format!("cannot write usage record: {e}")))?;
        writer
            .flush()
            .map_err(|e| ProviderError::ConfigError(format!("cannot flush usage log: {e}")))?;

        Ok(())
    }

    /// Query records matching filter criteria.
    ///
    /// Opens a fresh reader each time so results are never stale.
    pub fn query(&self, filter: &UsageFilter) -> Result<Vec<UsageRecord>, ProviderError> {
        let file = std::fs::File::open(&self.path)
            .map_err(|e| ProviderError::ConfigError(format!("cannot read usage log: {e}")))?;
        let reader = BufReader::new(file);
        let mut results = Vec::new();

        for line in reader.lines() {
            let line = line.map_err(|e| {
                ProviderError::ConfigError(format!("error reading usage log line: {e}"))
            })?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let record: UsageRecord = serde_json::from_str(trimmed)
                .map_err(|e| ProviderError::ConfigError(format!("malformed usage record: {e}")))?;
            if filter.matches(&record) {
                results.push(record);
            }
        }

        Ok(results)
    }

    /// Aggregate usage statistics for records matching the filter.
    pub fn aggregate(&self, filter: &UsageFilter) -> Result<UsageAggregate, ProviderError> {
        let records = self.query(filter)?;
        let mut agg = UsageAggregate::default();
        let mut cost_sum: f64 = 0.0;
        let mut has_cost = false;

        for r in &records {
            agg.total_input_tokens += r.input_tokens;
            agg.total_output_tokens += r.output_tokens;
            agg.total_media_units += r.media_units;
            agg.record_count += 1;
            if let Some(cost) = r.cost_usd {
                cost_sum += cost;
                has_cost = true;
            }
        }

        agg.total_cost_usd = if has_cost { Some(cost_sum) } else { None };
        Ok(agg)
    }
}

// ---------------------------------------------------------------------------
// Cost computation helpers
// ---------------------------------------------------------------------------

/// Compute the estimated USD cost of a completion call.
fn compute_completion_cost(
    provider: &dyn ModelProvider,
    input_tokens: u64,
    output_tokens: u64,
) -> Option<f64> {
    let pricing = provider.pricing()?;
    let input_cost = pricing
        .input_per_1k_tokens
        .map(|p| p * input_tokens as f64 / 1000.0);
    let output_cost = pricing
        .output_per_1k_tokens
        .map(|p| p * output_tokens as f64 / 1000.0);
    match (input_cost, output_cost) {
        (Some(i), Some(o)) => Some(i + o),
        (Some(i), None) => Some(i),
        (None, Some(o)) => Some(o),
        (None, None) => None,
    }
}

/// Compute the estimated USD cost of an embedding call.
fn compute_embedding_cost(provider: &dyn ModelProvider, token_count: u64) -> Option<f64> {
    let pricing = provider.pricing()?;
    pricing
        .embedding_per_1k_tokens
        .map(|p| p * token_count as f64 / 1000.0)
}

// ---------------------------------------------------------------------------
// MeteredCompletionProvider
// ---------------------------------------------------------------------------

/// Wraps a [`CompletionProvider`] to record usage after each call.
///
/// Logging is best-effort: if the record fails to write, the provider
/// result is still returned successfully.
pub struct MeteredCompletionProvider {
    inner: Arc<dyn CompletionProvider>,
    log: Arc<UsageLog>,
    source: String,
}

impl MeteredCompletionProvider {
    /// Create a new metered wrapper.
    pub fn new(inner: Arc<dyn CompletionProvider>, log: Arc<UsageLog>, source: String) -> Self {
        Self { inner, log, source }
    }
}

impl ModelProvider for MeteredCompletionProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn provider_class(&self) -> ProviderClass {
        self.inner.provider_class()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn capabilities(&self) -> &CapabilitySet {
        self.inner.capabilities()
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        self.inner.pricing()
    }
}

#[async_trait]
impl CompletionProvider for MeteredCompletionProvider {
    async fn complete(
        &self,
        request: &CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let response = self.inner.complete(request).await?;

        // Build and log the usage record (best-effort).
        let input_tokens = response.input_tokens.unwrap_or(0);
        let output_tokens = response.output_tokens.unwrap_or(0);
        let cost = compute_completion_cost(self.inner.as_ref(), input_tokens, output_tokens);

        let record = UsageRecord {
            provider: self.inner.name().to_string(),
            model: response.model.clone(),
            timestamp: symbiotic_core::now_unix(),
            input_tokens,
            output_tokens,
            media_units: 0,
            cost_usd: cost,
            request_type: RequestType::Completion,
            source: self.source.clone(),
            session_id: None,
        };

        // Best-effort: ignore logging errors.
        let _ = self.log.record(&record);

        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// MeteredEmbeddingProvider
// ---------------------------------------------------------------------------

/// Wraps an [`EmbeddingProvider`] to record usage after each call.
///
/// Logging is best-effort: if the record fails to write, the provider
/// result is still returned successfully.
pub struct MeteredEmbeddingProvider {
    inner: Arc<dyn EmbeddingProvider>,
    log: Arc<UsageLog>,
    source: String,
}

impl MeteredEmbeddingProvider {
    /// Create a new metered wrapper.
    pub fn new(inner: Arc<dyn EmbeddingProvider>, log: Arc<UsageLog>, source: String) -> Self {
        Self { inner, log, source }
    }
}

impl ModelProvider for MeteredEmbeddingProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn provider_class(&self) -> ProviderClass {
        self.inner.provider_class()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn capabilities(&self) -> &CapabilitySet {
        self.inner.capabilities()
    }

    fn pricing(&self) -> Option<&PricingInfo> {
        self.inner.pricing()
    }
}

#[async_trait]
impl EmbeddingProvider for MeteredEmbeddingProvider {
    async fn embed(&self, text: &str) -> Result<EmbedResult, ProviderError> {
        let result = self.inner.embed(text).await?;

        // Estimate token count from embedding dimensions (rough heuristic).
        // A better approach would be to have the inner provider report token counts,
        // but EmbedResult doesn't carry that info today.
        let estimated_tokens = (text.len() / 4) as u64; // ~4 chars per token
        let cost = compute_embedding_cost(self.inner.as_ref(), estimated_tokens);

        let record = UsageRecord {
            provider: self.inner.name().to_string(),
            model: result.model_name.clone(),
            timestamp: symbiotic_core::now_unix(),
            input_tokens: estimated_tokens,
            output_tokens: 0,
            media_units: 0,
            cost_usd: cost,
            request_type: RequestType::Embedding,
            source: self.source.clone(),
            session_id: None,
        };

        let _ = self.log.record(&record);

        Ok(result)
    }

    async fn embed_batch(&self, texts: &[&str]) -> Result<Vec<EmbedResult>, ProviderError> {
        let results = self.inner.embed_batch(texts).await?;

        let total_chars: usize = texts.iter().map(|t| t.len()).sum();
        let estimated_tokens = (total_chars / 4) as u64;
        let cost = compute_embedding_cost(self.inner.as_ref(), estimated_tokens);

        let model_name = results
            .first()
            .map(|r| r.model_name.clone())
            .unwrap_or_else(|| self.inner.model_name().to_string());

        let record = UsageRecord {
            provider: self.inner.name().to_string(),
            model: model_name,
            timestamp: symbiotic_core::now_unix(),
            input_tokens: estimated_tokens,
            output_tokens: 0,
            media_units: 0,
            cost_usd: cost,
            request_type: RequestType::Embedding,
            source: self.source.clone(),
            session_id: None,
        };

        let _ = self.log.record(&record);

        Ok(results)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CapabilitySet, PricingInfo, ProviderCapability, ProviderClass};
    use std::sync::Arc;
    use tempfile::TempDir;

    // -- Mock providers -------------------------------------------------------

    struct MockCompletionProvider {
        response: CompletionResponse,
        pricing: Option<PricingInfo>,
    }

    impl ModelProvider for MockCompletionProvider {
        fn name(&self) -> &str {
            "mock"
        }
        fn provider_class(&self) -> ProviderClass {
            ProviderClass::Cloud
        }
        fn model_name(&self) -> &str {
            "mock-model"
        }
        fn capabilities(&self) -> &CapabilitySet {
            // Leaked for test simplicity; tests are short-lived.
            Box::leak(Box::new(CapabilitySet::new(vec![
                ProviderCapability::Completion,
            ])))
        }
        fn pricing(&self) -> Option<&PricingInfo> {
            self.pricing.as_ref()
        }
    }

    #[async_trait]
    impl CompletionProvider for MockCompletionProvider {
        async fn complete(
            &self,
            _request: &CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(self.response.clone())
        }
    }

    struct MockEmbeddingProvider {
        result: EmbedResult,
        pricing: Option<PricingInfo>,
    }

    impl ModelProvider for MockEmbeddingProvider {
        fn name(&self) -> &str {
            "mock-embed"
        }
        fn provider_class(&self) -> ProviderClass {
            ProviderClass::Local
        }
        fn model_name(&self) -> &str {
            "mock-embed-model"
        }
        fn capabilities(&self) -> &CapabilitySet {
            Box::leak(Box::new(CapabilitySet::new(vec![
                ProviderCapability::Embedding,
            ])))
        }
        fn pricing(&self) -> Option<&PricingInfo> {
            self.pricing.as_ref()
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockEmbeddingProvider {
        async fn embed(&self, _text: &str) -> Result<EmbedResult, ProviderError> {
            Ok(self.result.clone())
        }
    }

    // -- Helper ---------------------------------------------------------------

    fn make_log(dir: &TempDir) -> Arc<UsageLog> {
        let path = dir.path().join("usage.ndjson");
        Arc::new(UsageLog::open(&path).unwrap())
    }

    fn sample_record(provider: &str, model: &str, source: &str, ts: u64) -> UsageRecord {
        UsageRecord {
            provider: provider.to_string(),
            model: model.to_string(),
            timestamp: ts,
            input_tokens: 100,
            output_tokens: 50,
            media_units: 0,
            cost_usd: Some(0.003),
            request_type: RequestType::Completion,
            source: source.to_string(),
            session_id: None,
        }
    }

    // -- Tests ----------------------------------------------------------------

    #[test]
    fn usage_log_open_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.ndjson");
        assert!(!path.exists());

        let _log = UsageLog::open(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn usage_log_record_and_query() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let record = sample_record("openai", "gpt-4o", "agent", 1000);
        log.record(&record).unwrap();

        let results = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider, "openai");
        assert_eq!(results[0].model, "gpt-4o");
        assert_eq!(results[0].input_tokens, 100);
        assert_eq!(results[0].output_tokens, 50);
        assert_eq!(results[0].cost_usd, Some(0.003));
    }

    #[test]
    fn usage_log_multiple_records() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("openai", "gpt-4o", "agent", 1000))
            .unwrap();
        log.record(&sample_record("anthropic", "claude-3", "ingestion", 2000))
            .unwrap();
        log.record(&sample_record("openai", "gpt-4o-mini", "marketing", 3000))
            .unwrap();

        let all = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn usage_filter_by_provider() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("openai", "gpt-4o", "agent", 1000))
            .unwrap();
        log.record(&sample_record("anthropic", "claude-3", "agent", 2000))
            .unwrap();

        let filter = UsageFilter {
            provider: Some("openai".to_string()),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider, "openai");
    }

    #[test]
    fn usage_filter_by_model() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("openai", "gpt-4o", "agent", 1000))
            .unwrap();
        log.record(&sample_record("openai", "gpt-4o-mini", "agent", 2000))
            .unwrap();

        let filter = UsageFilter {
            model: Some("gpt-4o-mini".to_string()),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].model, "gpt-4o-mini");
    }

    #[test]
    fn usage_filter_by_request_type() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let mut completion = sample_record("openai", "gpt-4o", "agent", 1000);
        completion.request_type = RequestType::Completion;
        let mut embedding = sample_record("openai", "embed", "ingestion", 2000);
        embedding.request_type = RequestType::Embedding;

        log.record(&completion).unwrap();
        log.record(&embedding).unwrap();

        let filter = UsageFilter {
            request_type: Some(RequestType::Embedding),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].request_type, RequestType::Embedding);
    }

    #[test]
    fn usage_filter_by_source() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("openai", "gpt-4o", "agent", 1000))
            .unwrap();
        log.record(&sample_record("openai", "gpt-4o", "ingestion", 2000))
            .unwrap();

        let filter = UsageFilter {
            source: Some("ingestion".to_string()),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].source, "ingestion");
    }

    #[test]
    fn usage_filter_by_time_range() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("a", "m", "s", 1000)).unwrap();
        log.record(&sample_record("b", "m", "s", 2000)).unwrap();
        log.record(&sample_record("c", "m", "s", 3000)).unwrap();

        // since=1500, until=2500 should match only ts=2000
        let filter = UsageFilter {
            since: Some(1500),
            until: Some(2500),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider, "b");
    }

    #[test]
    fn usage_filter_combined() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        log.record(&sample_record("openai", "gpt-4o", "agent", 1000))
            .unwrap();
        log.record(&sample_record("openai", "gpt-4o", "ingestion", 2000))
            .unwrap();
        log.record(&sample_record("anthropic", "claude-3", "agent", 3000))
            .unwrap();

        let filter = UsageFilter {
            provider: Some("openai".to_string()),
            source: Some("agent".to_string()),
            ..Default::default()
        };
        let results = log.query(&filter).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].timestamp, 1000);
    }

    #[test]
    fn usage_aggregate_basic() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let mut r1 = sample_record("openai", "gpt-4o", "agent", 1000);
        r1.input_tokens = 200;
        r1.output_tokens = 100;
        r1.cost_usd = Some(0.005);

        let mut r2 = sample_record("openai", "gpt-4o", "agent", 2000);
        r2.input_tokens = 300;
        r2.output_tokens = 150;
        r2.cost_usd = Some(0.008);

        log.record(&r1).unwrap();
        log.record(&r2).unwrap();

        let agg = log.aggregate(&UsageFilter::default()).unwrap();
        assert_eq!(agg.record_count, 2);
        assert_eq!(agg.total_input_tokens, 500);
        assert_eq!(agg.total_output_tokens, 250);
        assert!((agg.total_cost_usd.unwrap() - 0.013).abs() < 1e-9);
    }

    #[test]
    fn usage_aggregate_with_filter() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let mut r1 = sample_record("openai", "gpt-4o", "agent", 1000);
        r1.input_tokens = 100;
        r1.cost_usd = Some(0.001);

        let mut r2 = sample_record("anthropic", "claude-3", "agent", 2000);
        r2.input_tokens = 200;
        r2.cost_usd = Some(0.002);

        log.record(&r1).unwrap();
        log.record(&r2).unwrap();

        let filter = UsageFilter {
            provider: Some("anthropic".to_string()),
            ..Default::default()
        };
        let agg = log.aggregate(&filter).unwrap();
        assert_eq!(agg.record_count, 1);
        assert_eq!(agg.total_input_tokens, 200);
        assert!((agg.total_cost_usd.unwrap() - 0.002).abs() < 1e-9);
    }

    #[test]
    fn usage_aggregate_no_cost() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let mut r = sample_record("ollama", "llama3", "agent", 1000);
        r.cost_usd = None;
        log.record(&r).unwrap();

        let agg = log.aggregate(&UsageFilter::default()).unwrap();
        assert_eq!(agg.record_count, 1);
        assert!(agg.total_cost_usd.is_none());
    }

    #[test]
    fn usage_aggregate_partial_cost() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let mut r1 = sample_record("openai", "gpt-4o", "agent", 1000);
        r1.cost_usd = Some(0.01);
        let mut r2 = sample_record("ollama", "llama3", "agent", 2000);
        r2.cost_usd = None;

        log.record(&r1).unwrap();
        log.record(&r2).unwrap();

        let agg = log.aggregate(&UsageFilter::default()).unwrap();
        assert_eq!(agg.record_count, 2);
        // Only the record with cost contributes.
        assert!((agg.total_cost_usd.unwrap() - 0.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn metered_completion_provider_logs_usage() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let inner = Arc::new(MockCompletionProvider {
            response: CompletionResponse {
                content: "Hello".to_string(),
                model: "mock-model".to_string(),
                input_tokens: Some(10),
                output_tokens: Some(5),
                finish_reason: Some("stop".to_string()),
            },
            pricing: Some(PricingInfo {
                input_per_1k_tokens: Some(0.03),
                output_per_1k_tokens: Some(0.06),
                ..Default::default()
            }),
        });

        let metered = MeteredCompletionProvider::new(inner, log.clone(), "test-agent".to_string());

        let request = CompletionRequest {
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stop: None,
            model_hint: crate::types::ModelHint::Default,
        };

        let response = metered.complete(&request).await.unwrap();
        assert_eq!(response.content, "Hello");

        // Verify the usage record was logged.
        let records = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider, "mock");
        assert_eq!(records[0].model, "mock-model");
        assert_eq!(records[0].input_tokens, 10);
        assert_eq!(records[0].output_tokens, 5);
        assert_eq!(records[0].request_type, RequestType::Completion);
        assert_eq!(records[0].source, "test-agent");
        assert!(records[0].cost_usd.is_some());

        // Expected cost: (0.03 * 10/1000) + (0.06 * 5/1000) = 0.0003 + 0.0003 = 0.0006
        let cost = records[0].cost_usd.unwrap();
        assert!((cost - 0.0006).abs() < 1e-9);
    }

    #[tokio::test]
    async fn metered_completion_provider_no_pricing() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let inner = Arc::new(MockCompletionProvider {
            response: CompletionResponse {
                content: "Hi".to_string(),
                model: "mock-model".to_string(),
                input_tokens: Some(5),
                output_tokens: Some(3),
                finish_reason: None,
            },
            pricing: None,
        });

        let metered = MeteredCompletionProvider::new(inner, log.clone(), "cli".to_string());

        let request = CompletionRequest {
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stop: None,
            model_hint: crate::types::ModelHint::Default,
        };

        metered.complete(&request).await.unwrap();

        let records = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].cost_usd.is_none());
    }

    #[tokio::test]
    async fn metered_completion_delegates_model_provider() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let inner = Arc::new(MockCompletionProvider {
            response: CompletionResponse {
                content: "x".to_string(),
                model: "mock-model".to_string(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            },
            pricing: None,
        });

        let metered = MeteredCompletionProvider::new(inner, log, "src".to_string());
        assert_eq!(metered.name(), "mock");
        assert_eq!(metered.model_name(), "mock-model");
        assert_eq!(metered.provider_class(), ProviderClass::Cloud);
        assert!(metered.capabilities().has(ProviderCapability::Completion));
    }

    #[tokio::test]
    async fn metered_embedding_provider_logs_usage() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let inner = Arc::new(MockEmbeddingProvider {
            result: EmbedResult {
                embedding: vec![0.1, 0.2, 0.3],
                model_name: "mock-embed-model".to_string(),
                dimensions: 3,
            },
            pricing: Some(PricingInfo {
                embedding_per_1k_tokens: Some(0.0001),
                ..Default::default()
            }),
        });

        let metered = MeteredEmbeddingProvider::new(inner, log.clone(), "ingestion".to_string());

        let result = metered.embed("hello world test").await.unwrap();
        assert_eq!(result.embedding, vec![0.1, 0.2, 0.3]);

        let records = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].provider, "mock-embed");
        assert_eq!(records[0].request_type, RequestType::Embedding);
        assert_eq!(records[0].source, "ingestion");
        // "hello world test" = 16 chars, ~4 tokens
        assert_eq!(records[0].input_tokens, 4);
    }

    #[tokio::test]
    async fn metered_embedding_provider_batch() {
        let dir = TempDir::new().unwrap();
        let log = make_log(&dir);

        let inner = Arc::new(MockEmbeddingProvider {
            result: EmbedResult {
                embedding: vec![0.1, 0.2],
                model_name: "mock-embed-model".to_string(),
                dimensions: 2,
            },
            pricing: None,
        });

        let metered = MeteredEmbeddingProvider::new(inner, log.clone(), "batch".to_string());

        let results = metered.embed_batch(&["hello", "world"]).await.unwrap();
        assert_eq!(results.len(), 2);

        let records = log.query(&UsageFilter::default()).unwrap();
        assert_eq!(records.len(), 1);
        // "hello" (5) + "world" (5) = 10 chars, 10/4 = 2 tokens
        assert_eq!(records[0].input_tokens, 2);
    }

    #[test]
    fn compute_completion_cost_both() {
        let provider = MockCompletionProvider {
            response: CompletionResponse {
                content: String::new(),
                model: String::new(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            },
            pricing: Some(PricingInfo {
                input_per_1k_tokens: Some(0.03),
                output_per_1k_tokens: Some(0.06),
                ..Default::default()
            }),
        };

        let cost = super::compute_completion_cost(&provider, 1000, 500);
        // 0.03 * 1000/1000 + 0.06 * 500/1000 = 0.03 + 0.03 = 0.06
        assert!((cost.unwrap() - 0.06).abs() < 1e-9);
    }

    #[test]
    fn compute_completion_cost_input_only() {
        let provider = MockCompletionProvider {
            response: CompletionResponse {
                content: String::new(),
                model: String::new(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            },
            pricing: Some(PricingInfo {
                input_per_1k_tokens: Some(0.03),
                output_per_1k_tokens: None,
                ..Default::default()
            }),
        };

        let cost = super::compute_completion_cost(&provider, 1000, 500);
        assert!((cost.unwrap() - 0.03).abs() < 1e-9);
    }

    #[test]
    fn compute_completion_cost_none() {
        let provider = MockCompletionProvider {
            response: CompletionResponse {
                content: String::new(),
                model: String::new(),
                input_tokens: None,
                output_tokens: None,
                finish_reason: None,
            },
            pricing: None,
        };

        let cost = super::compute_completion_cost(&provider, 1000, 500);
        assert!(cost.is_none());
    }

    #[test]
    fn usage_log_query_reads_from_disk() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.ndjson");
        let log = UsageLog::open(&path).unwrap();

        // Write a record.
        log.record(&sample_record("a", "m", "s", 1000)).unwrap();

        // Open a second UsageLog on the same file and verify it can read what was written.
        let log2 = UsageLog::open(&path).unwrap();
        let results = log2.query(&UsageFilter::default()).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].provider, "a");
    }
}
