//! Integration test: Intake -> Archive -> Recall Gateway
//!
//! Verifies the full flow from URL/note ingestion through archive storage
//! to context retrieval, including sensitivity-based filtering and redaction.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use symbiotic_archive::{ArchiveSensitivity, FileArchiveStore, StoreRequest};
use symbiotic_context::{
    ArchiveEntry, ArchiveProvider, AuditRecord, AuditSink, ContextRequest, ModelClass, Purpose,
    RecallGateway, Sensitivity,
};
use symbiotic_core::intake::{normalize_url, IntakeKind, IntakeRequest, IntakeSource};
use symbiotic_intake::{
    ContentFetcher, FetchedContent, IntakePipeline, IntakePolicy, IntakeStore, ReviewQueue,
    SensitivityClassifier,
};
use tempfile::TempDir;
use url::Url;

// ---------------------------------------------------------------------------
// Test doubles
// ---------------------------------------------------------------------------

struct MockFetcher;

impl ContentFetcher for MockFetcher {
    fn fetch(&self, url: &Url) -> Result<FetchedContent> {
        Ok(FetchedContent {
            markdown: format!("# Article from {url}\n\nRust daemon queue architecture notes."),
            html_title: None,
            title: None,
        })
    }
}

/// IntakeStore backed by FileArchiveStore so that documents actually land on disk.
struct ArchiveBridgeStore {
    archive: Arc<FileArchiveStore>,
}

impl IntakeStore for ArchiveBridgeStore {
    fn exists(&self, idempotency_key: &str) -> Result<bool> {
        self.archive.exists_idempotency_key(idempotency_key)
    }

