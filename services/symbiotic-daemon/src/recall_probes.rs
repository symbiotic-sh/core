use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use symbiotic_archive::FileArchiveStore;
use symbiotic_context::{ContextRequest, ModelClass, Purpose, RecallGateway, Sensitivity};
use symbiotic_core::now_unix;
use symbiotic_memory::recall_probes::{
    derive_remediation_flags, generate_probe_queries, load_graph_probe_subjects, RecallProbeResult,
    RecallProbeRun, RecallProbeStatus, RecallProbeStore, RecallProbeSubject, RecallProbeTargetKind,
    RecallRemediationFlag,
};

pub const DEFAULT_RECALL_PROBE_TOP_K: usize = 10;
pub const DEFAULT_RECALL_PROBE_MAX_SUBJECTS: usize = 200;
pub const DEFAULT_RECALL_PROBE_MAX_QUERIES_PER_SUBJECT: usize = 3;
pub const RECALL_PROBE_PROPOSAL_FAILURE_THRESHOLD: u32 = 3;
pub const PERIODIC_RECALL_PROBE_COHORT: &str = "periodic_baseline";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallProbeEscalation {
    pub target_kind: RecallProbeTargetKind,
    pub target_id: String,
    pub consecutive_failures: u32,
    pub remediation_flags: Vec<RecallRemediationFlag>,
}

/// Execute one batch of recall probes against the live Recall Gateway.
#[allow(clippy::too_many_arguments)]
pub fn run_recall_probe_batch(
    recall_gateway: &RecallGateway,
    archive_store: &FileArchiveStore,
    vault_store: &FileArchiveStore,
    memory_db_path: Option<&Path>,
    probe_store: &RecallProbeStore,
    top_k: usize,
    max_subjects: usize,
    max_queries_per_subject: usize,
) -> Result<RecallProbeRun> {
    let mut subjects =
        build_probe_subjects(archive_store, vault_store, memory_db_path, max_subjects)?;
    run_recall_probe_batch_for_subjects(
        recall_gateway,
        probe_store,
        top_k,
        max_queries_per_subject,
        None,
        std::mem::take(&mut subjects),
    )
}

pub fn build_probe_subjects(
    archive_store: &FileArchiveStore,
    vault_store: &FileArchiveStore,
    memory_db_path: Option<&Path>,
    max_subjects: usize,
) -> Result<Vec<RecallProbeSubject>> {
    let mut subjects = build_archive_probe_subjects(archive_store, vault_store, max_subjects)?;
    if let Some(memory_db_path) = memory_db_path {
        let remaining = max_subjects.saturating_sub(subjects.len());
        if remaining > 0 {
            subjects.extend(load_graph_probe_subjects(memory_db_path, remaining)?);
        }
    }
    Ok(subjects)
}

pub fn run_recall_probe_batch_for_subjects(
    recall_gateway: &RecallGateway,
    probe_store: &RecallProbeStore,
    top_k: usize,
    max_queries_per_subject: usize,
    cohort: Option<String>,
    subjects: Vec<RecallProbeSubject>,
) -> Result<RecallProbeRun> {
    let started_at = now_unix();
    let run = RecallProbeRun {
        id: format!("probe-run-{}", uuid::Uuid::new_v4()),
        started_at,
        finished_at: None,
        cohort,
        top_k,
        subject_count: 0,
        matched_count: 0,
    };
    probe_store.start_run(&run)?;

    let subject_count = subjects.len();

    for subject in subjects {
        for query in generate_probe_queries(&subject, max_queries_per_subject) {
            let request = ContextRequest {
                request_id: format!("{}:{}", run.id, uuid::Uuid::new_v4()),
                query: query.clone(),
                model_class: ModelClass::Local,
                purpose: Purpose::Review,
                sensitivity_max: Sensitivity::Private,
                token_budget: 1024,
                tags: Vec::new(),
                recency_days: None,
                filter_threads: None,
                disclosure_tier: None,
                class_budget: None,
            };
            let pack = recall_gateway.get_context(&request)?;
            let top_items = pack.items.into_iter().take(top_k).collect::<Vec<_>>();
            let rank = top_items
                .iter()
                .position(|item| item.id == subject.target_id)
                .map(|index| index as u32 + 1);
            let mut result = RecallProbeResult {
                run_id: run.id.clone(),
                target_kind: subject.target_kind,
                target_id: subject.target_id.clone(),
                query,
                matched: rank.is_some(),
                rank,
                retrieval_mode: pack.retrieval_mode,
                top_item_ids: top_items.into_iter().map(|item| item.id).collect(),
                remediation_flags: Vec::new(),
                created_at: now_unix(),
            };
            result.remediation_flags = derive_remediation_flags(&result);
            probe_store.record_result(&result)?;
        }
    }

    let finished_at = now_unix();
    probe_store.finish_run(&run.id, finished_at, subject_count)?;
    probe_store
        .run(&run.id)?
        .ok_or_else(|| anyhow::anyhow!("recall probe run {} missing after completion", run.id))
}

pub fn collect_recall_probe_escalations(
    probe_store: &RecallProbeStore,
    run_id: &str,
) -> Result<Vec<RecallProbeEscalation>> {
    let mut grouped = HashMap::<(RecallProbeTargetKind, String), Vec<RecallProbeResult>>::new();
    for result in probe_store.results_for_run(run_id)? {
        grouped
            .entry((result.target_kind, result.target_id.clone()))
            .or_default()
            .push(result);
    }

    let mut escalations = Vec::new();
    for ((target_kind, target_id), results) in grouped {
        if results.iter().any(|result| result.matched) {
            continue;
        }
        let Some(summary) = probe_store.summary_for(target_kind, &target_id)? else {
            continue;
        };
        if summary.status != RecallProbeStatus::Unreachable
            || summary.consecutive_failures < RECALL_PROBE_PROPOSAL_FAILURE_THRESHOLD
            || summary.last_run_id != run_id
        {
            continue;
        }

        let mut flags = HashSet::new();
        for result in &results {
            for flag in &result.remediation_flags {
                flags.insert(*flag);
            }
        }
        escalations.push(RecallProbeEscalation {
            target_kind,
            target_id,
            consecutive_failures: summary.consecutive_failures,
            remediation_flags: flags.into_iter().collect(),
        });
    }

    Ok(escalations)
}