    fn store_archive_url(
        &self,
        url: &Url,
        content: &FetchedContent,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        let outcome = self.archive.store(StoreRequest {
            title_hint: None,
            content: content.markdown.clone(),
            source_url: Some(url.to_string()),
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }

    fn store_archive_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        let outcome = self.archive.store(StoreRequest {
            title_hint: None,
            content: note.to_string(),
            source_url: None,
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }

    fn store_vault_note(
        &self,
        note: &str,
        tags: &[String],
        idempotency_key: &str,
        firewall_verdict: symbiotic_firewall::types::FirewallVerdict,
    ) -> Result<String> {
        let outcome = self.archive.store(StoreRequest {
            title_hint: None,
            content: note.to_string(),
            source_url: None,
            tags: tags.to_vec(),
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: idempotency_key.to_string(),
            firewall_verdict: Some(firewall_verdict),
        })?;
        Ok(outcome.record_id)
    }
}

struct NoOpQueue;

impl ReviewQueue for NoOpQueue {
    fn enqueue(&self, record_id: &str) -> Result<String> {
        Ok(format!("review_{record_id}"))
    }
}

struct LowClassifier;

impl SensitivityClassifier for LowClassifier {
    fn classify_note(&self, _note: &str) -> symbiotic_intake::Sensitivity {
        symbiotic_intake::Sensitivity::Low
    }
}

struct HighClassifier;

impl SensitivityClassifier for HighClassifier {
    fn classify_note(&self, _note: &str) -> symbiotic_intake::Sensitivity {
        symbiotic_intake::Sensitivity::High
    }
}

/// Wraps FileArchiveStore as an ArchiveProvider for the Recall Gateway.
struct ArchiveProviderBridge {
    archive: Arc<FileArchiveStore>,
}

impl ArchiveProvider for ArchiveProviderBridge {
    fn list_entries(&self) -> Result<Vec<ArchiveEntry>> {
        let docs = self.archive.list()?;
        Ok(docs
            .into_iter()
            .map(|doc| {
                let sensitivity = match doc.sensitivity {
                    ArchiveSensitivity::Shareable => Sensitivity::Shareable,
                    ArchiveSensitivity::Restricted => Sensitivity::Restricted,
                    ArchiveSensitivity::Private => Sensitivity::Private,
                };
                ArchiveEntry {
                    id: doc.record_id,
                    title: doc.title,
                    content: doc.content,
                    tags: doc.tags,
                    sensitivity,
                    source_url: doc.source_url,
                    updated_at: doc.updated_at,
                    thread_id: None,
                    fact_class: None,
                }
            })
            .collect())
    }
}

#[derive(Default)]
struct CollectingAudit {
    records: Mutex<Vec<AuditRecord>>,
}

impl AuditSink for CollectingAudit {
    fn record(&self, record: AuditRecord) -> Result<()> {
        self.records.lock().expect("audit lock").push(record);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn ingest_url_then_recall_returns_entry() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    // Step 1: Ingest a URL through the intake pipeline
    let pipeline = IntakePipeline::new(
        Arc::new(MockFetcher),
        Arc::new(ArchiveBridgeStore {
            archive: archive.clone(),
        }),
        Arc::new(NoOpQueue),
        Arc::new(LowClassifier),
        IntakePolicy::default(),
    );

    let url = normalize_url("https://example.com/rust-daemon").expect("url");
    let request = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Url,
        urls: vec![url],
        note: None,
        tags: vec!["architecture".to_string()],
        file_path: None,
        title: None,
    };
    let result = pipeline.process(request).expect("ingest");
    assert_eq!(result.summary.ingested, 1);

    // Step 2: Verify the entry exists in the archive
    let docs = archive.list().expect("list");
    assert_eq!(docs.len(), 1);
    assert!(docs[0].content.contains("daemon queue architecture"));

    // Step 3: Query the Recall Gateway and verify the entry is returned
    let audit = Arc::new(CollectingAudit::default());
    let provider = Arc::new(ArchiveProviderBridge {
        archive: archive.clone(),
    });
    let gateway = RecallGateway::new(provider, audit.clone());

    let pack = gateway
        .get_context(&ContextRequest {
            request_id: "req-1".to_string(),
            query: "daemon queue".to_string(),
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
        .expect("recall");

    assert!(
        !pack.items.is_empty(),
        "recall should return the ingested entry"
    );
    assert!(
        pack.items[0].content.contains("daemon queue architecture"),
        "content should match what was ingested"
    );

    // Step 4: Verify audit was recorded
    let audit_records = audit.records.lock().expect("audit");
    assert_eq!(audit_records.len(), 1);
    assert_eq!(audit_records[0].request_id, "req-1");
}

#[test]
fn ingest_note_then_recall_returns_entry() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    let pipeline = IntakePipeline::new(
        Arc::new(MockFetcher),
        Arc::new(ArchiveBridgeStore {
            archive: archive.clone(),
        }),
        Arc::new(NoOpQueue),
        Arc::new(LowClassifier),
        IntakePolicy::default(),
    );

    let request = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Note,
        urls: vec![],
        note: Some("Meeting notes about vector search implementation".to_string()),
        tags: vec!["notes".to_string()],
        file_path: None,
        title: None,
    };
    let result = pipeline.process(request).expect("ingest");
    assert_eq!(result.summary.ingested, 1);

    let audit = Arc::new(CollectingAudit::default());
    let provider = Arc::new(ArchiveProviderBridge {
        archive: archive.clone(),
    });
    let gateway = RecallGateway::new(provider, audit);

    let pack = gateway
        .get_context(&ContextRequest {
            request_id: "req-2".to_string(),
            query: "vector search".to_string(),
            model_class: ModelClass::Local,
            purpose: Purpose::Plan,
            sensitivity_max: Sensitivity::Private,
            token_budget: 500,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("recall");

    assert!(!pack.items.is_empty());
    assert!(pack.items[0].content.contains("vector search"));
}

#[test]
fn cloud_model_redacts_private_entries() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    // Store a private entry containing sensitive data directly in archive
    archive
        .store(StoreRequest {
            title_hint: Some("Private credentials".to_string()),
            content: "Contact: user@example.com password=secret123 phone 12345678901".to_string(),
            source_url: None,
            tags: vec!["private".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: "private-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");

    let audit = Arc::new(CollectingAudit::default());
    let provider = Arc::new(ArchiveProviderBridge {
        archive: archive.clone(),
    });
    let gateway = RecallGateway::new(provider, audit.clone());

    let pack = gateway
        .get_context(&ContextRequest {
            request_id: "req-3".to_string(),
            query: "credentials".to_string(),
            model_class: ModelClass::Cloud,
            purpose: Purpose::Answer,
            sensitivity_max: Sensitivity::Shareable,
            token_budget: 500,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("recall");

    // The entry should be returned but with redacted content
    assert!(
        !pack.items.is_empty(),
        "private entries should still appear for cloud (redacted)"
    );
    let item = &pack.items[0];
    assert!(item.redacted, "entry must be marked as redacted");
    assert!(
        item.content.contains("[redacted-email]"),
        "email should be redacted"
    );
    assert!(
        item.content.contains("[redacted-phone]"),
        "phone should be redacted"
    );
    assert!(
        item.content.contains("[redacted-sensitive]"),
        "sensitive keyword should be redacted"
    );

    let audit_records = audit.records.lock().expect("audit");
    assert!(audit_records[0].redaction_applied);
}

#[test]
fn sensitivity_filter_excludes_private_entries_for_local_with_shareable_max() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    // Shareable entry
    archive
        .store(StoreRequest {
            title_hint: Some("Public notes".to_string()),
            content: "General architecture discussion".to_string(),
            source_url: None,
            tags: vec![],
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: "pub-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");

    // Private entry
    archive
        .store(StoreRequest {
            title_hint: Some("Secret plans".to_string()),
            content: "Confidential architecture roadmap".to_string(),
            source_url: None,
            tags: vec![],
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: "priv-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");

    let audit = Arc::new(CollectingAudit::default());
    let provider = Arc::new(ArchiveProviderBridge { archive });
    let gateway = RecallGateway::new(provider, audit);

    let pack = gateway
        .get_context(&ContextRequest {
            request_id: "req-4".to_string(),
            query: "architecture".to_string(),
            model_class: ModelClass::Local,
            purpose: Purpose::Answer,
            sensitivity_max: Sensitivity::Shareable,
            token_budget: 500,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("recall");

    // Only the shareable entry should be returned
    assert_eq!(pack.items.len(), 1);
    assert_eq!(pack.items[0].title, "Public notes");
}

#[test]
fn high_sensitivity_note_stored_as_private_in_archive() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    let pipeline = IntakePipeline::new(
        Arc::new(MockFetcher),
        Arc::new(ArchiveBridgeStore {
            archive: archive.clone(),
        }),
        Arc::new(NoOpQueue),
        Arc::new(HighClassifier),
        IntakePolicy::default(),
    );

    let request = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Note,
        urls: vec![],
        note: Some("password=super-secret api_key=abc123".to_string()),
        tags: vec!["credentials".to_string()],
        file_path: None,
        title: None,
    };
    let result = pipeline.process(request).expect("ingest");
    assert_eq!(result.summary.secure_routed, 1);

    // Verify the entry was stored with Private sensitivity
    let docs = archive.list().expect("list");
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].sensitivity, ArchiveSensitivity::Private);
}

#[test]
fn duplicate_url_not_stored_twice() {
    let tmp = TempDir::new().expect("tmpdir");
    let archive = Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));

    let bridge = Arc::new(ArchiveBridgeStore {
        archive: archive.clone(),
    });
    let pipeline = IntakePipeline::new(
        Arc::new(MockFetcher),
        bridge,
        Arc::new(NoOpQueue),
        Arc::new(LowClassifier),
        IntakePolicy::default(),
    );

    let url = normalize_url("https://example.com/duplicate-test").expect("url");
    let request = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Url,
        urls: vec![url.clone()],
        note: None,
        tags: vec!["test".to_string()],
        file_path: None,
        title: None,
    };

    // First ingest
    let r1 = pipeline.process(request).expect("first ingest");
    assert_eq!(r1.summary.ingested, 1);

    // Second ingest with same URL - new pipeline to share the same archive
    let bridge2 = Arc::new(ArchiveBridgeStore {
        archive: archive.clone(),
    });
    let pipeline2 = IntakePipeline::new(
        Arc::new(MockFetcher),
        bridge2,
        Arc::new(NoOpQueue),
        Arc::new(LowClassifier),
        IntakePolicy::default(),
    );
    let request2 = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Url,
        urls: vec![url],
        note: None,
        tags: vec!["test".to_string()],
        file_path: None,
        title: None,
    };
    let r2 = pipeline2.process(request2).expect("second ingest");
    assert_eq!(r2.summary.duplicates, 1);

    // Archive should still have only one entry
    let docs = archive.list().expect("list");
    assert_eq!(docs.len(), 1);
}