fn build_archive_probe_subjects(
    archive_store: &FileArchiveStore,
    vault_store: &FileArchiveStore,
    max_subjects: usize,
) -> Result<Vec<RecallProbeSubject>> {
    let mut docs = archive_store.list()?;
    docs.extend(vault_store.list()?);
    docs.sort_by_key(|doc| std::cmp::Reverse(doc.updated_at));

    Ok(docs
        .into_iter()
        .take(max_subjects)
        .map(|doc| RecallProbeSubject {
            target_kind: RecallProbeTargetKind::ArchiveEntry,
            target_id: doc.record_id,
            title: doc.title,
            aliases: Vec::new(),
            content: doc.content,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex;

    use chrono::Utc;
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};
    use symbiotic_context::graph::BfsGraphRetriever;
    use symbiotic_context::{ArchiveEntry, ArchiveProvider, AuditRecord, AuditSink};
    use symbiotic_memory::store::MemoryStore;
    use symbiotic_memory::types::{
        AllowedModels, Entity, EntityStatus, EntityType, Evidence, FactDisposition, Memory,
        MemorySpace, MemoryStatus, Sensitivity as MemorySensitivity,
    };
    use symbiotic_memory::{recall_probes::RecallProbeStatus, SqliteGraphStore, SqliteMemoryStore};

    #[derive(Default)]
    struct CollectingAudit {
        records: Mutex<Vec<AuditRecord>>,
    }

    impl AuditSink for CollectingAudit {
        fn record(&self, record: AuditRecord) -> Result<()> {
            self.records.lock().expect("audit").push(record);
            Ok(())
        }
    }

    struct ArchiveProviderBridge {
        archive: Arc<FileArchiveStore>,
        vault: Arc<FileArchiveStore>,
    }

    impl ArchiveProvider for ArchiveProviderBridge {
        fn list_entries(&self) -> Result<Vec<ArchiveEntry>> {
            let mut entries = Vec::new();
            for doc in self.archive.list()? {
                entries.push(map_doc(doc));
            }
            for doc in self.vault.list()? {
                entries.push(map_doc(doc));
            }
            Ok(entries)
        }
    }

    fn map_doc(doc: symbiotic_archive::ArchiveDocument) -> ArchiveEntry {
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
    }

    #[tokio::test]
    async fn recall_probe_batch_persists_reachable_documents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive =
            Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));
        let vault = Arc::new(FileArchiveStore::open(tmp.path().join("vault")).expect("vault"));

        let outcome = archive
            .store(StoreRequest {
                title_hint: Some("Rust Tokio Runtime".to_string()),
                content: "Tokio powers the async daemon runtime.".to_string(),
                source_url: None,
                tags: vec!["rust".to_string(), "tokio".to_string()],
                sensitivity: ArchiveSensitivity::Private,
                idempotency_key: "recall-probe-1".to_string(),
                firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
            })
            .expect("store");

        let provider = Arc::new(ArchiveProviderBridge {
            archive: archive.clone(),
            vault: vault.clone(),
        });
        let audit = Arc::new(CollectingAudit::default());
        let gateway = RecallGateway::new(provider, audit);
        let store = RecallProbeStore::open_in_memory().expect("probe store");

        let run = run_recall_probe_batch(&gateway, &archive, &vault, None, &store, 5, 10, 3)
            .expect("probe batch");
        assert_eq!(run.subject_count, 1);
        assert_eq!(run.matched_count, 1);

        let summary = store
            .summary_for(RecallProbeTargetKind::ArchiveEntry, &outcome.record_id)
            .expect("summary query")
            .expect("summary");
        assert_eq!(summary.status, RecallProbeStatus::Healthy);
    }

    #[tokio::test]
    async fn recall_probe_batch_can_target_graph_entities() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let archive =
            Arc::new(FileArchiveStore::open(tmp.path().join("archive")).expect("archive"));
        let vault = Arc::new(FileArchiveStore::open(tmp.path().join("vault")).expect("vault"));
        let db_path = tmp.path().join("memory.db");
        let memory_store = SqliteMemoryStore::open(&db_path).expect("memory store");
        memory_store.initialize().await.expect("initialize");

        let ts = Utc::now().to_rfc3339();
        let entity = Entity {
            id: "tokio".to_string(),
            entity_type: EntityType::Concept,
            name: "Tokio".to_string(),
            attributes: serde_json::json!({}),
            sensitivity: MemorySensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        };
        memory_store
            .create_entity(&entity)
            .await
            .expect("create entity");

        let memory = Memory {
            id: "memory-tokio".to_string(),
            entity_id: entity.id.clone(),
            fact: "Tokio powers the async runtime".to_string(),
            confidence: 0.9,
            disposition: FactDisposition::AutoStored,
            sensitivity: MemorySensitivity::Private,
            valid_from: ts.clone(),
            valid_to: None,
            status: MemoryStatus::Active,
            superseded_by: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
            fact_type: None,
            authored_by: None,
            supersedes: None,
            depends_on: Vec::new(),
            fsrs: None,
        };
        let evidence = Evidence {
            id: "evidence-tokio".to_string(),
            memory_id: Some(memory.id.clone()),
            relationship_id: None,
            entity_id: None,
            article_id: Some("article-tokio".to_string()),
            source_url: Some("https://example.com/tokio".to_string()),
            evidence_quote: Some("Tokio powers the async runtime".to_string()),
            observed_at: ts.clone(),
            created_at: ts.clone(),
        };
        memory_store
            .create_memory(&memory, &[evidence])
            .await
            .expect("create memory");

        let provider = Arc::new(ArchiveProviderBridge {
            archive: archive.clone(),
            vault: vault.clone(),
        });
        let audit = Arc::new(CollectingAudit::default());
        let mut gateway = RecallGateway::new(provider, audit);
        gateway.set_graph(Arc::new(BfsGraphRetriever::new(
            SqliteGraphStore::open(&db_path).expect("graph store"),
        )));

        let store = RecallProbeStore::open_in_memory().expect("probe store");
        let run = run_recall_probe_batch(
            &gateway,
            &archive,
            &vault,
            Some(db_path.as_path()),
            &store,
            5,
            10,
            3,
        )
        .expect("probe batch");
        assert!(run.subject_count >= 1);
        assert!(run.matched_count >= 1);

        let summary = store
            .summary_for(RecallProbeTargetKind::GraphEntity, &entity.id)
            .expect("summary query")
            .expect("summary");
        assert_eq!(summary.status, RecallProbeStatus::Healthy);
    }

    #[test]
    fn collect_recall_probe_escalations_surfaces_unreachable_targets() {
        let store = RecallProbeStore::open_in_memory().expect("store");
        store
            .start_run(&RecallProbeRun {
                id: "run-1".to_string(),
                started_at: 10,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count: 1,
                matched_count: 0,
            })
            .expect("start");

        for created_at in [11, 12, 13] {
            let mut result = RecallProbeResult {
                run_id: "run-1".to_string(),
                target_kind: RecallProbeTargetKind::ArchiveEntry,
                target_id: "entry-1".to_string(),
                query: format!("query-{created_at}"),
                matched: false,
                rank: None,
                retrieval_mode: "keyword".to_string(),
                top_item_ids: Vec::new(),
                remediation_flags: Vec::new(),
                created_at,
            };
            result.remediation_flags = derive_remediation_flags(&result);
            store.record_result(&result).expect("record");
        }
        store.finish_run("run-1", 14, 1).expect("finish");

        let escalations = collect_recall_probe_escalations(&store, "run-1").expect("escalations");
        assert_eq!(escalations.len(), 1);
        assert_eq!(
            escalations[0].target_kind,
            RecallProbeTargetKind::ArchiveEntry
        );
        assert_eq!(escalations[0].target_id, "entry-1");
        assert_eq!(
            escalations[0].consecutive_failures,
            RECALL_PROBE_PROPOSAL_FAILURE_THRESHOLD
        );
        assert!(escalations[0]
            .remediation_flags
            .contains(&RecallRemediationFlag::ManualReviewRequired));
    }
}
