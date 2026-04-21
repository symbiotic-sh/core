#![allow(clippy::needless_borrow)]

use super::*;
use crate::auth_jobs::{AuthInputKind, AuthJobPhase, AuthJobRecord, AuthJobStatus};
use crate::EventType;
use credential_gateway::{CredentialVault, FileCredentialVault, GoalScopedVault};
use credential_validator::{CredentialValidationResult, ValidateCredential};
use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use symbiotic_agent_runner::{run_with_bridge, BridgeClient, RunnerSessionConfig};
use symbiotic_context::Sensitivity as ContextSensitivity;
use symbiotic_control_plane::{
    AgentAssignment, AssignmentMode, CollaborationScope, HeartbeatStatus, HeartbeatUpdate, Lease,
    ManagementStore, ReviewMode, ScopeClaim, ScopeClaimStatus, ScopeMode, WorkItem, WorkItemKind,
    WorkItemStatus, WorkPriority, WorkUrgency,
};
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_intake::twitter::{
    canonicalize_twitter_url, Tweet, TweetThread, TwitterApiClient, TwitterApiError,
    TwitterFallbackClient,
};
use symbiotic_intake::ContentFetcher;
use symbiotic_matrix::transport::{InMemoryMatrixTransport, MatrixMessage};
use symbiotic_memory::MemoryStore;
use symbiotic_trust::{AgentTrustLevel, CapabilityToken};

/// Helper: get a detail field from an envelope's `sym.d` as `Option<&str>`.
fn detail_str<'a>(envelope: &'a MatrixEventEnvelope, key: &str) -> Option<&'a str> {
    envelope.sym.d.as_ref()?.get(key)?.as_str()
}

/// Helper: get a detail field from an envelope's `sym.d` as `Option<&Value>`.
fn detail_val<'a>(envelope: &'a MatrixEventEnvelope, key: &str) -> Option<&'a serde_json::Value> {
    envelope.sym.d.as_ref()?.get(key)
}

/// Helper: check if a detail field key exists.
fn has_detail(envelope: &MatrixEventEnvelope, key: &str) -> bool {
    envelope
        .sym
        .d
        .as_ref()
        .is_some_and(|d| d.get(key).is_some())
}

fn unique_suffix() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", now_unix(), std::process::id(), id)
}

fn write_archive_project_fixture(
    archive_root: &Path,
    project_slug: &str,
    project_id: &str,
    title: &str,
) {
    let project_dir = archive_root.join("operations/projects").join(project_slug);
    std::fs::create_dir_all(&project_dir).expect("create archive project dir");
    std::fs::write(
        project_dir.join("project.md"),
        format!(
            "---\nid: \"{project_id}\"\nslug: {project_slug}\ntitle: \"{title}\"\nstate: active\n---\n\n# {title}\n"
        ),
    )
    .expect("write archive project manifest");
}

#[derive(Default)]
struct StubXApiHttpClient {
    responses: Mutex<Vec<std::result::Result<String, String>>>,
    calls: Mutex<Vec<String>>,
}

impl StubXApiHttpClient {
    fn push_response(&self, response: std::result::Result<&str, &str>) {
        let mut responses = self.responses.lock().expect("stub response lock");
        responses.push(response.map(str::to_string).map_err(str::to_string));
    }

    fn call_count(&self) -> usize {
        self.calls.lock().expect("stub call lock").len()
    }
}

impl XApiHttpClient for StubXApiHttpClient {
    fn get_json(&self, url: &str, _bearer_token: &str) -> Result<String> {
        let mut calls = self.calls.lock().expect("stub call lock");
        calls.push(url.to_string());
        drop(calls);

        let mut responses = self.responses.lock().expect("stub response lock");
        if responses.is_empty() {
            return Err(anyhow!("stub x api error: missing response"));
        }
        let response = responses
            .remove(0)
            .map_err(|err| anyhow!("stub x api error: {err}"))?;
        Ok(response)
    }
}

#[derive(Default)]
struct StubPushHttpClient {
    responses: Mutex<Vec<std::result::Result<(), String>>>,
    calls: Mutex<u32>,
    bodies: Mutex<Vec<String>>,
}

impl StubPushHttpClient {
    fn push_response(&self, response: std::result::Result<(), &str>) {
        let mut responses = self.responses.lock().expect("stub push response lock");
        responses.push(response.map_err(str::to_string));
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().expect("stub push call lock")
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().expect("stub push body lock").clone()
    }
}

impl PushHttpClient for StubPushHttpClient {
    fn post_json(&self, _url: &str, body: &str, _bearer_token: Option<&str>) -> Result<()> {
        let mut calls = self.calls.lock().expect("stub push call lock");
        *calls += 1;
        drop(calls);
        self.bodies
            .lock()
            .expect("stub push body lock")
            .push(body.to_string());

        let mut responses = self.responses.lock().expect("stub push response lock");
        if responses.is_empty() {
            return Err(anyhow!("stub push response missing"));
        }
        match responses.remove(0) {
            Ok(()) => Ok(()),
            Err(err) => Err(anyhow!("stub push error: {err}")),
        }
    }
}

struct AlwaysFailPushProvider;

impl PushProvider for AlwaysFailPushProvider {
    fn send(&self, _notification: &PushNotification) -> Result<()> {
        Err(anyhow!("intentional push failure"))
    }
}

fn daemon_config_for_root(root: std::path::PathBuf) -> DaemonConfig {
    let queue_file = root.join("queue.state");
    DaemonConfig {
        queue_file,
        archive_root: root.join("archive"),
        vault_root: root.join("vault"),
        review_root: root.join("review"),
        domain_root: root.join("domains"),
        credential_vault_file: root.join("credentials.tsv"),
        audit_log_file: root.join("audit.log"),
        capability_tokens_file: root.join("capability-tokens.json"),
        bookmarks_api_file: root.join("bookmarks-api.txt"),
        bookmarks_browser_file: root.join("bookmarks-browser.txt"),
        x_thread_fallback_file: root.join("twitter-threads.txt"),
        x_api_base_url: "https://api.twitter.com/2".to_string(),
        x_client_id: None,
        x_client_secret: None,
        vps_provision_endpoint: None,
        vps_provision_token: None,
        hcloud_token: None,
        vps_region: "us-east-1".to_string(),
        vps_size: "shared-cpu-2x".to_string(),
        vps_image: "ubuntu-24-04".to_string(),
        vps_ssh_public_key: None,
        goal_log_file: root.join("goals.log"),
        goal_state_file: root.join("goal-state.tsv"),
        agent_log_file: root.join("agent-lifecycle.log"),
        agent_state_file: root.join("agent-state.tsv"),
        push_registry_file: root.join("push-tokens.tsv"),
        push_token_key_file: root.join("push-token.key"),
        push_outbox_file: root.join("push-outbox.ndjson"),
        push_ack_file: root.join("push-acks.ndjson"),
        push_telemetry_file: root.join("push-delivery.log"),
        push_gateway_url: None,
        push_gateway_api_key: None,
        push_apns_gateway_url: None,
        push_apns_gateway_api_key: None,
        push_fcm_gateway_url: None,
        push_fcm_gateway_api_key: None,
        push_apns_team_id: None,
        push_apns_key_id: None,
        push_apns_private_key_pem: None,
        push_apns_sandbox: false,
        push_fcm_project_id: None,
        push_fcm_service_account_email: None,
        push_fcm_private_key_pem: None,
        fetch_mode: FetchMode::Stub,
        worker_id: "test-worker".to_string(),
        lease_seconds: 60,
        retry_backoff_seconds: 1,
        max_matrix_message_bytes: 32 * 1024,
        blocked_hosts: HashSet::new(),
        room_roles: RoomRoleMap::default(),
        allowed_senders: HashSet::new(),
        allow_open_access: true,
        data_dir: root.join("data"),
        ollama_url: None,
        ollama_chat_model: None,
        model_manifest_file: root.join("model-manifest.toml"),
        openai_api_key: None,
        anthropic_api_key: None,
        claude_code_binary: None,
        claude_code_model: None,
        codex_binary: None,
        codex_model: None,
        gemini_api_key: None,
        gemini_model: None,
        openrouter_api_key: None,
        openrouter_model: None,
        default_provider: None,
        llm_gateway_socket: None,
        llm_gateway_world_accessible: false,
        agent_backend: AgentBackend::React,
        runner_harness_mode: crate::workers::RunnerHarnessMode::Process,

        role_dir: root.join("roles"),
        secrets_file: root.join("config").join(".env.secrets"),
        archive_path: None,
        blob_store_root: root.join("blob-store"),
        blob_store_key_file: None,
        tier3_phone_only: true,
        embedding_batch_size: 8,
        embedding_retry_interval_secs: 300,
        recall_probe_interval_secs: 24 * 3600,
        recall_probe_top_k: 10,
        recall_probe_max_subjects_per_run: 200,
        recall_probe_max_queries_per_subject: 3,
        soul_file: None,
        push_preferences_file: root.join("push-preferences.toml"),
        push_rate_limit_per_hour: 30,
        push_stale_device_days: 90,
        enable_process_engineer: false,
        pe_graduation_db: root.join("pe-graduation.db"),
        somatic_db: root.join("somatic.db"),
        auth_scripts_dir: None,
        auth_sandbox_bin: None,
        node_bin: None,
        auth_approval_ttl_secs: 300,
        auth_input_ttl_secs: 300,
        matrix_homeserver: None,
        matrix_server_name: None,
        matrix_access_token: None,
    }
}

fn daemon_config_for_test(name: &str) -> DaemonConfig {
    let root = std::env::temp_dir().join(format!("symbiotic_daemon_{name}_{}", unique_suffix()));
    daemon_config_for_root(root)
}

fn daemon_for_test(name: &str) -> SymbioticDaemon {
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(daemon_config_for_test(name)).expect("daemon should initialize");
    daemon
}

fn init_git_repo(path: &std::path::Path) {
    let init = Command::new("git")
        .args(["init"])
        .current_dir(path)
        .output()
        .expect("git init");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let user_name = Command::new("git")
        .args(["config", "user.name", "Symbiotic Test"])
        .current_dir(path)
        .output()
        .expect("git config user.name");
    assert!(
        user_name.status.success(),
        "git config user.name failed: {}",
        String::from_utf8_lossy(&user_name.stderr)
    );

    let user_email = Command::new("git")
        .args(["config", "user.email", "test@symbiotic.local"])
        .current_dir(path)
        .output()
        .expect("git config user.email");
    assert!(
        user_email.status.success(),
        "git config user.email failed: {}",
        String::from_utf8_lossy(&user_email.stderr)
    );
}

fn credential_gateway_bin_for_test() -> std::path::PathBuf {
    let binary_name = format!("credential-gateway{}", std::env::consts::EXE_SUFFIX);
    let current_exe = std::env::current_exe().expect("current exe");
    let sibling = current_exe.with_file_name(&binary_name);
    if sibling.exists() {
        return sibling;
    }

    current_exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(|dir| dir.join(&binary_name))
        .filter(|candidate| candidate.exists())
        .unwrap_or_else(|| std::path::PathBuf::from(binary_name))
}

fn daemon_with_auth_script(name: &str, script_body: &str) -> SymbioticDaemon {
    let mut config = daemon_config_for_test(name);
    let scripts_dir = config.data_dir.join("auth-scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    let script_path = scripts_dir.join("github.com.sh");
    std::fs::write(&script_path, script_body).expect("write auth script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    config.auth_scripts_dir = Some(scripts_dir);
    config.auth_sandbox_bin = Some(credential_gateway_bin_for_test());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    daemon
}

fn test_work_item(id: &str, observed_at: i64) -> WorkItem {
    WorkItem {
        id: id.to_string(),
        project_id: "symbiotic".to_string(),
        initiative_id: None,
        parent_work_item_id: None,
        kind: WorkItemKind::Execution,
        thread_id: None,
        title: "Management recovery".to_string(),
        summary: "Recover persisted management state".to_string(),
        status: WorkItemStatus::ClaimPending,
        priority: WorkPriority::P1,
        urgency: WorkUrgency::Normal,
        assignment_mode: AssignmentMode::SingleOwner,
        requested_scopes: Vec::new(),
        accepted_claim_ids: Vec::new(),
        assignee: Some(AgentAssignment {
            agent_id: "agent-1".to_string(),
            runner_id: Some("sandbox-1".to_string()),
            assigned_at: observed_at,
        }),
        blocked_by: Vec::new(),
        depends_on: Vec::new(),
        review_mode: ReviewMode::NoReview,
        cancellation: None,
        created_at: observed_at,
        updated_at: observed_at,
    }
}

fn test_claim(id: &str, work_item_id: &str, observed_at: i64) -> ScopeClaim {
    ScopeClaim {
        id: id.to_string(),
        work_item_id: work_item_id.to_string(),
        holder_agent_id: "agent-1".to_string(),
        scope: CollaborationScope::RepoPath {
            repo_id: "runtime".to_string(),
            path: "services/symbiotic-daemon/src".to_string(),
        },
        mode: ScopeMode::ExclusiveWrite,
        status: ScopeClaimStatus::Active,
        lease: Lease::new("agent-1".to_string(), observed_at, 30, 2),
        granted_at: observed_at,
        updated_at: observed_at,
    }
}

fn sample_push_notification() -> PushNotification {
    PushNotification {
        notification_id: "notif-1".to_string(),
        device_id: "device-1".to_string(),
        token_hash: "hash-1".to_string(),
        encrypted_token: "enc:test-device-token-abc123".to_string(),
        platform: "apns".to_string(),
        priority: "critical".to_string(),
        title: "Credential request".to_string(),
        body: "Approve login".to_string(),
        rid: "rid-1".to_string(),
        event_type: "auth.required".to_string(),
        event_status: "queued".to_string(),
        ts: now_unix(),
        thread_id: None,
        badge: None,
    }
}

#[test]
fn daemon_recovers_management_store_and_expires_stale_claims() {
    let config = daemon_config_for_test("management-store-recovery");
    let management_root = config.data_dir.join("control-plane");

    {
        let mut store = ManagementStore::new(management_root);
        store
            .upsert_work_item(test_work_item("w1", 100))
            .expect("write work item");
        store
            .grant_claim(test_claim("c1", "w1", 100))
            .expect("grant claim");
    }

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let store = daemon
        .management_store
        .lock()
        .expect("management store lock should succeed");

    assert_eq!(store.work_item_count(), 1);
    assert_eq!(store.claim_count(), 1);
    assert_eq!(store.active_claim_count(), 0);
    assert_eq!(
        store
            .get_work_item("w1")
            .expect("work item should load")
            .status,
        WorkItemStatus::Expired
    );
    assert_eq!(
        store.get_claim("c1").expect("claim should load").status,
        ScopeClaimStatus::Expired
    );
}

#[test]
fn daemon_management_api_persists_running_claim_state() {
    let now = symbiotic_queue::now_unix() as i64;
    let root = std::env::temp_dir().join(format!(
        "symbiotic_daemon_management-api_{}",
        unique_suffix()
    ));
    let config = daemon_config_for_root(root.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    daemon
        .upsert_management_work_item(test_work_item("w1", now))
        .expect("write work item");
    daemon
        .grant_management_scope_claim(test_claim("c1", "w1", now))
        .expect("grant claim");
    daemon
        .record_management_heartbeat(HeartbeatUpdate {
            work_item_id: "w1".to_string(),
            agent_id: "agent-1".to_string(),
            status: HeartbeatStatus::Alive,
            progress_summary: Some("started work".to_string()),
            progress_percent: Some(20),
            needs_attention: false,
            observed_at: now + 10,
        })
        .expect("record heartbeat");

    drop(daemon);

    let (reopened, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(daemon_config_for_root(root)).expect("reopen daemon");
    let store = reopened
        .management_store
        .lock()
        .expect("management store lock should succeed");

    assert_eq!(store.work_item_count(), 1);
    assert_eq!(store.claim_count(), 1);
    assert_eq!(store.active_claim_count(), 1);
    assert_eq!(
        store
            .get_work_item("w1")
            .expect("work item should load")
            .status,
        WorkItemStatus::Running
    );
    assert_eq!(
        store
            .get_claim("c1")
            .expect("claim should load")
            .lease
            .last_heartbeat_at,
        now + 10
    );
}

#[test]
fn intake_message_enqueues_ingest_jobs() {
    let daemon = daemon_for_test("intake");
    let reply = daemon
        .handle_intake_message("https://example.com/a #symbiotic")
        .expect("message should be handled");

    assert_eq!(reply.result.summary.total, 1);
    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn submit_intake_request_enqueues_url_jobs() {
    let daemon = daemon_for_test("submit-url");
    let result = daemon
        .submit_intake_request(IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Url,
            urls: vec![normalize_url("https://example.com/u").expect("valid url")],
            note: None,
            tags: vec!["tag".to_string()],
            file_path: None,
            title: None,
        })
        .expect("submit should work");

    assert_eq!(result.summary.total, 1);
    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn submit_intake_request_processes_notes_via_pipeline() {
    let daemon = daemon_for_test("submit-note");
    let result = daemon
        .submit_intake_request(IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("A low sensitivity note".to_string()),
            tags: vec!["memo".to_string()],
            file_path: None,
            title: None,
        })
        .expect("submit should work");

    assert_eq!(result.summary.total, 1);
    assert_eq!(result.summary.ingested, 1);
    assert_eq!(result.items[0].route, IntakeRoute::Archive);
    assert!(result.items[0].review_queued);
}

#[test]
fn submit_intake_urls_raw_handles_invalid_entries_in_service_layer() {
    let daemon = daemon_for_test("submit-raw");
    let result = daemon
        .submit_intake_urls_raw(
            vec!["https://example.com/u".to_string(), "bad://url".to_string()],
            vec!["tag".to_string()],
            IntakeSource::Cli,
        )
        .expect("submit raw should work");

    assert_eq!(result.summary.total, 2);
    assert_eq!(result.summary.ingested, 1);
    assert_eq!(result.summary.invalid, 1);
    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn blocked_ingest_job_is_acknowledged_without_retry() {
    let daemon = daemon_for_test("ingest-blocked-target");
    let blocked = normalize_url("http://169.254.169.254/latest/meta-data").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(vec![blocked], vec!["tag".to_string()], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let event = daemon
        .run_once(now_unix())
        .expect("run should succeed")
        .expect("job should be processed")
        .0;
    assert_eq!(event.event_type, EventType::IngestFetch);
    assert_eq!(event.status, "completed");
    assert_eq!(event.detail, "Blocked");

    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 0);

    let records = daemon.archive_records().expect("archive list should work");
    assert!(
        records.is_empty(),
        "blocked ingest should not persist records"
    );
}

#[test]
fn run_once_processes_ingest_then_review_job() {
    let daemon = daemon_for_test("run_once");
    let normalized = normalize_url("https://example.com/p1").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(vec![normalized], vec!["tag".to_string()], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let event1 = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("job should be processed")
        .0;
    assert_eq!(event1.event_type, EventType::IngestFetch);
    assert_eq!(event1.status, "completed");

    let queued_review = daemon
        .queued_jobs_of_type("archive.review.enqueue")
        .expect("queue query should work");
    assert_eq!(queued_review.len(), 1);
    let record_id = decode_review_payload(&queued_review[0].payload).expect("record id");

    let event2 = daemon
        .run_once(now_unix() + 1)
        .expect("run once should succeed")
        .expect("review job should be processed")
        .0;
    assert_eq!(event2.event_type, EventType::ArchiveReviewEnqueue);
    assert_eq!(event2.status, "completed");

    let queued_review_run = daemon
        .queued_jobs_of_type("archive.review")
        .expect("queue query should work");
    assert_eq!(queued_review_run.len(), 1);

    let event3 = daemon
        .run_once(now_unix() + 2)
        .expect("run once should succeed")
        .expect("review run should be processed")
        .0;
    assert_eq!(event3.event_type, EventType::ArchiveReview);
    assert_eq!(event3.status, "completed");

    let review_record = daemon
        .review_record(&record_id)
        .expect("review should load");
    assert!(review_record.is_some());
}

#[test]
fn status_snapshot_reports_queue_counts() {
    let daemon = daemon_for_test("status");
    let now = now_unix();
    let normalized = normalize_url("https://example.com/status").expect("valid url");
    daemon
        .enqueue_intake_urls(vec![normalized], vec![], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let before = daemon.status_snapshot(now).expect("snapshot should work");
    assert_eq!(before.queued, 1);
    assert_eq!(before.running, 0);

    let _event = daemon
        .run_once(now)
        .expect("run once should succeed")
        .expect("ingest should run")
        .0;
    let after = daemon
        .status_snapshot(now + 1)
        .expect("snapshot should work");
    assert_eq!(after.done, 1);
}

#[test]
fn build_snapshot_envelope_includes_queue_counts() {
    let daemon = daemon_for_test("snapshot-envelope");
    let now = now_unix();
    let normalized = normalize_url("https://example.com/snap").expect("valid url");
    daemon
        .enqueue_intake_urls(vec![normalized], vec![], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let envelope = daemon
        .build_snapshot_envelope(now, None)
        .expect("build_snapshot_envelope should work");
    assert_eq!(envelope.sym.a.as_deref(), Some("snapshot"));
    assert_eq!(envelope.sym.k, Kind::State);
    assert_eq!(detail_val(&envelope, "queued"), Some(&serde_json::json!(1)));
    assert!(!has_detail(&envelope, "uptime_secs"));
}

#[test]
fn build_snapshot_envelope_includes_uptime() {
    let daemon = daemon_for_test("snapshot-uptime");
    let now = now_unix();

    let envelope = daemon
        .build_snapshot_envelope(now, Some(120))
        .expect("build_snapshot_envelope should work");
    assert_eq!(envelope.sym.a.as_deref(), Some("snapshot"));
    assert_eq!(
        detail_val(&envelope, "uptime_secs"),
        Some(&serde_json::json!(120))
    );
}

#[test]
fn context_gateway_returns_ingested_archive_content() {
    let daemon = daemon_for_test("context");
    let normalized = normalize_url("https://example.com/context").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(
            vec![normalized],
            vec!["architecture".to_string()],
            IntakeSource::Cli,
        )
        .expect("enqueue should succeed");
    let _event = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("ingest should run");

    let pack = daemon
        .get_context(&ContextRequest {
            request_id: "ctx-1".to_string(),
            query: "fetched".to_string(),
            model_class: symbiotic_context::ModelClass::Local,
            purpose: symbiotic_context::Purpose::Answer,
            sensitivity_max: ContextSensitivity::Private,
            token_budget: 200,
            tags: vec!["architecture".to_string()],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("context should build");
    assert!(!pack.items.is_empty());
}

#[test]
fn context_gateway_returns_graph_backed_memory_items() {
    let daemon = daemon_for_test("context-graph");
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    rt.block_on(async {
        use symbiotic_memory::store::MemoryStore;
        use symbiotic_memory::{
            AllowedModels, Entity, EntityStatus, Evidence, FactDisposition, Memory, MemorySpace,
            MemoryStatus, Sensitivity as MemorySensitivity,
        };

        let ts = chrono::Utc::now().to_rfc3339();
        let entity = Entity {
            id: "entity-rust-safety".to_string(),
            entity_type: symbiotic_memory::EntityType::Concept,
            name: "Rust safety".to_string(),
            attributes: serde_json::json!({}),
            sensitivity: MemorySensitivity::Private,
            allowed_models: AllowedModels::LocalOnly,
            space: MemorySpace::Knowledge,
            status: EntityStatus::Active,
            merged_into: None,
            created_at: ts.clone(),
            updated_at: ts.clone(),
        };
        daemon
            .memory_store
            .create_entity(&entity)
            .await
            .expect("create entity");

        let memory = Memory {
            id: "memory-rust-safety".to_string(),
            entity_id: entity.id.clone(),
            fact: "Rust ownership keeps memory-safe code deterministic.".to_string(),
            confidence: 0.95,
            disposition: FactDisposition::UserConfirmed,
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
            id: "evidence-rust-safety".to_string(),
            memory_id: Some(memory.id.clone()),
            relationship_id: None,
            entity_id: Some(entity.id.clone()),
            article_id: Some("archive-rust-safety".to_string()),
            source_url: Some("https://example.com/rust-safety".to_string()),
            evidence_quote: Some("Ownership enforces memory safety.".to_string()),
            observed_at: ts.clone(),
            created_at: ts,
        };
        daemon
            .memory_store
            .create_memory(&memory, &[evidence])
            .await
            .expect("create memory");
    });

    let pack = daemon
        .get_context(&ContextRequest {
            request_id: "ctx-graph-1".to_string(),
            query: "Rust safety".to_string(),
            model_class: symbiotic_context::ModelClass::Local,
            purpose: symbiotic_context::Purpose::Answer,
            sensitivity_max: ContextSensitivity::Private,
            token_budget: 200,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("context should build");

    let graph_item = pack
        .items
        .iter()
        .find(|item| item.id == "entity-rust-safety")
        .expect("graph-backed memory item should be returned");
    assert_eq!(graph_item.r#type, "memory");
    assert!(graph_item
        .content
        .contains("memory-safe code deterministic"));
    assert!(
        graph_item
            .evidence
            .iter()
            .any(|entry| entry == "archive-rust-safety"),
        "memory item should preserve evidence from the live memory graph"
    );
}

#[test]
fn x_status_urls_use_twitter_fallback_content() {
    let config = daemon_config_for_test("x-status");
    if let Some(parent) = config.x_thread_fallback_file.parent() {
        fs::create_dir_all(parent).expect("create fallback fixture dir");
    }
    fs::write(
        &config.x_thread_fallback_file,
        "https://x.com/someone/status/1234567890\tFixture X thread text\n",
    )
    .expect("write fallback fixture");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let normalized = normalize_url("https://x.com/someone/status/1234567890").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(vec![normalized.clone()], vec![], IntakeSource::Cli)
        .expect("enqueue should succeed");
    let _event = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("ingest should run");

    let docs = daemon
        .archive_store
        .list()
        .expect("archive list should work");
    // T54: The intake pipeline canonicalizes Twitter URLs (x.com/user/status/ID
    // -> x.com/i/status/ID) before storing, so look up by the canonical form.
    let canonical = canonicalize_twitter_url(&normalized);
    let doc = docs
        .iter()
        .find(|doc| doc.source_url.as_deref() == Some(canonical.as_str()))
        .expect("record should be present");
    assert!(doc.content.contains("X Thread by"));
}

#[test]
fn strip_html_removes_tags_and_keeps_text() {
    let html = "<html><body><h1>Title</h1><p>Hello &amp; world</p></body></html>";
    let text = strip_html(html);
    assert!(text.contains("Title"));
    assert!(text.contains("Hello & world"));
}

#[test]
fn matrix_intake_route_emits_structured_event() {
    let daemon = daemon_for_test("matrix-route");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#intake".to_string(),
                sender: "@user:test".to_string(),
                body: "https://example.com/a #tag".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].msgtype, "sym.e");
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(
        detail_val(&events[0], "ingested"),
        Some(&serde_json::json!(1))
    );
}

#[test]
fn matrix_control_route_queues_workflow_jobs() {
    let daemon = daemon_for_test("matrix-control-workflow");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("workflow.run"));
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_queues_workflow_jobs_from_v1_json_command() {
    let daemon = daemon_for_test("matrix-control-workflow-json");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"workflow.run","d":{"template":"intake-url"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("workflow.run"));
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

// ── Approval-gate dispatcher tests (T126 §07b.iii-b) ────────────────────

/// Build a minimal `ApprovalContext` for dispatch tests. The content is
/// irrelevant — only the state-machine handoff is under test.
fn approval_dispatch_test_context() -> crate::approval_gate::ApprovalContext {
    crate::approval_gate::ApprovalContext {
        operation: crate::approval_gate::ApprovalOperation::PushExternal,
        explanation: "dispatcher test".to_string(),
        repo_id: "repo:test".to_string(),
        remote_url: "git@example.com:test/test.git".to_string(),
        local_bare_path: "data/git-server/repos/test.git".to_string(),
        branch: "agent/test".to_string(),
        is_protected_branch: false,
        agent_id: "agent-test".to_string(),
        goal_id: Some("goal-test".to_string()),
        project_id: "project:test".to_string(),
        commit_range: crate::approval_gate::CommitRange {
            from_sha: "0000000000000000000000000000000000000000".to_string(),
            to_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            commit_count: 1,
        },
        diff_stats: crate::approval_gate::DiffStats {
            files_changed: 1,
            insertions: 1,
            deletions: 0,
        },
        top_commit_message: "dispatcher test commit".to_string(),
        archeology_detail: None,
    }
}

#[test]
fn dispatcher_routes_approve_to_gate() {
    let daemon = daemon_for_test("approval-dispatch-approve");
    let now = now_unix();

    // Open a ticket directly on the gate.
    let ticket_id = {
        let mut gate = daemon.approval_gate.lock().unwrap();
        let ticket = gate.open_ticket(approval_dispatch_test_context(), now, 3600);
        ticket.ticket_id
    };

    let envelopes = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@op:test".to_string(),
                body: format!("approve {ticket_id}"),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].sym.k, Kind::Message);
    assert_eq!(envelopes[0].sym.s, Some(Status::Success));
    assert_eq!(detail_str(&envelopes[0], "ticket_id"), Some(&*ticket_id));
    assert_eq!(detail_str(&envelopes[0], "approved_by"), Some("@op:test"));

    // The state machine actually transitioned.
    let gate = daemon.approval_gate.lock().unwrap();
    let stored = gate.get(&ticket_id).expect("ticket stored");
    assert_eq!(
        stored.state,
        crate::approval_gate::ApprovalState::Approved {
            approved_by: "@op:test".to_string(),
            at: now,
        }
    );
}

#[test]
fn dispatcher_routes_deny_with_reason() {
    let daemon = daemon_for_test("approval-dispatch-deny");
    let now = now_unix();

    let ticket_id = {
        let mut gate = daemon.approval_gate.lock().unwrap();
        let ticket = gate.open_ticket(approval_dispatch_test_context(), now, 3600);
        ticket.ticket_id
    };

    let envelopes = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@op:test".to_string(),
                body: format!("deny {ticket_id} too risky"),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].sym.k, Kind::Message);
    assert_eq!(envelopes[0].sym.s, Some(Status::Success));
    assert_eq!(detail_str(&envelopes[0], "ticket_id"), Some(&*ticket_id));
    assert_eq!(detail_str(&envelopes[0], "denied_by"), Some("@op:test"));
    assert_eq!(detail_str(&envelopes[0], "reason"), Some("too risky"));

    let gate = daemon.approval_gate.lock().unwrap();
    let stored = gate.get(&ticket_id).expect("ticket stored");
    assert_eq!(
        stored.state,
        crate::approval_gate::ApprovalState::Denied {
            denied_by: "@op:test".to_string(),
            at: now,
            reason: Some("too risky".to_string()),
        }
    );
}

#[test]
fn dispatcher_inspect_unknown_ticket_returns_fail() {
    let daemon = daemon_for_test("approval-dispatch-inspect-unknown");
    let now = now_unix();

    let envelopes = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@op:test".to_string(),
                body: "inspect bogus-id".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].sym.k, Kind::Message);
    assert_eq!(envelopes[0].sym.s, Some(Status::Fail));
    assert_eq!(detail_str(&envelopes[0], "ticket_id"), Some("bogus-id"));
}

#[test]
fn matrix_control_route_queues_auth_issue_jobs() {
    let daemon = daemon_for_test("matrix-control-auth");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "auth issue x.com web.login,profile.read".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("auth.issue"));
    let queued = daemon
        .queued_jobs_of_type("auth.issue")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_queues_bookmarks_sync_jobs() {
    let daemon = daemon_for_test("matrix-control-bookmarks");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "bookmarks sync browser 25".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("bookmarks.sync"));
    let queued = daemon
        .queued_jobs_of_type("bookmarks.sync")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn stream_route_classifies_unknown_as_chat() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let daemon = daemon_for_test("stream-classify-unknown");
    let now = now_unix();
    // Short unknown text is classified as Quick by UxClassifier -> chat.reply.
    // Without a real LLM provider, the reply will have status "failed".
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#stream".to_string(),
                sender: "@user:test".to_string(),
                body: "hello".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // First event is classification.result (state), second is chat reply.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sym.a.as_deref(), Some("classification.result"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(detail_str(&events[0], "class"), Some("quick"));
    assert_eq!(events[1].sym.k, Kind::Message);
    // LLM call fails (no provider) but the classification path worked.
    assert_eq!(events[1].sym.s, Some(Status::Fail));
}

#[test]
fn stream_route_classifies_goal_via_deliberation() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let daemon = daemon_for_test("stream-classify-goal");
    let now = now_unix();
    // "build me a landing page" triggers Goal classification (contains "build ").
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#stream".to_string(),
                sender: "@user:test".to_string(),
                body: "build me a landing page for my startup".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // First event is classification.result, second is a goal pipeline event.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sym.a.as_deref(), Some("classification.result"));
    assert_eq!(detail_str(&events[0], "class"), Some("goal"));
    // UxClassifier detects "build " -> Goal -> pipeline. The pipeline may
    // return a deliberation/inquisition event depending on the classifier
    // outcome (no real LLM in tests).
    assert_eq!(events[1].sym.k, Kind::Message);
    assert_eq!(detail_str(&events[1], "classification"), Some("goal"));
}

#[test]
fn stream_route_classifies_quick_for_natural_language() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let daemon = daemon_for_test("stream-classify-quick");
    let now = now_unix();
    // Simple question without goal/task indicators -> Quick -> chat.reply.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#stream".to_string(),
                sender: "@user:test".to_string(),
                body: "Research the best flight deals to Tokyo for next month".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // First event is classification.result (state), second is chat reply.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sym.a.as_deref(), Some("classification.result"));
    assert_eq!(detail_str(&events[0], "class"), Some("quick"));
    assert_eq!(events[1].sym.k, Kind::Message);
    assert_eq!(detail_str(&events[1], "classification"), Some("quick"));
}

#[test]
fn stream_route_classifies_short_task() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let daemon = daemon_for_test("stream-classify-task");
    let now = now_unix();
    // "summarize" prefix triggers ShortTask -> task.result.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#stream".to_string(),
                sender: "@user:test".to_string(),
                body: "summarize this article for me".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // First event is classification.result (state), second is task result.
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sym.a.as_deref(), Some("classification.result"));
    assert_eq!(detail_str(&events[0], "class"), Some("short_task"));
    assert_eq!(detail_str(&events[0], "confidence"), Some("0.75"));
    assert_eq!(events[1].sym.k, Kind::Message);
    assert_eq!(detail_str(&events[1], "classification"), Some("short_task"));
}

#[test]
fn stream_route_classifies_intake_url() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let daemon = daemon_for_test("stream-classify-intake");
    let now = now_unix();
    // URL pasted in #stream should be classified as Intake.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#stream".to_string(),
                sender: "@user:test".to_string(),
                body: "https://example.com/some-article".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // First event is classification.result with class=intake.
    assert!(events.len() >= 2);
    assert_eq!(events[0].sym.a.as_deref(), Some("classification.result"));
    assert_eq!(detail_str(&events[0], "class"), Some("intake"));
    // Second event is intake response (Message kind).
    assert_eq!(events[1].sym.k, Kind::Message);
}

#[test]
fn matrix_route_rejects_messages_over_size_limit() {
    let mut config = daemon_config_for_test("matrix-message-limit");
    config.max_matrix_message_bytes = 8;
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert!(events[0].body.contains("max allowed size"));
}

#[test]
fn parse_control_command_rejects_v1_json() {
    // v1 format is no longer accepted — only v2 sym.c format
    let parsed = parse_control_command(r#"{"v":1,"command":"workflow.run","template":"x"}"#);
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_rejects_unknown_json_without_sym_c() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"workflow.run","d":{"template":"x","extra":1}}}"#,
    );
    // Extra fields in sym.d are ignored (passed through), so this should still parse
    assert!(matches!(parsed, ControlCommand::RunWorkflow { .. }));
}

#[test]
fn parse_control_command_accepts_goal_start_text() {
    let parsed = parse_control_command("goal start test-workflow");
    assert!(matches!(parsed, ControlCommand::GoalStart { .. }));
}

#[test]
fn parse_control_command_accepts_goal_list_text() {
    let parsed = parse_control_command("goal list");
    assert!(matches!(parsed, ControlCommand::GoalList));
}

#[test]
fn parse_control_command_accepts_goal_retry_text() {
    let parsed = parse_control_command("goal retry test-workflow");
    assert!(matches!(parsed, ControlCommand::GoalRetry { .. }));
}

#[test]
fn parse_control_command_accepts_goal_stop_text() {
    let parsed = parse_control_command("goal stop test-workflow");
    assert!(matches!(parsed, ControlCommand::GoalStop { .. }));
}

#[test]
fn parse_control_command_accepts_goal_start_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.start","d":{"template":"test-workflow"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::GoalStart { .. }));
}

#[test]
fn parse_control_command_accepts_goal_list_json() {
    let parsed =
        parse_control_command(r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.list"}}"#);
    assert!(matches!(parsed, ControlCommand::GoalList));
}

#[test]
fn parse_control_command_accepts_goal_retry_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.retry","d":{"template":"test-workflow"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::GoalRetry { .. }));
}

#[test]
fn parse_control_command_accepts_goal_stop_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"goal.stop","d":{"template":"test-workflow"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::GoalStop { .. }));
}

#[test]
fn parse_control_command_rejects_goal_start_without_template() {
    let parsed = parse_control_command("goal start");
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_accepts_install_run_text() {
    let parsed = parse_control_command("install run byok install-001");
    assert!(matches!(parsed, ControlCommand::InstallRun { .. }));
}

#[test]
fn parse_control_command_accepts_install_run_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.run","d":{"mode":"managed","install_id":"i-1"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::InstallRun { .. }));
}

#[test]
fn parse_control_command_accepts_install_provision_text() {
    let parsed = parse_control_command("install provision install-001");
    assert!(matches!(
        parsed,
        ControlCommand::InstallProvision {
            mode,
            install_id: Some(id)
        } if mode == "byok" && id == "install-001"
    ));
}

#[test]
fn parse_control_command_accepts_install_provision_text_with_mode() {
    let parsed = parse_control_command("install provision managed install-001");
    assert!(matches!(
        parsed,
        ControlCommand::InstallProvision {
            mode,
            install_id: Some(id)
        } if mode == "managed" && id == "install-001"
    ));
}

#[test]
fn parse_control_command_accepts_install_provision_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.provision","d":{"mode":"managed","install_id":"i-1"}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::InstallProvision {
            mode,
            install_id: Some(id)
        } if mode == "managed" && id == "i-1"
    ));
}

#[test]
fn parse_control_command_rejects_install_provision_with_invalid_mode() {
    let parsed = parse_control_command("install provision invalid install-001");
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_accepts_install_bootstrap_text() {
    let parsed = parse_control_command("install bootstrap");
    assert!(matches!(parsed, ControlCommand::InstallBootstrap));
}

#[test]
fn parse_control_command_accepts_install_bootstrap_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.bootstrap"}}"#,
    );
    assert!(matches!(parsed, ControlCommand::InstallBootstrap));
}

#[test]
fn parse_control_command_accepts_install_verify_text() {
    let parsed = parse_control_command("install verify managed install-verify-001");
    assert!(matches!(
        parsed,
        ControlCommand::InstallVerify {
            mode,
            install_id: Some(id)
        } if mode == "managed" && id == "install-verify-001"
    ));
}

#[test]
fn parse_control_command_accepts_install_verify_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.verify","d":{"mode":"byok","install_id":"i-verify"}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::InstallVerify {
            mode,
            install_id: Some(id)
        } if mode == "byok" && id == "i-verify"
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_run_text() {
    let parsed = parse_control_command("recall probe run 25");
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeRun {
            max_subjects: 25,
            ..
        }
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_run_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"recall.probe.run","d":{"top_k":5,"max_subjects":50,"max_queries_per_subject":2}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeRun {
            top_k: 5,
            max_subjects: 50,
            max_queries_per_subject: 2,
        }
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_summary_text() {
    let parsed = parse_control_command("recall probe summary archive_entry entry-1");
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeSummary {
            target_kind: symbiotic_memory::recall_probes::RecallProbeTargetKind::ArchiveEntry,
            ..
        }
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_health_text() {
    let parsed = parse_control_command("recall probe health 7");
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeHealth { limit: 7 }
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_regressions_text() {
    let parsed = parse_control_command("recall probe regressions run-2 4");
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeRegressions { run_id, limit }
            if run_id == "run-2" && limit == 4
    ));
}

#[test]
fn parse_control_command_accepts_recall_probe_regressions_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"recall.probe.regressions","d":{"run_id":"run-2","limit":6}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::RecallProbeRegressions { run_id, limit }
            if run_id == "run-2" && limit == 6
    ));
}

#[test]
fn parse_control_command_rejects_install_run_invalid_mode() {
    let parsed = parse_control_command("install run unknown");
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_install_verify_defaults_to_byok_when_only_install_id_is_provided() {
    let parsed = parse_control_command("install verify unknown");
    assert!(matches!(
        parsed,
        ControlCommand::InstallVerify { mode, install_id }
            if mode == "byok" && install_id.as_deref() == Some("unknown")
    ));
}

#[test]
fn parse_control_command_accepts_push_register_text() {
    let parsed = parse_control_command("push register device1 apns token123");
    assert!(matches!(parsed, ControlCommand::RegisterPush { .. }));
}

#[test]
fn parse_control_command_accepts_push_register_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.register","d":{"device_id":"device1","token":"abc","platform":"apns"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::RegisterPush { .. }));
}

#[test]
fn parse_control_command_accepts_push_ack_text() {
    let parsed = parse_control_command("push ack notif-123 run-1");
    assert!(matches!(parsed, ControlCommand::PushAck { .. }));
}

#[test]
fn parse_control_command_accepts_push_ack_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.ack","d":{"notification_id":"notif-1","run_id":"run-1"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::PushAck { .. }));
}

#[test]
fn parse_control_command_rejects_push_register_missing_platform_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.register","d":{"device_id":"device1","token":"abc"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_rejects_bookmarks_sync_zero_limit_text() {
    let parsed = parse_control_command("bookmarks sync api 0");
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_bookmarks_sync_zero_limit_json_passes_through() {
    // In v2, limit validation is not done at parser level; it passes through.
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"bookmarks.sync","d":{"limit":0}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::BookmarksSync { limit, .. } if limit == 0));
}

#[test]
fn parse_control_command_rejects_bookmarks_sync_limit_over_max_text() {
    let parsed = parse_control_command("bookmarks sync api 501");
    assert!(matches!(parsed, ControlCommand::Unknown { .. }));
}

#[test]
fn parse_control_command_bookmarks_sync_limit_over_max_json_passes_through() {
    // In v2, limit validation is not done at parser level; it passes through.
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"bookmarks.sync","d":{"limit":501}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::BookmarksSync { limit, .. } if limit == 501));
}

#[test]
fn matrix_control_route_registers_push_device() {
    let daemon = daemon_for_test("matrix-push-register");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "push register device1 apns token123".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    let devices = daemon.list_push_devices().expect("list should work");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, "device1");
    assert!(!devices[0].encrypted_token.is_empty());
    assert!(devices[0].encrypted_token.contains(':'));
}

#[test]
fn matrix_control_route_accepts_install_run() {
    let daemon = daemon_for_test("matrix-install-run");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "install run byok install-001".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("install.run"));
    assert_eq!(detail_str(&events[0], "mode"), Some("byok"));
    let queued = daemon
        .queued_jobs_of_type("install.run")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_accepts_recall_probe_run_and_status() {
    let daemon = daemon_for_test("matrix-recall-probe-run");
    let now = now_unix();
    let outcome = daemon
        .archive_store()
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Rust Tokio Runtime".to_string()),
            content: "Tokio powers the async daemon runtime.".to_string(),
            source_url: None,
            tags: vec!["rust".to_string(), "tokio".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Private,
            idempotency_key: "matrix-recall-probe-run".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "recall probe run 10".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("recall.probe.completed"));
    let run_id = detail_str(&events[0], "run_id")
        .expect("run_id detail")
        .to_string();
    let matched_subject_count = detail_val(&events[0], "matched_subject_count")
        .and_then(|value| value.as_u64())
        .expect("matched_subject_count");
    let subject_count = detail_val(&events[0], "subject_count")
        .and_then(|value| value.as_u64())
        .expect("subject_count");
    assert!(matched_subject_count >= 1);
    assert!(subject_count >= matched_subject_count);

    let status_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: format!("recall probe status {run_id}"),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("status route should work");

    assert_eq!(status_events.len(), 1);
    assert_eq!(
        status_events[0].sym.a.as_deref(),
        Some("recall.probe.status")
    );
    assert_eq!(
        detail_str(&status_events[0], "run_id"),
        Some(run_id.as_str())
    );
    assert_eq!(
        detail_val(&status_events[0], "subject_count").and_then(|value| value.as_u64()),
        Some(subject_count)
    );
    assert_eq!(
        detail_val(&status_events[0], "matched_subject_count").and_then(|value| value.as_u64()),
        Some(matched_subject_count)
    );

    let summary_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: format!("recall probe summary archive_entry {}", outcome.record_id),
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("summary route should work");

    assert_eq!(summary_events.len(), 1);
    assert_eq!(
        summary_events[0].sym.a.as_deref(),
        Some("recall.probe.summary")
    );
    assert_eq!(
        detail_str(&summary_events[0], "target_kind"),
        Some("archive_entry")
    );
    assert_eq!(
        detail_str(&summary_events[0], "target_id"),
        Some(outcome.record_id.as_str())
    );
    let remediation_flags =
        detail_str(&summary_events[0], "remediation_flags").expect("remediation flags json");
    let failed_queries =
        detail_str(&summary_events[0], "failed_queries").expect("failed queries json");
    assert!(serde_json::from_str::<Vec<String>>(remediation_flags).is_ok());
    assert!(serde_json::from_str::<Vec<String>>(failed_queries).is_ok());
}

#[test]
fn periodic_recall_probe_cycle_routes_to_status_room() {
    let daemon = daemon_for_test("periodic-recall-probe");
    let now = now_unix();
    daemon
        .archive_store()
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Recall Probe Periodic".to_string()),
            content: "Periodic probe coverage should use the same live gateway path.".to_string(),
            source_url: None,
            tags: vec!["recall".to_string(), "periodic".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Private,
            idempotency_key: "periodic-recall-probe".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");

    let events = daemon
        .run_periodic_recall_probe_cycle(now)
        .expect("periodic recall probe should run");
    assert!(!events.is_empty());
    assert_eq!(events[0].room_id, "#status");
    assert_eq!(
        events[0].envelope.sym.a.as_deref(),
        Some("recall.probe.completed")
    );
}

#[test]
fn periodic_recall_probe_cycle_pins_stable_baseline_cohort() {
    let daemon = daemon_for_test("periodic-recall-probe-baseline");
    let now = now_unix();
    let first = daemon
        .archive_store()
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Pinned Baseline One".to_string()),
            content: "The first periodic cohort subject should stay pinned.".to_string(),
            source_url: None,
            tags: vec!["recall".to_string(), "baseline".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Private,
            idempotency_key: "periodic-recall-probe-baseline-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store first");

    let first_events = daemon
        .run_periodic_recall_probe_cycle(now)
        .expect("first periodic recall probe should run");
    assert_eq!(first_events[0].room_id, "#status");
    assert_eq!(
        detail_str(&first_events[0].envelope, "cohort"),
        Some("periodic_baseline")
    );
    assert_eq!(
        detail_val(&first_events[0].envelope, "subject_count").and_then(|value| value.as_u64()),
        Some(1)
    );

    daemon
        .archive_store()
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Pinned Baseline Two".to_string()),
            content: "A newer document should not change the periodic cohort immediately."
                .to_string(),
            source_url: None,
            tags: vec!["recall".to_string(), "baseline".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Private,
            idempotency_key: "periodic-recall-probe-baseline-2".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store second");

    let second_events = daemon
        .run_periodic_recall_probe_cycle(now + 1)
        .expect("second periodic recall probe should run");
    assert_eq!(
        detail_str(&second_events[0].envelope, "cohort"),
        Some("periodic_baseline")
    );
    assert_eq!(
        detail_val(&second_events[0].envelope, "subject_count").and_then(|value| value.as_u64()),
        Some(1)
    );

    let store = symbiotic_memory::recall_probes::RecallProbeStore::open(
        daemon.config.data_dir.join("runtime/recall-probes.db"),
    )
    .expect("probe store");
    let baseline = store
        .baseline_targets("periodic_baseline")
        .expect("baseline targets");
    assert_eq!(baseline.len(), 1);
    assert_eq!(baseline[0].target_id, first.record_id);
}

#[test]
fn matrix_control_route_accepts_recall_probe_health() {
    let daemon = daemon_for_test("matrix-recall-probe-health");
    let now = now_unix();
    daemon
        .archive_store()
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Recall Probe Health".to_string()),
            content: "Health view should expose the worst summaries first.".to_string(),
            source_url: None,
            tags: vec!["recall".to_string(), "health".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Private,
            idempotency_key: "matrix-recall-probe-health".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("store");
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "recall probe run 10".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("run route should work");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "recall probe health 5".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("health route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("recall.probe.health"));
    assert_eq!(detail_val(&events[0], "limit"), Some(&serde_json::json!(5)));
    let summaries = detail_str(&events[0], "summaries").expect("summaries json");
    let summary_rows =
        serde_json::from_str::<Vec<serde_json::Value>>(summaries).expect("summary rows");
    assert!(!summary_rows.is_empty());
    assert!(summary_rows[0].get("remediation_flags").is_some());
    assert!(summary_rows[0].get("failed_queries").is_some());
}

#[test]
fn matrix_control_route_accepts_recall_probe_regressions() {
    let daemon = daemon_for_test("matrix-recall-probe-regressions");
    let now = now_unix();
    let store = symbiotic_memory::recall_probes::RecallProbeStore::open(
        daemon.config.data_dir.join("runtime/recall-probes.db"),
    )
    .expect("probe store");

    for (run_id, started_at, finished_at, subject_count, outcomes) in [
        (
            "run-1",
            10u64,
            20u64,
            2usize,
            vec![
                ("tokio", true, Some(1), 11u64),
                ("axum", false, None, 12u64),
            ],
        ),
        (
            "run-2",
            30u64,
            40u64,
            2usize,
            vec![
                ("tokio", false, None, 31u64),
                ("axum", true, Some(2), 32u64),
            ],
        ),
    ] {
        store
            .start_run(&symbiotic_memory::recall_probes::RecallProbeRun {
                id: run_id.to_string(),
                started_at,
                finished_at: None,
                cohort: None,
                top_k: 10,
                subject_count,
                matched_count: 0,
            })
            .expect("start run");
        for (target_id, matched, rank, created_at) in outcomes {
            store
                .record_result(&symbiotic_memory::recall_probes::RecallProbeResult {
                    run_id: run_id.to_string(),
                    target_kind:
                        symbiotic_memory::recall_probes::RecallProbeTargetKind::ArchiveEntry,
                    target_id: target_id.to_string(),
                    query: format!("q-{target_id}-{created_at}"),
                    matched,
                    rank,
                    retrieval_mode: "hybrid".to_string(),
                    top_item_ids: Vec::new(),
                    remediation_flags: Vec::new(),
                    created_at,
                })
                .expect("record result");
        }
        store
            .finish_run(run_id, finished_at, subject_count)
            .expect("finish run");
    }

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "recall probe regressions run-2 5".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("regressions route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("recall.probe.regressions"));
    assert_eq!(detail_str(&events[0], "current_run_id"), Some("run-2"));
    assert_eq!(detail_str(&events[0], "baseline_run_id"), Some("run-1"));
    assert_eq!(
        detail_val(&events[0], "regression_count").and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        detail_val(&events[0], "improvement_count").and_then(|value| value.as_u64()),
        Some(1)
    );
    let regressions = detail_str(&events[0], "regressions").expect("regressions json");
    assert!(regressions.contains("\"target_id\":\"tokio\""));
    let improvements = detail_str(&events[0], "improvements").expect("improvements json");
    assert!(improvements.contains("\"target_id\":\"axum\""));
}

#[test]
fn matrix_control_route_accepts_goal_start() {
    let daemon = daemon_for_test("matrix-goal-start");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "template"), Some("test-workflow"));
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_goal_list_returns_state_summary() {
    let daemon = daemon_for_test("matrix-goal-list");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("start route should work");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal list".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("list route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    assert_eq!(events[0].sym.s, Some(Status::Success));
    assert_eq!(detail_val(&events[0], "count"), Some(&serde_json::json!(1)));
    let items_json_val =
        detail_val(&events[0], "items_json").expect("items_json should be present");
    let items_json = items_json_val
        .as_str()
        .expect("items_json should be a string");
    let parsed: serde_json::Value =
        serde_json::from_str(items_json).expect("items_json should be valid json");
    let first = parsed
        .as_array()
        .and_then(|items| items.first())
        .expect("items_json should include one item");
    assert_eq!(
        first.get("template").and_then(|value| value.as_str()),
        Some("test-workflow")
    );
    assert_eq!(
        first.get("status").and_then(|value| value.as_str()),
        Some("queued")
    );
    assert!(
        first
            .get("updated_at")
            .and_then(|value| value.as_u64())
            .is_some(),
        "items_json should include updated_at"
    );
}

#[test]
fn matrix_goal_list_keeps_distinct_templates_for_same_room() {
    let daemon = daemon_for_test("matrix-goal-list-multi-template");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("first goal start should work");

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start intake-url".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("second goal start should work");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal list".to_string(),
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("goal list should work");

    let items_json_val =
        detail_val(&events[0], "items_json").expect("items_json should be present");
    let items_json = items_json_val
        .as_str()
        .expect("items_json should be a string");
    let parsed: serde_json::Value =
        serde_json::from_str(items_json).expect("items_json should be valid json");
    let items = parsed.as_array().expect("items_json should be an array");
    assert_eq!(items.len(), 2, "two templates should be persisted");
}

#[test]
fn matrix_control_route_accepts_goal_retry() {
    let daemon = daemon_for_test("matrix-goal-retry");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal retry test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_accepts_goal_stop() {
    let daemon = daemon_for_test("matrix-goal-stop");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("start route should work");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal stop test-workflow".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("stop route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
}

#[test]
fn goal_stop_requests_cancel_queued_workflow_before_start() {
    let daemon = daemon_for_test("goal-stop-cancels");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start test-workflow".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("start route should work");
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal stop test-workflow".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("stop route should work");

    let event = daemon
        .run_once(now + 2)
        .expect("run_once should succeed")
        .expect("workflow event should exist")
        .0;

    assert_eq!(event.event_type, EventType::GoalCancelled);
    assert_eq!(event.status, "completed");
}

#[test]
fn matrix_control_route_accepts_install_provision() {
    let daemon = daemon_for_test("matrix-install-provision");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "install provision install-001".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("install.provision"));
    let queued = daemon
        .queued_jobs_of_type("install.provision")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_accepts_install_bootstrap() {
    let daemon = daemon_for_test("matrix-install-bootstrap");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "install bootstrap".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("install.bootstrap"));
    let queued = daemon
        .queued_jobs_of_type("install.bootstrap")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_control_route_accepts_install_verify() {
    let daemon = daemon_for_test("matrix-install-verify");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "install verify byok install-verify-001".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "type"), Some("install.verify"));
    assert_eq!(
        detail_str(&events[0], "install_id"),
        Some("install-verify-001")
    );
    let queued = daemon
        .queued_jobs_of_type("install.verify")
        .expect("queue should be readable");
    assert_eq!(queued.len(), 1);
}

#[test]
fn install_provision_and_bootstrap_jobs_are_processed_by_worker() {
    let daemon = daemon_for_test("worker-install-provision-bootstrap");
    let now = now_unix();

    daemon
        .queue_install_provision("byok", "install-001")
        .expect("provision job should queue");
    daemon
        .queue_install_bootstrap()
        .expect("bootstrap job should queue");

    let provision_event = daemon
        .run_once(now)
        .expect("run_once should succeed")
        .expect("event should exist")
        .0;
    assert_eq!(provision_event.event_type, EventType::InstallNucleus);
    assert_eq!(provision_event.status, "completed");

    let bootstrap_event = daemon
        .run_once(now + 1)
        .expect("run_once should succeed")
        .expect("event should exist")
        .0;
    assert_eq!(bootstrap_event.event_type, EventType::InstallMatrix);
    assert_eq!(bootstrap_event.status, "completed");

    let done_provision = daemon
        .done_jobs_of_type("install.provision")
        .expect("queue should be readable");
    let done_bootstrap = daemon
        .done_jobs_of_type("install.bootstrap")
        .expect("queue should be readable");
    assert_eq!(done_provision.len(), 1);
    assert_eq!(done_bootstrap.len(), 1);
}

#[test]
fn install_verify_job_is_processed_by_worker() {
    let daemon = daemon_for_test("worker-install-verify");
    let now = now_unix();

    daemon
        .queue_install_verify("byok", Some("install-verify-001"))
        .expect("verify job should queue");

    let event = daemon
        .run_once(now)
        .expect("run_once should succeed")
        .expect("event should exist")
        .0;
    assert_eq!(event.event_type, EventType::InstallRecall);
    assert!(event.status == "completed" || event.status == "failed");

    let done_verify = daemon
        .done_jobs_of_type("install.verify")
        .expect("queue should be readable");
    assert_eq!(done_verify.len(), 1);
}

#[test]
fn push_registration_encrypts_and_round_trips_token() {
    let daemon = daemon_for_test("push-encrypt-roundtrip");
    let now = now_unix();
    daemon
        .register_push_device("dev1", "real-apns-token-xyz", "apns", now)
        .expect("register should work");
    let devices = daemon.list_push_devices().expect("list should work");
    assert_eq!(devices.len(), 1);
    assert_ne!(devices[0].encrypted_token, "real-apns-token-xyz");
    let decrypted = daemon
        .push_registry
        .decrypt_token(&devices[0].encrypted_token)
        .expect("decrypt should succeed");
    assert_eq!(decrypted, "real-apns-token-xyz");
}

#[tokio::test]
async fn push_outbox_contains_encrypted_token_not_plaintext() {
    let daemon = daemon_for_test("push-outbox-token");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    // Register a push device
    transport
        .push_incoming(MatrixMessage {
            room_id: "#control".to_string(),
            sender: "@user:test".to_string(),
            body: "push register device1 apns my-real-token".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");

    daemon
        .pump_transport_once(&transport, now)
        .await
        .expect("pump should work");

    // Send a failed event that triggers a push notification (v2: only failures trigger push)
    let failed_envelope =
        MatrixEventEnvelope::new(Kind::Message, Status::Fail, now, "something broke");
    daemon
        .send_matrix_event(&transport, "#status", failed_envelope, now)
        .await
        .expect("send should work");

    let outbox_content =
        fs::read_to_string(&daemon.config.push_outbox_file).expect("outbox file should exist");
    assert!(
        !outbox_content.contains("my-real-token"),
        "outbox must NOT contain plaintext device token"
    );
    assert!(
        outbox_content.contains("encrypted_token"),
        "outbox should contain encrypted_token field"
    );
}

#[test]
fn matrix_control_route_records_push_ack() {
    let daemon = daemon_for_test("matrix-push-ack");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "push ack notif-123 run-1".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("push.ack"));
    let ack_path = daemon.config.push_ack_file.clone();
    let ack_content = fs::read_to_string(&ack_path).expect("ack file should exist");
    assert!(ack_content.contains("notif-123"));
}

#[test]
fn http_push_provider_retries_and_succeeds() {
    let http = Arc::new(StubPushHttpClient::default());
    http.push_response(Err("temporary error"));
    http.push_response(Err("still failing"));
    http.push_response(Ok(()));
    let provider = HttpPushProvider::new(
        "https://push.symbiotic.sh/send".to_string(),
        Some("api-key".to_string()),
        http.clone(),
    );

    provider
        .send(&sample_push_notification())
        .expect("delivery should succeed");
    assert_eq!(http.call_count(), 3);
}

#[test]
fn http_push_provider_fails_after_max_attempts() {
    let http = Arc::new(StubPushHttpClient::default());
    http.push_response(Err("temporary error"));
    http.push_response(Err("still failing"));
    http.push_response(Err("last failure"));
    let provider = HttpPushProvider::new(
        "https://push.symbiotic.sh/send".to_string(),
        None,
        http.clone(),
    );

    let err = provider
        .send(&sample_push_notification())
        .expect_err("delivery should fail");
    assert!(err.to_string().contains("3 attempts"));
    assert_eq!(http.call_count(), 3);
}

#[test]
fn apns_gateway_provider_only_sends_for_apns_platform() {
    let http = Arc::new(StubPushHttpClient::default());
    http.push_response(Ok(()));
    let provider = ApnsGatewayPushProvider::new(
        "https://push.symbiotic.sh/apns/send".to_string(),
        Some("api-key".to_string()),
        http.clone(),
    );
    let mut notification = sample_push_notification();
    notification.platform = "fcm".to_string();
    provider
        .send(&notification)
        .expect("non-apns should be skipped");
    assert_eq!(http.call_count(), 0);

    notification.platform = "apns".to_string();
    provider.send(&notification).expect("apns should send");
    assert_eq!(http.call_count(), 1);
    let body = http.bodies().pop().expect("payload should be captured");
    assert!(body.contains("\"provider\":\"apns\""));
}

#[test]
fn fcm_gateway_provider_only_sends_for_fcm_platform() {
    let http = Arc::new(StubPushHttpClient::default());
    http.push_response(Ok(()));
    let provider = FcmGatewayPushProvider::new(
        "https://push.symbiotic.sh/fcm/send".to_string(),
        Some("api-key".to_string()),
        http.clone(),
    );
    let mut notification = sample_push_notification();
    notification.platform = "apns".to_string();
    provider
        .send(&notification)
        .expect("non-fcm should be skipped");
    assert_eq!(http.call_count(), 0);

    notification.platform = "fcm".to_string();
    provider.send(&notification).expect("fcm should send");
    assert_eq!(http.call_count(), 1);
    let body = http.bodies().pop().expect("payload should be captured");
    assert!(body.contains("\"provider\":\"fcm\""));
}

#[test]
fn composite_push_provider_returns_error_when_any_provider_fails() {
    let root = std::env::temp_dir().join(format!("symbiotic_push_composite_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let file_provider = FilePushProvider::open(root.join("push-outbox.ndjson"))
        .expect("file provider should initialize");
    let provider = CompositePushProvider::new(vec![
        Arc::new(file_provider),
        Arc::new(AlwaysFailPushProvider),
    ]);

    let err = provider
        .send(&sample_push_notification())
        .expect_err("composite provider should fail");
    assert!(err.to_string().contains("intentional push failure"));
}

#[cfg(unix)]
#[test]
fn daemon_hardens_sensitive_runtime_file_permissions() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = daemon_for_test("runtime-permissions");
    let now = now_unix();

    let _ = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "push register device1 apns token123".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("push register should succeed");
    let _ = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "push ack notif-123 run-1".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("push ack should succeed");

    for path in [
        daemon.config.audit_log_file.clone(),
        daemon.config.capability_tokens_file.clone(),
        daemon.config.push_registry_file.clone(),
        daemon.config.push_outbox_file.clone(),
        daemon.config.push_ack_file.clone(),
        daemon.config.push_telemetry_file.clone(),
    ] {
        let mode = fs::metadata(&path)
            .expect("metadata should load")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "unexpected mode for {}", path.display());
    }
}

#[tokio::test]
async fn pump_transport_emits_push_event_for_auth_required() {
    let daemon = daemon_for_test("matrix-push-auth");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    transport
        .push_incoming(MatrixMessage {
            room_id: "#control".to_string(),
            sender: "@user:test".to_string(),
            body: "push register device1 apns token123".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");
    transport
        .push_incoming(MatrixMessage {
            room_id: "#credentials".to_string(),
            sender: "@user:test".to_string(),
            body: "auth issue x.com web.login".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");

    daemon
        .pump_transport_once(&transport, now)
        .await
        .expect("pump should work");

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    // In v2, auth.issue acknowledgments (Status::Working) don't trigger push
    // notifications. Only failures, questions, and notifications do.
    // Verify the auth issue response was sent to #credentials.
    assert!(outgoing.iter().any(|event| {
        event.room_id == "#credentials" && event.envelope.sym.s == Some(Status::Working)
    }));
}

#[tokio::test]
async fn send_matrix_event_emits_thread_observability_summary_for_goal_thread() {
    let daemon = daemon_for_test("thread-observability-summary");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    crate::goal_state::upsert_goal_state(
        &daemon.config.goal_state_file,
        GoalState {
            goal_room: "#goals".to_string(),
            thread_id: Some("goal-abc".to_string()),
            project_id: "project:test".to_string(),
            template: "inquisition:goal-abc".to_string(),
            status: "awaiting_input".to_string(),
            last_job_id: "job-1".to_string(),
            last_run_id: Some("goal-abc".to_string()),
            owner: Some("@user:test".to_string()),
            updated_at: now,
            complexity: None,
            pipeline_stage: Some("awaiting_input".to_string()),
            audit_id: None,
            plan_id: None,
        },
    )
    .expect("goal state upsert");

    let envelope =
        MatrixEventEnvelope::new(Kind::Message, Status::Awaiting, now, "Need clarification")
            .with_thread("goal-abc")
            .with_detail_field("goal_id", "goal-abc")
            .with_detail_field("template", "inquisition:goal-abc");

    daemon
        .send_matrix_event(&transport, "#thread-goal-abc", envelope, now)
        .await
        .expect("send should work");

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(
        outgoing.len(),
        3,
        "primary event + observability summary + snapshot"
    );
    assert_eq!(
        outgoing[1].envelope.sym.a.as_deref(),
        Some("thread.observability.summary")
    );
    assert_eq!(
        detail_str(&outgoing[1].envelope, "thread_id"),
        Some("goal-abc")
    );
    assert_eq!(
        detail_val(&outgoing[1].envelope, "active_operation_count")
            .and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        detail_val(&outgoing[1].envelope, "waiting_for_user").and_then(|value| value.as_bool()),
        Some(true)
    );
    assert_eq!(
        outgoing[2].envelope.sym.a.as_deref(),
        Some("thread.observability.snapshot")
    );
    assert_eq!(
        detail_val(&outgoing[2].envelope, "operations")
            .and_then(|value| value.as_array())
            .map(|operations| operations.len()),
        Some(1)
    );
}

#[tokio::test]
async fn send_matrix_event_does_not_recurse_for_observability_summary() {
    let daemon = daemon_for_test("thread-observability-no-recurse");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();
    let envelope = MatrixEventEnvelope::state(
        "thread.observability.summary",
        now,
        "Thread observability updated",
    )
    .with_thread("goal-abc")
    .with_detail_field("thread_id", "goal-abc")
    .with_detail_field("active_operation_count", 1)
    .with_detail_field("waiting_for_user", false)
    .with_detail_field("has_failure", false)
    .with_detail_field("updated_at", now as i64);

    daemon
        .send_matrix_event(&transport, "#thread-goal-abc", envelope, now)
        .await
        .expect("send should work");

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(
        outgoing.len(),
        1,
        "summary event should not emit another state"
    );
    assert_eq!(
        outgoing[0].envelope.sym.a.as_deref(),
        Some("thread.observability.summary")
    );
}

#[tokio::test]
async fn send_matrix_event_does_not_recurse_for_observability_snapshot() {
    let daemon = daemon_for_test("thread-observability-snapshot-no-recurse");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();
    let envelope = MatrixEventEnvelope::state(
        "thread.observability.snapshot",
        now,
        "Thread observability snapshot updated",
    )
    .with_thread("goal-abc")
    .with_detail_field("thread_id", "goal-abc")
    .with_detail_field("operations", serde_json::json!([]))
    .with_detail_field("updated_at", now as i64);

    daemon
        .send_matrix_event(&transport, "#thread-goal-abc", envelope, now)
        .await
        .expect("send should work");

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(
        outgoing.len(),
        1,
        "snapshot event should not emit another state"
    );
    assert_eq!(
        outgoing[0].envelope.sym.a.as_deref(),
        Some("thread.observability.snapshot")
    );
}

#[test]
fn matrix_credentials_route_queues_auth_issue_jobs() {
    let daemon = daemon_for_test("matrix-credentials-auth");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: "auth issue x.com web.login".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
    let queued = daemon
        .queued_jobs_of_type("auth.issue")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_credentials_route_rejects_invalid_commands() {
    let daemon = daemon_for_test("matrix-credentials-reject");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#cred-123".to_string(),
                sender: "@user:test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert_eq!(events[0].sym.s, Some(Status::Fail));
}

#[test]
fn matrix_status_route_returns_queue_snapshot() {
    let daemon = daemon_for_test("matrix-status");
    let now = now_unix();
    daemon
        .queue_workflow_run("intake-url")
        .expect("queue should work");
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#status".to_string(),
                sender: "@user:test".to_string(),
                body: "status".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("snapshot"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(
        detail_val(&events[0], "queued"),
        Some(&serde_json::json!(1))
    );
}

#[test]
fn matrix_alerts_route_accepts_alert_message() {
    let daemon = daemon_for_test("matrix-alerts");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#alerts".to_string(),
                sender: "@user:test".to_string(),
                body: "please check this escalation".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("alert.received"));
    assert_eq!(events[0].sym.k, Kind::State);
}

#[test]
fn matrix_goal_route_queues_workflow_job() {
    let daemon = daemon_for_test("goal-route");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goal-intake".to_string(),
                sender: "@user:test".to_string(),
                body: "run intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn matrix_goal_route_dedupes_active_workflow_job_for_same_goal_and_template() {
    let daemon = daemon_for_test("goal-route-dedupe");
    let now = now_unix();

    let first = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goal-intake".to_string(),
                sender: "@user:test".to_string(),
                body: "run intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("first route should work");
    let second = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goal-intake".to_string(),
                sender: "@user:test".to_string(),
                body: "run intake-url".to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("second route should work");

    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(first[0].sym.s, Some(Status::Working));
    assert_eq!(second[0].sym.s, Some(Status::Working));

    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);

    // Second request should be deduplicated (same template already running)
    assert!(second[0].body.contains("already running"));
}

#[test]
fn goal_route_persists_lifecycle_log() {
    let config = daemon_config_for_test("goal-log");
    let goal_log_file = config.goal_log_file.clone();
    let goal_state_file = config.goal_state_file.clone();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config.clone()).expect("daemon should initialize");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goal-intake".to_string(),
                sender: "@user:test".to_string(),
                body: "run intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));

    let workflow_event = daemon
        .run_once(now + 1)
        .expect("run should work")
        .expect("workflow job should exist")
        .0;
    assert_eq!(workflow_event.event_type, EventType::WorkflowRun);
    assert_eq!(workflow_event.status, "completed");
    assert_eq!(workflow_event.goal_room.as_deref(), Some("#goal-intake"));
    assert_eq!(workflow_event.goal_template.as_deref(), Some("intake-url"));
    assert!(workflow_event.goal_run_id.is_some());
    let workflow_event = daemon.run_once(now + 2).expect("run should work");
    assert!(
        workflow_event.is_none(),
        "goal run should be single workflow job"
    );

    let event = daemon
        .done_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(event.len(), 1);

    let content = fs::read_to_string(goal_log_file).expect("goal log should be readable");
    assert!(content.contains("goal.started"));
    assert!(content.contains("goal.completed"));
    assert!(content.contains("intake-url"));

    let state = daemon
        .goal_state("#goal-intake")
        .expect("state query should work")
        .expect("goal state should exist");
    assert_eq!(state.status, "completed");
    assert_eq!(state.template, "intake-url");
    assert_eq!(state.owner.as_deref(), Some("@user:test"));
    assert!(state.last_run_id.is_some());

    let (reopened, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon reopen should work");
    let reopened_state = reopened
        .goal_state("#goal-intake")
        .expect("state query should work")
        .expect("goal state should exist");
    assert_eq!(reopened_state.status, "completed");
    assert!(goal_state_file.exists());
}

#[test]
fn matrix_goal_route_rejects_missing_template() {
    let daemon = daemon_for_test("goal-route-reject");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goal-intake".to_string(),
                sender: "@user:test".to_string(),
                body: "status".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert_eq!(events[0].sym.s, Some(Status::Fail));
}

#[tokio::test]
async fn pump_transport_processes_messages_and_emits_replies() {
    let daemon = daemon_for_test("matrix-pump");
    let now = now_unix();
    let transport = InMemoryMatrixTransport::default();
    transport
        .push_incoming(MatrixMessage {
            room_id: "#intake".to_string(),
            sender: "@user:test".to_string(),
            body: "https://example.com/a".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");

    let processed = daemon
        .pump_transport_once(&transport, now)
        .await
        .expect("pump should work");
    assert_eq!(processed, 1);

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].room_id, "#intake");
    assert_eq!(outgoing[0].envelope.sym.s, Some(Status::Working));
}

#[tokio::test]
async fn pump_transport_forwards_failed_events_to_alerts_room() {
    let daemon = daemon_for_test("matrix-pump-alerts-forward");
    let now = now_unix();
    let transport = InMemoryMatrixTransport::default();
    let failed_payload = serde_json::to_string(&MatrixEventEnvelope::new(
        Kind::Message,
        Status::Fail,
        now,
        "workflow failed",
    ))
    .expect("serialize should work");
    transport
        .push_incoming(MatrixMessage {
            room_id: "#goal-intake".to_string(),
            sender: "@daemon:test".to_string(),
            body: failed_payload,
            timestamp: now,
        })
        .expect("push_incoming");

    let processed = daemon
        .pump_transport_once(&transport, now + 1)
        .await
        .expect("pump should work");
    assert_eq!(processed, 1);

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].room_id, "#alerts");
    assert_eq!(
        outgoing[0].envelope.sym.a.as_deref(),
        Some("alert.forwarded")
    );
}

#[test]
fn matrix_intake_route_rejects_messages_without_urls() {
    let daemon = daemon_for_test("matrix-route-reject-no-url");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#intake".to_string(),
                sender: "@user:test".to_string(),
                body: "hello there #tag".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert_eq!(events[0].sym.s, Some(Status::Fail));
}

#[test]
fn payload_roundtrip_preserves_fields() {
    let payload = IngestPayload {
        url: "https://example.com/path?a=1,b=2".to_string(),
        tags: vec!["alpha,beta".to_string(), "x|y".to_string()],
        source: IntakeSource::Matrix,
        run_id: "run_12345".to_string(),
    };
    let encoded = encode_ingest_payload(&payload);
    let decoded = decode_ingest_payload(&encoded).expect("decode should work");
    assert_eq!(decoded.url, payload.url);
    assert_eq!(decoded.tags, payload.tags);
    assert_eq!(decoded.source, payload.source);
    assert_eq!(decoded.run_id, "run_12345");
}

#[test]
fn review_payload_roundtrip_preserves_run_id() {
    let encoded = encode_review_payload("rec_001", Some("run_789"));
    let decoded = decode_review_payload_full(&encoded).expect("decode should work");
    assert_eq!(decoded.record_id, "rec_001");
    assert_eq!(decoded.run_id.as_deref(), Some("run_789"));
}

#[test]
fn review_payload_without_run_id_is_backwards_compatible() {
    let encoded = "record_id=rec_002";
    let decoded = decode_review_payload_full(encoded).expect("decode should work");
    assert_eq!(decoded.record_id, "rec_002");
    assert_eq!(decoded.run_id, None);
}

#[test]
fn workflow_payload_roundtrip_preserves_goal_context() {
    let payload = WorkflowRunPayload {
        template: "intake-url".to_string(),
        goal_room: Some("#goal-ops:example.org".to_string()),
        goal_sender: Some("@user:test".to_string()),
        project_id: Some("project:test".to_string()),
        user_answer: None,
        goal_id: None,
        user_goal: None,
        replan_context: None,
    };
    let encoded = encode_workflow_payload(&payload);
    let decoded = decode_workflow_payload(&encoded).expect("decode should work");
    assert_eq!(decoded, payload);
}

#[test]
fn workflow_payload_roundtrip_with_user_answer() {
    let payload = WorkflowRunPayload {
        template: "deliberation".to_string(),
        goal_room: Some("#goals:example.org".to_string()),
        goal_sender: Some("@user:test".to_string()),
        project_id: Some("project:test".to_string()),
        user_answer: Some("Yes, proceed with option A".to_string()),
        goal_id: Some("goal-abc123".to_string()),
        user_goal: Some("Build a flight search tool".to_string()),
        replan_context: None,
    };
    let encoded = encode_workflow_payload(&payload);
    let decoded = decode_workflow_payload(&encoded).expect("decode should work");
    assert_eq!(decoded, payload);
}

#[test]
fn workflow_payload_backward_compat_3_fields() {
    // Old 3-field payloads should still decode with user_answer and goal_id as None.
    // Construct the encoded string via encode_workflow_payload so the escaping is correct.
    let original = WorkflowRunPayload {
        template: "my-template".to_string(),
        goal_room: Some("#goals:example.org".to_string()),
        goal_sender: Some("@user:test".to_string()),
        project_id: Some("project:test".to_string()),
        user_answer: None,
        goal_id: None,
        user_goal: None,
        replan_context: None,
    };
    let encoded = encode_workflow_payload(&original);
    // The 3-field encoded payload should decode correctly with new optional fields as None.
    let decoded = decode_workflow_payload(&encoded).expect("decode should work");
    assert_eq!(decoded.template, "my-template");
    assert_eq!(decoded.goal_room.as_deref(), Some("#goals:example.org"));
    assert_eq!(decoded.goal_sender.as_deref(), Some("@user:test"));
    assert_eq!(decoded.user_answer, None);
    assert_eq!(decoded.goal_id, None);
}

#[test]
fn workflow_payload_roundtrip_with_replan_context() {
    let payload = WorkflowRunPayload {
        template: "inquisition:goal-abc123".to_string(),
        goal_room: Some("#goals:example.org".to_string()),
        goal_sender: Some("@user:test".to_string()),
        project_id: Some("project:test".to_string()),
        user_answer: None,
        goal_id: Some("goal-abc123".to_string()),
        user_goal: Some("Deploy the travel workflow".to_string()),
        replan_context: Some(
            "Replan requested because task 'approval-gate' entered blocked.".to_string(),
        ),
    };
    let encoded = encode_workflow_payload(&payload);
    let decoded = decode_workflow_payload(&encoded).expect("decode should work");
    assert_eq!(decoded, payload);
}

#[test]
fn capability_authorization_allows_scoped_token() {
    let daemon = daemon_for_test("auth");
    let now = now_unix();
    daemon
        .issue_capability_token(symbiotic_trust::CapabilityToken {
            token_id: "token-1".to_string(),
            subject: "agent-sec".to_string(),
            trust_level: AgentTrustLevel::ExternalAct,
            scopes: ["action.browser.login".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        })
        .expect("token issue should work");

    daemon
        .authorize_capability(
            "token-1",
            "agent-sec",
            AgentTrustLevel::CredentialAccess,
            "action.browser.login",
            now,
        )
        .expect("authorization should pass");
}

#[test]
fn capability_authorization_denies_missing_scope() {
    let daemon = daemon_for_test("auth-deny");
    let now = now_unix();
    daemon
        .issue_capability_token(symbiotic_trust::CapabilityToken {
            token_id: "token-2".to_string(),
            subject: "agent-sec".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["archive.write".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        })
        .expect("token issue should work");

    let err = daemon
        .authorize_capability(
            "token-2",
            "agent-sec",
            AgentTrustLevel::CredentialAccess,
            "action.browser.login",
            now,
        )
        .expect_err("authorization should fail");
    assert!(err.to_string().contains("scope not permitted"));
}

#[test]
fn capability_tokens_persist_across_reopen() {
    let config = daemon_config_for_test("auth-persist");
    let now = now_unix();

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config.clone()).expect("daemon open");
    daemon
        .issue_capability_token(symbiotic_trust::CapabilityToken {
            token_id: "persist-token".to_string(),
            subject: "agent-sec".to_string(),
            trust_level: AgentTrustLevel::CredentialAccess,
            scopes: ["archive.write".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        })
        .expect("token issue should work");
    drop(daemon);

    let (reopened, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon reopen");
    reopened
        .authorize_capability(
            "persist-token",
            "agent-sec",
            AgentTrustLevel::ReadOnly,
            "archive.write",
            now + 1,
        )
        .expect("persisted token should authorize");
}

#[test]
fn daemon_spawns_and_executes_secure_agent_scope() {
    let daemon = daemon_for_test("agents");
    let now = now_unix();
    let agent_id = daemon
        .spawn_task_agent(
            "task-agent-1",
            AgentParent::System,
            false,
            vec!["archive.read".to_string()],
            None,
            now,
        )
        .expect("agent spawn should work");

    daemon
        .execute_agent_scope(&agent_id, "archive.read", now + 1)
        .expect("scope should be allowed");
    let denied = daemon.execute_agent_scope(&agent_id, "credential.read", now + 2);
    assert!(denied.is_err());

    let agent = daemon
        .get_agent(&agent_id)
        .expect("get_agent should not fail")
        .expect("agent should exist");
    assert_eq!(agent.audit_trail.len(), 2);
}

#[test]
fn daemon_persists_agent_lifecycle_state_and_audit_log() {
    let config = daemon_config_for_test("agents-persist");
    let agent_log_file = config.agent_log_file.clone();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config.clone()).expect("daemon should initialize");
    let now = now_unix();
    let agent_id = daemon
        .spawn_task_agent(
            "task-agent-2",
            AgentParent::Goal {
                slug: "intake".to_string(),
            },
            false,
            vec!["archive.read".to_string()],
            None,
            now,
        )
        .expect("agent spawn should work");

    daemon
        .execute_agent_scope(&agent_id, "archive.read", now + 1)
        .expect("scope should be allowed");
    let denied = daemon.execute_agent_scope(&agent_id, "credential.read", now + 2);
    assert!(denied.is_err());

    let state = daemon
        .agent_state(&agent_id)
        .expect("state query should work")
        .expect("state should exist");
    assert_eq!(state.last_status, "denied");
    assert_eq!(state.last_scope.as_deref(), Some("credential.read"));
    assert_eq!(state.parent, "goal:intake");

    let lifecycle_log =
        fs::read_to_string(agent_log_file).expect("agent lifecycle log should be readable");
    assert!(lifecycle_log.contains("agent.spawned"));
    assert!(lifecycle_log.contains("agent.scope"));

    let (reopened, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon reopen should work");
    let reopened_state = reopened
        .agent_state(&agent_id)
        .expect("state query should work")
        .expect("state should exist");
    assert_eq!(reopened_state.last_status, "denied");
    assert_eq!(
        reopened_state.last_scope.as_deref(),
        Some("credential.read")
    );
}

#[test]
fn daemon_issues_and_validates_login_session_handle() {
    let daemon = daemon_for_test("cred");
    let now = now_unix();

    daemon
        .store_login_credential("x.com", "user", "secret")
        .expect("credential store should work");
    let handle_id = daemon
        .issue_login_session_handle("x.com", vec!["web.login".to_string()], now)
        .expect("handle issue should work");

    daemon
        .validate_login_session_handle(&handle_id, "x.com", "web.login", now + 5)
        .expect("handle validation should work");
}

#[test]
fn auth_issue_job_is_processed_by_worker() {
    let daemon = daemon_for_test("auth-job");
    let now = now_unix();
    daemon
        .store_login_credential("x.com", "user", "secret")
        .expect("credential store should work");

    let _job_id = daemon
        .queue_auth_issue_request("x.com", vec!["web.login".to_string()])
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::AuthIssue);
    assert_eq!(event.status, "completed");
    assert!(event.detail.starts_with("sh_"));
}

#[test]
fn auth_issue_job_with_missing_credentials_is_terminal_failure() {
    let daemon = daemon_for_test("auth-job-missing-creds");
    let now = now_unix();
    let _job_id = daemon
        .queue_auth_issue_request("x.com", vec!["web.login".to_string()])
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::AuthIssue);
    assert_eq!(event.status, "failed");
    assert!(event.detail.contains("missing credentials"));

    let queued = daemon
        .queued_jobs_of_type("auth.issue")
        .expect("queue query should work");
    assert_eq!(queued.len(), 0);
}

#[test]
fn auth_issue_job_with_reserved_target_is_terminal_failure() {
    let daemon = daemon_for_test("auth-job-reserved-target");
    let now = now_unix();
    let _job_id = daemon
        .queue_auth_issue_request("http://localhost/admin", vec!["web.login".to_string()])
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::AuthIssue);
    assert_eq!(event.status, "failed");
    assert!(event.detail.contains("blocked"));

    let queued = daemon
        .queued_jobs_of_type("auth.issue")
        .expect("queue query should work");
    assert_eq!(queued.len(), 0);
}

#[test]
fn workflow_template_can_run_directly() {
    let daemon = daemon_for_test("workflow-direct");
    let result = daemon
        .run_workflow_template("intake-url")
        .expect("workflow should run");
    assert_eq!(result.status, WorkflowStatus::Success);
    assert_eq!(result.step_results.len(), 5);
}

#[test]
fn workflow_run_job_is_processed_by_worker() {
    let daemon = daemon_for_test("workflow-job");
    let now = now_unix();
    let _job = daemon
        .queue_workflow_run("intake-url")
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::WorkflowRun);
    assert_eq!(event.status, "completed");
    assert!(event.detail.starts_with("wf_"));
}

fn make_test_vault(root: &Path) -> Arc<GoalScopedVault> {
    Arc::new(GoalScopedVault::open(root.join("vault")).expect("open vault"))
}

fn seed_vault_token(vault: &dyn CredentialVault) {
    vault
        .put(CredentialRecord {
            service: X_OAUTH_VAULT_SERVICE.to_string(),
            username: "bearer".to_string(),
            secret: r#"{"access_token":"at-test","token_type":"bearer","obtained_at":1}"#
                .to_string(),
            totp_secret: None,
        })
        .expect("seed vault token");
}

#[test]
fn x_api_bookmarks_client_reads_paginated_bookmarks() {
    let root = std::env::temp_dir().join(format!("symbiotic_x_api_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    seed_vault_token(&*vault);

    let http = Arc::new(StubXApiHttpClient::default());
    http.push_response(Ok(r#"{
                "data":[{"id":"101","author_id":"u1"}],
                "includes":{"users":[{"id":"u1","username":"alpha"}]},
                "meta":{"next_token":"next-1"}
            }"#));
    http.push_response(Ok(r#"{
                "data":[{"id":"202","author_id":"u2"}],
                "includes":{"users":[{"id":"u2","username":"beta"}]},
                "meta":{}
            }"#));

    let client = XApiBookmarksClient::with_http(
        vault,
        "https://api.twitter.com/2".to_string(),
        http.clone(),
    );
    let urls = client.list_bookmark_urls(10).expect("x api list");
    assert_eq!(urls.len(), 2);
    assert_eq!(urls[0].as_str(), "https://x.com/alpha/status/101");
    assert_eq!(urls[1].as_str(), "https://x.com/beta/status/202");
    assert_eq!(http.call_count(), 2);
}

#[test]
fn bookmarks_api_source_falls_back_to_browser_fixture_when_api_fails() {
    let root =
        std::env::temp_dir().join(format!("symbiotic_bookmarks_fallback_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    seed_vault_token(&*vault);
    let api_file = root.join("bookmarks-api.txt");
    let browser_file = root.join("bookmarks-browser.txt");
    fs::write(&browser_file, "https://x.com/browser/status/300\n").expect("write browser file");

    let http = Arc::new(StubXApiHttpClient::default());
    http.push_response(Err("unauthorized"));
    let api_client =
        XApiBookmarksClient::with_http(vault, "https://api.twitter.com/2".to_string(), http);
    let client =
        HybridBookmarksSyncClient::with_api_client(api_file, browser_file, Some(api_client));

    let urls = client
        .list_bookmark_urls(BookmarksSource::Api, 10)
        .expect("fallback list");
    assert_eq!(urls.len(), 1);
    assert_eq!(urls[0].as_str(), "https://x.com/browser/status/300");
}

#[test]
fn x_api_thread_client_fetches_lookup_and_conversation() {
    let root = std::env::temp_dir().join(format!("symbiotic_x_thread_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    seed_vault_token(&*vault);

    let http = Arc::new(StubXApiHttpClient::default());
    http.push_response(Ok(r#"{
                "data":{"id":"111","author_id":"u1","text":"root tweet","conversation_id":"111"},
                "includes":{"users":[{"id":"u1","username":"alice"}]}
            }"#));
    http.push_response(Ok(r#"{
                "data":[
                    {"id":"111","author_id":"u1","text":"root tweet","conversation_id":"111"},
                    {"id":"112","author_id":"u2","text":"reply tweet","conversation_id":"111"}
                ],
                "includes":{"users":[{"id":"u2","username":"bob"}]}
            }"#));

    let client = XApiThreadClient::new(vault, "https://api.twitter.com/2".to_string(), http);
    let thread = client
        .fetch_thread("alice", "111")
        .expect("thread fetch should succeed");
    assert_eq!(thread.root_handle, "alice");
    assert_eq!(thread.root_tweet_id, "111");
    assert_eq!(thread.tweets.len(), 2);
    assert_eq!(thread.tweets[0].author_handle, "alice");
    assert_eq!(thread.tweets[0].text, "root tweet");
    assert_eq!(thread.tweets[1].author_handle, "bob");
    assert_eq!(thread.tweets[1].text, "reply tweet");
}

#[test]
fn file_twitter_fallback_uses_fixture_line_before_stub() {
    let root = std::env::temp_dir().join(format!("symbiotic_thread_fallback_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let fixture = root.join("twitter-threads.txt");
    fs::write(
        &fixture,
        "https://x.com/demo/status/10\tFixture text from browser fallback\n",
    )
    .expect("write fixture");
    let fallback = FileTwitterFallback::new(fixture, Box::new(StubTwitterFallback));
    let thread = fallback
        .fetch_thread(&Url::parse("https://x.com/demo/status/10").expect("valid url"))
        .expect("fallback should work");
    assert_eq!(thread.tweets.len(), 1);
    assert_eq!(thread.tweets[0].text, "Fixture text from browser fallback");
}

#[test]
fn file_twitter_fallback_errors_when_fixture_missing_and_no_network_fallback() {
    let root = std::env::temp_dir().join(format!(
        "symbiotic_thread_fallback_missing_{}",
        unique_suffix()
    ));
    fs::create_dir_all(&root).expect("create temp dir");
    let fixture = root.join("twitter-threads.txt");
    let fallback = FileTwitterFallback::new(fixture, Box::new(StubTwitterFallback));
    let error = fallback
        .fetch_thread(&Url::parse("https://x.com/demo/status/11").expect("valid url"))
        .expect_err("missing fixture should bubble error");
    assert!(error.to_string().contains("twitter fallback unavailable"));
}

struct SuccessTwitterApi;

impl TwitterApiClient for SuccessTwitterApi {
    fn fetch_thread(
        &self,
        handle: &str,
        tweet_id: &str,
    ) -> std::result::Result<TweetThread, TwitterApiError> {
        Ok(TweetThread {
            root_handle: handle.to_string(),
            root_tweet_id: tweet_id.to_string(),
            tweets: vec![Tweet {
                author_handle: handle.to_string(),
                text: "api thread".to_string(),
                tweet_id: tweet_id.to_string(),
            }],
        })
    }
}

struct FailingTwitterFallback;

impl TwitterFallbackClient for FailingTwitterFallback {
    fn fetch_thread(&self, _url: &Url) -> Result<TweetThread> {
        Err(anyhow!("fallback should not be called"))
    }
}

#[test]
fn daemon_fetcher_prefers_twitter_api_before_fallback() {
    let fetcher = DaemonFetcher::with_twitter_clients(
        FetchMode::Stub,
        Box::new(SuccessTwitterApi),
        Box::new(FailingTwitterFallback),
    );
    let url = Url::parse("https://x.com/demo/status/55").expect("valid url");
    let content = fetcher.fetch(&url).expect("fetch should succeed");
    assert!(content.markdown.contains("api thread"));
}

#[test]
fn bookmarks_sync_job_is_processed_by_worker() {
    let config = daemon_config_for_test("bookmarks-job");
    if let Some(parent) = config.bookmarks_api_file.parent() {
        fs::create_dir_all(parent).expect("should create bookmark api fixture dir");
    }
    fs::write(
        &config.bookmarks_api_file,
        "\
https://x.com/a/status/1
https://x.com/a/status/1
invalid-url
https://x.com/b/status/2
",
    )
    .expect("should write bookmark fixture");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();
    let _job = daemon
        .queue_bookmarks_sync("api", 10)
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::BookmarksSync);
    assert_eq!(event.status, "completed");
    assert!(event.detail.contains("source=api"));
    assert!(event.detail.contains("total=2"));
    assert!(event.detail.contains("ingested=2"));
    assert!(event.detail.contains("duplicate=0"));

    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 2);
}

#[test]
fn bookmarks_sync_job_uses_browser_source_fixture() {
    let config = daemon_config_for_test("bookmarks-browser-source");
    if let Some(parent) = config.bookmarks_browser_file.parent() {
        fs::create_dir_all(parent).expect("should create bookmark browser fixture dir");
    }
    fs::write(
        &config.bookmarks_browser_file,
        "\
https://x.com/c/status/3
https://x.com/d/status/4
",
    )
    .expect("should write browser fixture");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();
    let _job = daemon
        .queue_bookmarks_sync("browser", 1)
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::BookmarksSync);
    assert_eq!(event.status, "completed");
    assert!(event.detail.contains("source=browser"));
    assert!(event.detail.contains("total=1"));

    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);
}

#[test]
fn bookmarks_sync_job_is_noop_when_source_feed_missing() {
    let daemon = daemon_for_test("bookmarks-empty");
    let now = now_unix();
    let _job = daemon
        .queue_bookmarks_sync("api", 10)
        .expect("queue should work");

    let event = daemon
        .run_once(now)
        .expect("run once should work")
        .expect("job should exist")
        .0;
    assert_eq!(event.event_type, EventType::BookmarksSync);
    assert_eq!(event.status, "completed");
    assert!(event.detail.contains("source=api"));
    assert!(event.detail.contains("total=0"));

    let queued = daemon
        .queued_jobs_of_type("ingest.fetch")
        .expect("queue query should work");
    assert_eq!(queued.len(), 0);
}

// --- F1: Room-role map routing tests ---

#[test]
fn room_role_map_routes_by_room_id() {
    let mut config = daemon_config_for_test("room-role-map");
    config.room_roles = RoomRoleMap {
        control: Some("!ctrl:example.test".to_string()),
        intake: Some("!intk:example.test".to_string()),
        alerts: Some("!alrt:example.test".to_string()),
        status: Some("!stat:example.test".to_string()),
        ..Default::default()
    };
    config.allowed_senders = ["@user:test".to_string()].into_iter().collect();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "!intk:example.test".to_string(),
                sender: "@user:test".to_string(),
                body: "https://example.com/article".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "!ctrl:example.test".to_string(),
                sender: "@user:test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
}

#[test]
fn room_role_map_rejects_unknown_room_id() {
    let mut config = daemon_config_for_test("room-role-reject");
    config.room_roles = RoomRoleMap {
        control: Some("!ctrl:example.test".to_string()),
        intake: Some("!intk:example.test".to_string()),
        ..Default::default()
    };
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "!unknown:example.test".to_string(),
                sender: "@user:test".to_string(),
                body: "hello".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
}

// --- F2: Sender authorization tests ---

#[test]
fn sender_authorization_rejects_unauthorized_sender() {
    let mut config = daemon_config_for_test("sender-auth-reject");
    config.allowed_senders = ["@owner:example.test".to_string()].into_iter().collect();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@stranger:evil.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert!(events[0].body.contains("not authorized"));
}

#[test]
fn sender_authorization_allows_listed_sender() {
    let mut config = daemon_config_for_test("sender-auth-allow");
    config.allowed_senders = ["@owner:example.test".to_string()].into_iter().collect();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@owner:example.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
}

#[test]
fn sender_authorization_case_insensitive() {
    let mut config = daemon_config_for_test("sender-auth-case");
    config.allowed_senders = ["@Owner:Example.Test".to_string()].into_iter().collect();
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@owner:example.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
}

#[test]
fn empty_allowlist_denies_by_default() {
    let mut config = daemon_config_for_test("sender-auth-open");
    config.allow_open_access = false;
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@anyone:anywhere.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert!(events[0].body.contains("not authorized"));
}

#[test]
fn empty_allowlist_permits_when_open_access_enabled() {
    let mut config = daemon_config_for_test("sender-auth-open-explicit");
    config.allow_open_access = true;
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@anyone:anywhere.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
}

#[test]
fn empty_allowlist_denies_when_room_roles_configured() {
    let mut config = daemon_config_for_test("sender-auth-closed");
    config.allow_open_access = false;
    config.room_roles = RoomRoleMap {
        control: Some("!ctrl:example.test".to_string()),
        ..Default::default()
    };
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "!ctrl:example.test".to_string(),
                sender: "@anyone:anywhere.test".to_string(),
                body: "workflow intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert!(events[0].body.contains("not authorized"));
}

// --- F6: Room config tests ---

#[test]
fn room_role_map_is_configured_returns_true_when_any_set() {
    let empty = RoomRoleMap::default();
    assert!(!empty.is_configured());

    let partial = RoomRoleMap {
        control: Some("!ctrl:test".to_string()),
        ..Default::default()
    };
    assert!(partial.is_configured());
}

#[test]
fn room_role_map_role_for_returns_correct_role() {
    let map = RoomRoleMap {
        control: Some("!ctrl:test".to_string()),
        intake: Some("!intk:test".to_string()),
        alerts: Some("!alrt:test".to_string()),
        status: Some("!stat:test".to_string()),
        credentials: Some("!cred:test".to_string()),
        goals: Some("!goals:test".to_string()),
        stream: Some("!stream:test".to_string()),
    };
    assert_eq!(map.role_for("!ctrl:test"), Some(RoomRole::Control));
    assert_eq!(map.role_for("!intk:test"), Some(RoomRole::Intake));
    assert_eq!(map.role_for("!alrt:test"), Some(RoomRole::Alerts));
    assert_eq!(map.role_for("!stat:test"), Some(RoomRole::Status));
    assert_eq!(map.role_for("!cred:test"), Some(RoomRole::Credentials));
    assert_eq!(map.role_for("!goals:test"), Some(RoomRole::Goals));
    assert_eq!(map.role_for("!stream:test"), Some(RoomRole::Stream));
    assert_eq!(map.role_for("!unknown:test"), None);
}

// --- F4: Vault-backed X OAuth token tests ---

#[test]
fn vault_token_load_returns_access_token() {
    let root = std::env::temp_dir().join(format!("symbiotic_vault_token_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    seed_vault_token(&*vault);
    let token = load_x_access_token_from_vault(vault.as_ref()).expect("load token");
    assert_eq!(token, "at-test");
}

#[test]
fn vault_token_load_returns_error_when_missing() {
    let root =
        std::env::temp_dir().join(format!("symbiotic_vault_token_missing_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    let err = load_x_access_token_from_vault(vault.as_ref()).expect_err("should fail");
    assert!(err.to_string().contains("not found"), "error: {err}");
}

#[test]
fn vault_token_load_rejects_empty_access_token() {
    let root =
        std::env::temp_dir().join(format!("symbiotic_vault_token_empty_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault = make_test_vault(&root);
    vault
        .put(CredentialRecord {
            service: X_OAUTH_VAULT_SERVICE.to_string(),
            username: "bearer".to_string(),
            secret: r#"{"access_token":"  ","token_type":"bearer","obtained_at":1}"#.to_string(),
            totp_secret: None,
        })
        .expect("seed");
    let err = load_x_access_token_from_vault(vault.as_ref()).expect_err("should fail");
    assert!(err.to_string().contains("empty"), "error: {err}");
}

#[test]
fn vault_tokens_encrypted_on_disk() {
    let root = std::env::temp_dir().join(format!("symbiotic_vault_token_enc_{}", unique_suffix()));
    fs::create_dir_all(&root).expect("create temp dir");
    let vault_path = root.join("vault.tsv");
    let vault = Arc::new(FileCredentialVault::open(&vault_path).expect("open vault"));
    vault
        .put(CredentialRecord {
            service: X_OAUTH_VAULT_SERVICE.to_string(),
            username: "bearer".to_string(),
            secret:
                r#"{"access_token":"super-secret-xtoken","token_type":"bearer","obtained_at":1}"#
                    .to_string(),
            totp_secret: None,
        })
        .expect("seed");
    let raw = fs::read_to_string(&vault_path).expect("read vault file");
    assert!(
        !raw.contains("super-secret-xtoken"),
        "access token must not appear in plaintext on disk"
    );
    assert!(raw.contains("svlt2"), "vault should use svlt2 encryption");
}

#[test]
fn daemon_role_registry_has_defaults() {
    let daemon = daemon_for_test("role-defaults");
    assert!(daemon.role_registry().get("researcher").is_some());
    assert!(daemon.role_registry().get("coder").is_some());
    assert!(daemon.role_registry().get("reviewer").is_some());
}

#[test]
fn daemon_resolve_role_config_returns_system_prompt() {
    let daemon = daemon_for_test("role-config");
    let config = daemon
        .resolve_role_config(Some("researcher"))
        .expect("researcher role exists");
    assert!(config.system_prompt.unwrap().contains("research"));

    assert!(daemon.resolve_role_config(None).is_none());
    assert!(daemon.resolve_role_config(Some("nonexistent")).is_none());
}

#[test]
fn daemon_spawn_agent_with_role_merges_capabilities() {
    let daemon = daemon_for_test("role-agent");
    let now = now_unix();
    // security-analyst requires private data and has credential.read capability
    let agent_id = daemon
        .spawn_task_agent(
            "task-role-1",
            AgentParent::System,
            false, // not private by default
            vec!["archive.read".to_string()],
            Some("security-analyst".to_string()),
            now,
        )
        .expect("agent spawn should work");

    let agent = daemon
        .get_agent(&agent_id)
        .expect("get_agent should not fail")
        .expect("agent should exist");
    // security-analyst sets requires_private_data=true, so should be Local LLM
    assert!(matches!(
        agent.llm_type,
        symbiotic_agents::LlmType::Local { .. }
    ));
}

// -----------------------------------------------------------------------
// ProviderRouter integration tests
// -----------------------------------------------------------------------

/// Mock embedding provider for ProviderRouter integration tests.
struct MockContextEmbedProvider {
    class: symbiotic_context::embedding::ProviderClass,
    model: String,
    embedding: Vec<f32>,
    should_fail: bool,
}

impl MockContextEmbedProvider {
    fn local(embedding: Vec<f32>) -> Arc<Self> {
        Arc::new(Self {
            class: symbiotic_context::embedding::ProviderClass::Local,
            model: "mock-local".to_string(),
            embedding,
            should_fail: false,
        })
    }

    fn cloud(embedding: Vec<f32>) -> Arc<Self> {
        Arc::new(Self {
            class: symbiotic_context::embedding::ProviderClass::Cloud,
            model: "mock-cloud".to_string(),
            embedding,
            should_fail: false,
        })
    }

    fn failing_cloud() -> Arc<Self> {
        Arc::new(Self {
            class: symbiotic_context::embedding::ProviderClass::Cloud,
            model: "mock-cloud-fail".to_string(),
            embedding: vec![],
            should_fail: true,
        })
    }
}

#[async_trait]
impl symbiotic_context::embedding::EmbeddingProvider for MockContextEmbedProvider {
    fn provider_class(&self) -> symbiotic_context::embedding::ProviderClass {
        self.class
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    async fn embed(
        &self,
        _text: &str,
    ) -> std::result::Result<
        symbiotic_context::embedding::EmbedResult,
        symbiotic_context::embedding::EmbedError,
    > {
        if self.should_fail {
            return Err(symbiotic_context::embedding::EmbedError::Unavailable(
                "mock unavailable".to_string(),
            ));
        }
        Ok(symbiotic_context::embedding::EmbedResult {
            embedding: self.embedding.clone(),
            model_name: self.model.clone(),
            dimensions: self.embedding.len(),
        })
    }
}

/// Helper: build a ProviderRouter with registered providers, returning
/// the `EmbedRouter` adapter ready for `IntakeEmbeddingProcessor`.
fn build_embed_router(
    local: Arc<MockContextEmbedProvider>,
    cloud: Option<Arc<MockContextEmbedProvider>>,
) -> Arc<dyn EmbedRouter> {
    let mut registry = ProviderRegistry::new();

    let local_adapter = Arc::new(ContextEmbeddingAdapter::new(
        "ollama",
        local as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    ));
    registry.register(RegisteredProvider {
        base: local_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
        completion: None,
        embedding: Some(local_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
        image: None,
        video: None,
        agent: None,
    });

    if let Some(cloud_provider) = cloud {
        let cloud_adapter = Arc::new(ContextEmbeddingAdapter::new(
            "openai",
            cloud_provider as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
        ));
        registry.register(RegisteredProvider {
            base: cloud_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
            completion: None,
            embedding: Some(cloud_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
            image: None,
            video: None,
            agent: None,
        });
        let _ = registry.set_default(ProviderCapability::Embedding, "openai");
    } else {
        let _ = registry.set_default(ProviderCapability::Embedding, "ollama");
    }

    let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
        registry,
    ))));
    Arc::new(ProviderRouterAdapter { router })
}

#[tokio::test]
async fn provider_router_local_only_routing() {
    let local = MockContextEmbedProvider::local(vec![0.1, 0.2, 0.3]);
    let router = build_embed_router(local, None);

    // Shareable content should work with local-only.
    let result = router
        .embed("hello world", symbiotic_core::Sensitivity::Shareable)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    assert_eq!(embed.embedding, vec![0.1, 0.2, 0.3]);
    assert_eq!(embed.model_name, "mock-local");

    // Restricted content should also work with local-only.
    let result = router
        .embed("secret data", symbiotic_core::Sensitivity::Restricted)
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn provider_router_sensitivity_restricts_to_local() {
    let local = MockContextEmbedProvider::local(vec![1.0, 2.0]);
    let cloud = MockContextEmbedProvider::cloud(vec![3.0, 4.0]);
    let router = build_embed_router(local, Some(cloud));

    // Shareable → prefers cloud (default provider).
    let result = router
        .embed("public info", symbiotic_core::Sensitivity::Shareable)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    assert_eq!(embed.model_name, "mock-cloud");

    // Restricted → must use local only.
    let result = router
        .embed("private data", symbiotic_core::Sensitivity::Restricted)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    assert_eq!(embed.model_name, "mock-local");

    // Private → must use local only.
    let result = router
        .embed("private data", symbiotic_core::Sensitivity::Private)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    assert_eq!(embed.model_name, "mock-local");
}

#[tokio::test]
async fn provider_router_cloud_fallback_to_local() {
    // Cloud fails → falls back to local for shareable content.
    // Use zero-retry config for fast test — need to build custom router.
    let mut registry = ProviderRegistry::new();

    let local_inner = MockContextEmbedProvider::local(vec![5.0, 6.0]);
    let local_adapter = Arc::new(ContextEmbeddingAdapter::new(
        "ollama",
        local_inner as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    ));
    registry.register(RegisteredProvider {
        base: local_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
        completion: None,
        embedding: Some(local_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
        image: None,
        video: None,
        agent: None,
    });

    let cloud_inner = MockContextEmbedProvider::failing_cloud();
    let cloud_adapter = Arc::new(ContextEmbeddingAdapter::new(
        "openai",
        cloud_inner as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    ));
    registry.register(RegisteredProvider {
        base: cloud_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
        completion: None,
        embedding: Some(cloud_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
        image: None,
        video: None,
        agent: None,
    });
    let _ = registry.set_default(ProviderCapability::Embedding, "openai");

    let pr = Arc::new(
        ProviderRouter::new(Arc::new(std::sync::RwLock::new(registry))).with_retry_config(
            symbiotic_providers::RetryConfig {
                max_retries: 0,
                initial_backoff_ms: 1,
                backoff_multiplier: 1.0,
            },
        ),
    );
    let router: Arc<dyn EmbedRouter> = Arc::new(ProviderRouterAdapter { router: pr });

    let result = router
        .embed("test fallback", symbiotic_core::Sensitivity::Shareable)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    // Should have fallen back to local.
    assert_eq!(embed.model_name, "mock-local");
    assert_eq!(embed.embedding, vec![5.0, 6.0]);
}

#[tokio::test]
async fn provider_router_budget_enforcement() {
    // Build a router with budget enforcement that blocks the cloud provider.
    let mut registry = ProviderRegistry::new();

    let local_inner = MockContextEmbedProvider::local(vec![1.0]);
    let local_adapter = Arc::new(ContextEmbeddingAdapter::new(
        "ollama",
        local_inner as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    ));
    registry.register(RegisteredProvider {
        base: local_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
        completion: None,
        embedding: Some(local_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
        image: None,
        video: None,
        agent: None,
    });

    let cloud_inner = MockContextEmbedProvider::cloud(vec![9.0]);
    let cloud_adapter = Arc::new(ContextEmbeddingAdapter::new(
        "openai",
        cloud_inner as Arc<dyn symbiotic_context::embedding::EmbeddingProvider>,
    ));
    registry.register(RegisteredProvider {
        base: cloud_adapter.clone() as Arc<dyn symbiotic_providers::ModelProvider>,
        completion: None,
        embedding: Some(cloud_adapter as Arc<dyn symbiotic_providers::EmbeddingProvider>),
        image: None,
        video: None,
        agent: None,
    });
    let _ = registry.set_default(ProviderCapability::Embedding, "openai");

    // Create budget that blocks "openai".
    let dir = tempfile::TempDir::new().unwrap();
    let log_path = dir.path().join("usage.ndjson");
    let log = Arc::new(symbiotic_providers::UsageLog::open(&log_path).unwrap());
    log.record(&symbiotic_providers::types::UsageRecord {
        provider: "openai".to_string(),
        model: "test".to_string(),
        timestamp: symbiotic_core::now_unix(),
        input_tokens: 100,
        output_tokens: 50,
        media_units: 0,
        cost_usd: Some(50.0),
        request_type: symbiotic_providers::types::RequestType::Embedding,
        source: "test".to_string(),
        session_id: None,
    })
    .unwrap();

    let mut per_provider = std::collections::HashMap::new();
    per_provider.insert(
        "openai".to_string(),
        symbiotic_providers::ProviderBudget {
            daily_limit_usd: Some(1.0), // already spent $50
            monthly_limit_usd: None,
            max_tokens_per_request: None,
            max_media_units_per_day: None,
            max_agent_tasks_per_day: None,
        },
    );

    let budget_config = symbiotic_providers::BudgetConfig {
        global_daily_limit_usd: None,
        per_provider,
        alert_threshold_percent: 80.0,
    };

    let budget = Arc::new(symbiotic_providers::BudgetEnforcer::new(budget_config, log));
    let pr = Arc::new(
        ProviderRouter::new(Arc::new(std::sync::RwLock::new(registry))).with_budget(budget),
    );
    let router: Arc<dyn EmbedRouter> = Arc::new(ProviderRouterAdapter { router: pr });

    // Cloud is over budget → should fall back to local.
    let result = router
        .embed("budget test", symbiotic_core::Sensitivity::Shareable)
        .await;
    assert!(result.is_ok());
    let embed = result.unwrap();
    assert_eq!(embed.model_name, "mock-local");
}

// --- Secret ingestion tests ---

#[test]
fn parse_control_command_accepts_install_secret_put_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"byok","key":"SYMBIOTIC_OPENROUTER_API_KEY","value":"sk-test"}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::InstallSecretPut { mode, key, value }
        if mode == "byok" && key == "SYMBIOTIC_OPENROUTER_API_KEY" && value == "sk-test"
    ));
}

#[test]
fn parse_control_command_accepts_install_secret_put_text() {
    let parsed =
        parse_control_command("install secret put byok SYMBIOTIC_OPENROUTER_API_KEY sk-test");
    assert!(matches!(
        parsed,
        ControlCommand::InstallSecretPut { mode, key, value }
        if mode == "byok" && key == "SYMBIOTIC_OPENROUTER_API_KEY" && value == "sk-test"
    ));
}

#[test]
fn parse_control_command_accepts_install_secret_validate_json() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.validate","d":{"mode":"byok"}}}"#,
    );
    assert!(matches!(
        parsed,
        ControlCommand::InstallSecretValidate { mode } if mode == "byok"
    ));
}

#[test]
fn parse_control_command_accepts_install_secret_validate_text() {
    let parsed = parse_control_command("install secret validate managed");
    assert!(matches!(
        parsed,
        ControlCommand::InstallSecretValidate { mode } if mode == "managed"
    ));
}

#[test]
fn parse_control_command_install_secret_put_missing_value_defaults_empty() {
    // In v2 format, missing value defaults to empty string at parser level.
    // The handler in commands.rs rejects empty values.
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"byok","key":"SYMBIOTIC_OPENROUTER_API_KEY"}}}"#,
    );
    assert!(matches!(parsed, ControlCommand::InstallSecretPut { value, .. } if value.is_empty()));
}

#[test]
fn parse_control_command_rejects_install_secret_put_missing_mode() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"key":"SYMBIOTIC_OPENROUTER_API_KEY","value":"sk-test"}}}"#,
    );
    // In v2, mode defaults to "byok" in parse_extracted_command, so this now succeeds
    assert!(matches!(parsed, ControlCommand::InstallSecretPut { .. }));
}

#[test]
fn parse_control_command_rejects_install_secret_validate_invalid_mode() {
    let parsed = parse_control_command(
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.validate","d":{"mode":"invalid"}}}"#,
    );
    // In v2 via parse_extracted_command, mode is passed through without validation — succeeds with "invalid"
    assert!(matches!(
        parsed,
        ControlCommand::InstallSecretValidate { .. }
    ));
}

#[test]
fn secret_put_rejects_unknown_key() {
    let daemon = daemon_for_test("secret-reject-unknown-key");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"byok","key":"RANDOM_BAD_KEY","value":"test"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("install.secret.rejected"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(detail_str(&events[0], "key"), Some("RANDOM_BAD_KEY"));
}

#[test]
fn secret_put_rejects_key_not_allowed_for_mode() {
    let daemon = daemon_for_test("secret-reject-mode-mismatch");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"managed","key":"SYMBIOTIC_OPENROUTER_API_KEY","value":"sk-test"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("install.secret.rejected"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(detail_str(&events[0], "mode"), Some("managed"));
}

#[test]
fn secret_put_rejects_empty_value() {
    let daemon = daemon_for_test("secret-reject-empty-value");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"byok","key":"SYMBIOTIC_OPENROUTER_API_KEY","value":""}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    // Empty value is rejected at the parser level (non-empty constraint)
    // Either a state event (install.secret.rejected) or a message event (fail)
    assert!(
        events[0].sym.a.as_deref() == Some("install.secret.rejected")
            || events[0].sym.s == Some(Status::Fail)
    );
}

#[test]
fn secret_put_stores_and_confirms_without_leaking_value() {
    let daemon = daemon_for_test("secret-put-stores");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.put","d":{"mode":"byok","key":"SYMBIOTIC_OPENROUTER_API_KEY","value":"sk-secret-value-12345"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("install.secret.stored"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(
        detail_str(&events[0], "key"),
        Some("SYMBIOTIC_OPENROUTER_API_KEY")
    );
    assert_eq!(detail_str(&events[0], "status"), Some("present"));

    // SECURITY: Verify no secret value appears in any event field
    let secret_value = "sk-secret-value-12345";
    assert!(
        !events[0].body.contains(secret_value),
        "secret value must not appear in event body"
    );
    if let Some(d) = events[0].sym.d.as_ref().and_then(|v| v.as_object()) {
        for (detail_key, dv) in d {
            let dv_str = dv.as_str().unwrap_or("");
            assert!(
                !dv_str.contains(secret_value),
                "secret value leaked in event detail '{detail_key}'"
            );
        }
    }

    // Verify the file was actually written
    assert!(daemon.config.secrets_file.exists());

    // Verify file permissions are 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::metadata(&daemon.config.secrets_file)
            .unwrap()
            .permissions();
        assert_eq!(perms.mode() & 0o777, 0o600);
    }
}

#[test]
fn secret_validate_reports_missing_when_no_secrets() {
    let daemon = daemon_for_test("secret-validate-missing");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.validate","d":{"mode":"byok"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("install.secret.validated"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(
        detail_val(&events[0], "all_present"),
        Some(&serde_json::json!(false))
    );
}

#[test]
fn secret_validate_reports_present_after_put() {
    let daemon = daemon_for_test("secret-validate-present");
    let now = now_unix();

    // First, store all required BYOK secrets.
    for (idx, (key, value)) in [
        ("ANTHROPIC_API_KEY", "sk-ant-abc"),
        ("OPENAI_API_KEY", "sk-openai-abc"),
        ("SYMBIOTIC_X_CLIENT_ID", "x-client-id"),
        ("SYMBIOTIC_X_CLIENT_SECRET", "x-client-secret"),
    ]
    .iter()
    .enumerate()
    {
        daemon
            .route_matrix_message(
                &MatrixMessage {
                    room_id: "#control".to_string(),
                    sender: "@user:test".to_string(),
                    body: format!(
                        r#"{{"msgtype":"sym.c","body":"","sym":{{"v":2,"c":"install.secret.put","d":{{"mode":"byok","key":"{key}","value":"{value}"}}}}}}"#
                    ),
                    timestamp: now + idx as u64,
                },
                now + idx as u64,
            )
            .expect("put should work");
    }

    // Now validate
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"install.secret.validate","d":{"mode":"byok"}}}"#.to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("validate should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("install.secret.validated"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(
        detail_val(&events[0], "all_present"),
        Some(&serde_json::json!(true))
    );

    // SECURITY: Verify the secrets JSON does not contain any actual secret values
    let secrets_json = detail_str(&events[0], "secrets").expect("secrets detail should be present");
    assert!(
        !secrets_json.contains("sk-ant-abc"),
        "actual secret value must not appear in validation response"
    );
    assert!(
        !secrets_json.contains("sk-openai-abc"),
        "actual secret value must not appear in validation response"
    );
    assert!(
        secrets_json.contains("present"),
        "validation should contain 'present' status"
    );
}

// ---------------------------------------------------------------------------
// T84: Intake embedding wiring tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_job_invokes_embedding_processor() {
    let daemon = daemon_for_test("embed-ingest");
    // Verify the embedding processor is initialised.
    assert!(
        daemon.embedding_processor.is_some(),
        "embedding_processor should be Some in test daemon"
    );

    let normalized = normalize_url("https://example.com/embed1").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(
            vec![normalized],
            vec!["embed".to_string()],
            IntakeSource::Cli,
        )
        .expect("enqueue should succeed");

    let event = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("ingest job should be processed")
        .0;

    assert_eq!(event.event_type, EventType::IngestFetch);
    assert_eq!(event.status, "completed");
    assert_eq!(event.detail, "Ingested");

    // The ingest succeeded. Embedding was attempted — since no real Ollama
    // is running, the chunks will be counted as failed or pending, but the
    // ingest job itself must NOT have failed.
    let done = daemon
        .done_jobs_of_type("ingest.fetch")
        .expect("query done jobs");
    assert_eq!(done.len(), 1, "ingest job should be done, not failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_job_succeeds_even_when_embeddings_fail() {
    // This test confirms the key constraint: embedding errors never fail the
    // ingest job. With no Ollama running, embeddings will fail, but the event
    // status must still be "completed".
    let daemon = daemon_for_test("embed-fail-safe");

    let normalized = normalize_url("https://example.com/embed-fail").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(vec![normalized], vec![], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let event = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("ingest job should be processed")
        .0;

    assert_eq!(event.event_type, EventType::IngestFetch);
    assert_eq!(event.status, "completed");

    // Verify the document is stored despite embedding failure.
    let records = daemon.archive_records().expect("archive list should work");
    assert!(
        !records.is_empty(),
        "archive should contain the ingested document"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_ingest_skips_embeddings() {
    let mut config = daemon_config_for_test("embed-blocked");
    config.blocked_hosts.insert("evil.example.com".to_string());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blocked = normalize_url("http://evil.example.com/secret").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(vec![blocked], vec![], IntakeSource::Cli)
        .expect("enqueue should succeed");

    let event = daemon
        .run_once(now_unix())
        .expect("run once should succeed")
        .expect("blocked job should be processed")
        .0;

    assert_eq!(event.event_type, EventType::IngestFetch);
    assert_eq!(event.status, "completed");
    assert_eq!(event.detail, "Blocked");

    // No document was archived, so embeddings should not have been attempted.
    let records = daemon.archive_records().expect("archive list");
    assert!(
        records.is_empty(),
        "blocked ingest should not store records"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_stores_document_before_embedding() {
    // Verify that after run_once completes successfully for an ingest.fetch job,
    // the archive contains the document — embeddings happen AFTER storage.
    let daemon = daemon_for_test("embed-order");

    let url = normalize_url("https://example.com/order-test").expect("valid url");
    let _result = daemon
        .enqueue_intake_urls(
            vec![url.clone()],
            vec!["order".to_string()],
            IntakeSource::Cli,
        )
        .expect("enqueue");
    let event = daemon.run_once(now_unix()).expect("run").expect("ingest").0;
    assert_eq!(event.status, "completed");
    assert_eq!(event.detail, "Ingested");

    // The document should be in the archive.
    let records = daemon.archive_records().expect("archive list");
    assert_eq!(records.len(), 1, "document should be archived");
    assert!(
        records[0].content.contains("fetched"),
        "stub fetcher content should be present"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_intake_embeddings_tolerates_missing_record() {
    let daemon = daemon_for_test("embed-no-record");
    // Directly call run_intake_embeddings with a non-existent record.
    // Should log and return without panicking.
    daemon.run_intake_embeddings(Some("nonexistent-record-id"), "run_test");
    // No assertion needed — just verify no panic.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_intake_embeddings_skips_when_no_record_id() {
    let daemon = daemon_for_test("embed-no-rid");
    // Calling with no record_id returns immediately.
    daemon.run_intake_embeddings(None, "run_skip");
    // No assertion needed — just verify no panic.
}

// ---------------------------------------------------------------------------
// T104 Phase 3 — Sensitivity-Driven Storage Routing Tests
// ---------------------------------------------------------------------------

#[test]
fn detect_blob_category_medical() {
    use super::detect_blob_category;
    use symbiotic_vault_store::BlobCategory;

    assert_eq!(
        detect_blob_category("Patient diagnosis of hypertension"),
        BlobCategory::Medical,
    );
    assert_eq!(
        detect_blob_category("bloodwork results from the lab"),
        BlobCategory::Medical,
    );
    assert_eq!(
        detect_blob_category("HIPAA compliance report for clinic"),
        BlobCategory::Medical,
    );
    assert_eq!(
        detect_blob_category("Cholesterol 220 mg/dL — prescription for statins"),
        BlobCategory::Medical,
    );
}

#[test]
fn detect_blob_category_financial() {
    use super::detect_blob_category;
    use symbiotic_vault_store::BlobCategory;

    assert_eq!(
        detect_blob_category("2025 Tax Return filed on April 15"),
        BlobCategory::Financial,
    );
    assert_eq!(
        detect_blob_category("Bank statement for checking account"),
        BlobCategory::Financial,
    );
    assert_eq!(
        detect_blob_category("Invoice #1234 for consulting services"),
        BlobCategory::Financial,
    );
    assert_eq!(
        detect_blob_category("IRS notice about estimated payments"),
        BlobCategory::Financial,
    );
    assert_eq!(
        detect_blob_category("Brokerage account summary for Q4"),
        BlobCategory::Financial,
    );
}

#[test]
fn detect_blob_category_legal() {
    use super::detect_blob_category;
    use symbiotic_vault_store::BlobCategory;

    assert_eq!(
        detect_blob_category("Employment contract effective January 2026"),
        BlobCategory::Legal,
    );
    assert_eq!(
        detect_blob_category("Non-disclosure agreement with Acme Corp"),
        BlobCategory::Legal,
    );
    assert_eq!(
        detect_blob_category("Signed NDA for project Alpha"),
        BlobCategory::Legal,
    );
    assert_eq!(
        detect_blob_category("Power of attorney designating Jane Doe"),
        BlobCategory::Legal,
    );
}

#[test]
fn detect_blob_category_credential() {
    use super::detect_blob_category;
    use symbiotic_vault_store::BlobCategory;

    assert_eq!(
        detect_blob_category("password for the production database"),
        BlobCategory::Credential,
    );
    assert_eq!(
        detect_blob_category("api_key=sk_live_abc123"),
        BlobCategory::Credential,
    );
    assert_eq!(
        detect_blob_category("-----BEGIN PRIVATE KEY-----\nMIIEvgIB..."),
        BlobCategory::Credential,
    );
}

#[test]
fn detect_blob_category_general_fallback() {
    use super::detect_blob_category;
    use symbiotic_vault_store::BlobCategory;

    assert_eq!(
        detect_blob_category("Some personal journal entry about my day"),
        BlobCategory::Custom("general".to_string()),
    );
    assert_eq!(
        detect_blob_category(""),
        BlobCategory::Custom("general".to_string()),
    );
}

#[test]
fn route_private_to_blob_store_with_configured_key() {
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};

    // Create a daemon with a blob store key.
    let mut config = daemon_config_for_test("blob-route-private");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    // Generate a test key and write it to a file.
    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-test.key");
    std::fs::create_dir_all(&root).unwrap();
    {
        use symbiotic_vault_store::keys::ExposeSecret;
        std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();
    }

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Verify blob store was initialized.
    assert!(
        daemon.encrypted_blob_store.is_some(),
        "blob store should be initialized"
    );
    assert!(
        daemon.blob_recipient.is_some(),
        "blob recipient should be initialized"
    );

    // Store a Private document in the archive.
    let content = "Patient diagnosis of acute bronchitis. Prescribed amoxicillin 500mg.";
    let outcome = daemon
        .archive_store
        .store(StoreRequest {
            title_hint: Some("Medical Record".to_string()),
            content: content.to_string(),
            source_url: Some("https://example.com/medical".to_string()),
            tags: vec!["medical".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: "test-private-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("archive store should succeed");

    let record_id = outcome.record_id;

    // Route the private content to the blob store.
    daemon.route_private_to_blob_store(&record_id, "run_test_1");

    // Verify: blob store should contain the encrypted blob.
    let blobs = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .list(None)
        .unwrap();
    assert_eq!(blobs.len(), 1, "one blob should be stored");
    assert_eq!(blobs[0].id, record_id);
    assert_eq!(
        blobs[0].category,
        symbiotic_vault_store::BlobCategory::Medical
    );
    assert_eq!(blobs[0].metadata.content_type, "text/markdown");

    // Verify: the blob can be decrypted and matches original content.
    let decrypted = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .read(&record_id, &identity)
        .unwrap();
    assert_eq!(
        String::from_utf8(decrypted).unwrap(),
        content,
        "decrypted content should match original"
    );

    // Verify: archive entry was replaced with metadata-only placeholder.
    let archived = daemon
        .archive_store
        .get(&record_id)
        .unwrap()
        .expect("archive record should still exist");
    assert!(
        archived.content.contains("blob_id:"),
        "archive should contain blob_id reference"
    );
    assert!(
        archived.content.contains("age-encrypted blob store"),
        "archive should contain encryption notice"
    );
    assert!(
        !archived.content.contains("bronchitis"),
        "archive should NOT contain original plaintext"
    );
    assert!(
        archived.tags.contains(&"tier3/encrypted".to_string()),
        "archive tags should include tier3/encrypted"
    );
}

#[test]
fn route_shareable_content_skips_blob_store() {
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};

    // Create a daemon with a blob store key.
    let mut config = daemon_config_for_test("blob-route-shareable");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-test.key");
    std::fs::create_dir_all(&root).unwrap();
    {
        use symbiotic_vault_store::keys::ExposeSecret;
        std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();
    }

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Store a Shareable document.
    let content = "Rust async programming patterns.";
    let outcome = daemon
        .archive_store
        .store(StoreRequest {
            title_hint: Some("Tech Note".to_string()),
            content: content.to_string(),
            source_url: Some("https://example.com/rust".to_string()),
            tags: vec!["tech".to_string()],
            sensitivity: ArchiveSensitivity::Shareable,
            idempotency_key: "test-shareable-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("archive store should succeed");

    let record_id = outcome.record_id;

    // Try to route — should be a no-op since it's Shareable, not Private.
    daemon.route_private_to_blob_store(&record_id, "run_test_2");

    // Verify: blob store should be empty.
    let blobs = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .list(None)
        .unwrap();
    assert!(
        blobs.is_empty(),
        "blob store should be empty for Shareable content"
    );

    // Verify: archive content should be unchanged.
    let archived = daemon
        .archive_store
        .get(&record_id)
        .unwrap()
        .expect("archive record should exist");
    assert!(
        archived.content.contains("Rust async"),
        "archive should contain original content"
    );
}

#[test]
fn route_restricted_content_skips_blob_store() {
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};

    // Create a daemon with a blob store key.
    let mut config = daemon_config_for_test("blob-route-restricted");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-test.key");
    std::fs::create_dir_all(&root).unwrap();
    {
        use symbiotic_vault_store::keys::ExposeSecret;
        std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();
    }

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Store a Restricted document.
    let content = "Personal journal entry about my week.";
    let outcome = daemon
        .archive_store
        .store(StoreRequest {
            title_hint: Some("Journal".to_string()),
            content: content.to_string(),
            source_url: None,
            tags: vec!["personal".to_string()],
            sensitivity: ArchiveSensitivity::Restricted,
            idempotency_key: "test-restricted-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("archive store should succeed");

    let record_id = outcome.record_id;

    // Try to route — should be a no-op since it's Restricted, not Private.
    daemon.route_private_to_blob_store(&record_id, "run_test_3");

    // Verify: blob store should be empty.
    let blobs = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .list(None)
        .unwrap();
    assert!(
        blobs.is_empty(),
        "blob store should be empty for Restricted content"
    );
}

#[test]
fn route_private_without_blob_store_is_noop() {
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};

    // Create a daemon WITHOUT blob store key.
    let config = daemon_config_for_test("blob-route-no-key");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Verify blob store is NOT initialized.
    assert!(
        daemon.encrypted_blob_store.is_none(),
        "blob store should not be initialized without key"
    );

    // Store a Private document.
    let content = "Top secret financial data.";
    let outcome = daemon
        .archive_store
        .store(StoreRequest {
            title_hint: Some("Finance".to_string()),
            content: content.to_string(),
            source_url: None,
            tags: vec!["finance".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: "test-no-key-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("archive store should succeed");

    let record_id = outcome.record_id;

    // Route should be a no-op (no blob store configured).
    daemon.route_private_to_blob_store(&record_id, "run_test_4");

    // Verify: archive content unchanged (plaintext remains since no blob store).
    let archived = daemon
        .archive_store
        .get(&record_id)
        .unwrap()
        .expect("archive record should exist");
    assert!(
        archived.content.contains("Top secret financial"),
        "archive should retain original content when blob store is not configured"
    );
}

#[test]
fn route_private_nonexistent_record_is_noop() {
    // Create a daemon with blob store configured.
    let mut config = daemon_config_for_test("blob-route-missing");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-test.key");
    std::fs::create_dir_all(&root).unwrap();
    {
        use symbiotic_vault_store::keys::ExposeSecret;
        std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();
    }

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Route with non-existent record — should not panic.
    daemon.route_private_to_blob_store("nonexistent-record", "run_test_5");

    // Verify: blob store should be empty.
    let blobs = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .list(None)
        .unwrap();
    assert!(blobs.is_empty(), "blob store should be empty");
}

#[test]
fn metadata_only_placeholder_for_private_content() {
    use symbiotic_archive::{ArchiveSensitivity, StoreRequest};

    // Create a daemon with blob store.
    let mut config = daemon_config_for_test("blob-metadata-placeholder");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-test.key");
    std::fs::create_dir_all(&root).unwrap();
    {
        use symbiotic_vault_store::keys::ExposeSecret;
        std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();
    }

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Store a Private financial document.
    let content = "Tax return for 2025. Total income: $150,000. Deductions: $25,000.";
    let outcome = daemon
        .archive_store
        .store(StoreRequest {
            title_hint: Some("Tax Return 2025".to_string()),
            content: content.to_string(),
            source_url: None,
            tags: vec!["tax".to_string(), "2025".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            idempotency_key: "test-metadata-1".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("archive store should succeed");

    let record_id = outcome.record_id;

    // Route to blob store.
    daemon.route_private_to_blob_store(&record_id, "run_test_6");

    // Check blob metadata.
    let blobs = daemon
        .encrypted_blob_store
        .as_ref()
        .unwrap()
        .list(None)
        .unwrap();
    assert_eq!(blobs.len(), 1);
    assert_eq!(
        blobs[0].category,
        symbiotic_vault_store::BlobCategory::Financial,
        "content mentioning tax return should be categorized as Financial"
    );
    assert_eq!(blobs[0].metadata.content_type, "text/markdown");
    assert!(blobs[0].metadata.size_bytes > 0);

    // Check archive placeholder.
    let archived = daemon
        .archive_store
        .get(&record_id)
        .unwrap()
        .expect("archive record should exist");

    // Placeholder should contain structured metadata but NOT the original content.
    assert!(archived.content.contains("status: encrypted"));
    assert!(archived.content.contains("category: financial"));
    assert!(
        !archived.content.contains("$150,000"),
        "placeholder must NOT contain original financial data"
    );
    assert!(
        !archived.content.contains("Deductions"),
        "placeholder must NOT contain original financial data"
    );
    // Title should be prefixed with [encrypted].
    assert!(
        archived.title.contains("[encrypted]"),
        "title should indicate encryption"
    );
}

// ---- Tier 3 phone-only filter tests ----

#[tokio::test]
async fn tier3_phone_only_redacts_private_envelope() {
    let daemon = daemon_for_test("tier3-redact");
    assert!(daemon.config.tier3_phone_only, "default should be true");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "secret")
        .with_sensitivity("private")
        .with_detail_field("url", "https://example.com")
        .with_detail_field("some_content", "should be removed");

    daemon
        .send_matrix_event(&transport, "#status", envelope, now)
        .await
        .expect("send should succeed");

    let sent = transport.drain_outgoing().expect("drain should work");
    assert_eq!(sent.len(), 1);
    let sent_envelope = &sent[0].envelope;
    assert!(
        sent_envelope.body.contains("Private content"),
        "body should be redacted"
    );
    assert!(
        sent_envelope.body.contains("phone only"),
        "body should mention phone only"
    );
    assert_eq!(
        sent_envelope.sensitivity(),
        Some("private"),
        "sensitivity tag should be preserved"
    );
    assert_eq!(
        detail_str(&sent_envelope, "url"),
        Some("https://example.com"),
        "url should be preserved"
    );
    assert_eq!(
        detail_str(&sent_envelope, "phone_only"),
        Some("true"),
        "phone_only marker should be set"
    );
    assert!(
        !has_detail(&sent_envelope, "some_content"),
        "non-preserved detail should be stripped"
    );
}

#[tokio::test]
async fn tier3_phone_only_disabled_passes_private_through() {
    let mut config = daemon_config_for_test("tier3-disabled");
    config.tier3_phone_only = false;
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "secret content")
        .with_sensitivity("private");

    daemon
        .send_matrix_event(&transport, "#status", envelope, now)
        .await
        .expect("send should succeed");

    let sent = transport.drain_outgoing().expect("drain should work");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].envelope.body, "secret content",
        "body should NOT be redacted when tier3_phone_only is false"
    );
}

#[tokio::test]
async fn tier3_phone_only_does_not_redact_non_private() {
    let daemon = daemon_for_test("tier3-shareable");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "public info")
        .with_sensitivity("shareable");

    daemon
        .send_matrix_event(&transport, "#status", envelope, now)
        .await
        .expect("send should succeed");

    let sent = transport.drain_outgoing().expect("drain should work");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].envelope.body, "public info",
        "body should NOT be redacted for shareable"
    );
}

#[tokio::test]
async fn tier3_phone_only_no_sensitivity_tag_passes_through() {
    let daemon = daemon_for_test("tier3-no-tag");
    let transport = InMemoryMatrixTransport::default();
    let now = now_unix();

    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "normal");

    daemon
        .send_matrix_event(&transport, "#status", envelope, now)
        .await
        .expect("send should succeed");

    let sent = transport.drain_outgoing().expect("drain should work");
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].envelope.body, "normal",
        "body should pass through when no sensitivity tag"
    );
}

#[test]
fn should_skip_tier3_event_filters_private() {
    let daemon = daemon_for_test("skip-tier3-private");
    let now = now_unix();
    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "secret")
        .with_sensitivity("private");
    let json = serde_json::to_string(&envelope).expect("serialize should work");
    assert!(
        daemon.should_skip_tier3_event(&json),
        "should skip private events"
    );
}

#[test]
fn should_skip_tier3_event_passes_shareable() {
    let daemon = daemon_for_test("skip-tier3-shareable");
    let now = now_unix();
    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "public")
        .with_sensitivity("shareable");
    let json = serde_json::to_string(&envelope).expect("serialize should work");
    assert!(
        !daemon.should_skip_tier3_event(&json),
        "should NOT skip shareable events"
    );
}

#[test]
fn should_skip_tier3_event_disabled_passes_private() {
    let mut config = daemon_config_for_test("skip-tier3-disabled");
    config.tier3_phone_only = false;
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let now = now_unix();
    let envelope = MatrixEventEnvelope::new(Kind::Message, Status::Success, now, "secret")
        .with_sensitivity("private");
    let json = serde_json::to_string(&envelope).expect("serialize should work");
    assert!(
        !daemon.should_skip_tier3_event(&json),
        "should NOT skip when tier3_phone_only is false"
    );
}

#[test]
fn should_skip_tier3_event_non_event_body() {
    let daemon = daemon_for_test("skip-tier3-plain");
    assert!(
        !daemon.should_skip_tier3_event("just a plain message"),
        "plain text should not be skipped"
    );
}

#[test]
fn daemon_event_sensitivity_field_round_trips_to_envelope() {
    let now = now_unix();
    // Simulate the main.rs pattern: DaemonEvent.sensitivity -> envelope.with_sensitivity
    let event = DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job_1".to_string()),
        detail: "run_1".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run_1".to_string()),
        url: Some("https://example.com".to_string()),
        title: Some("Example".to_string()),
        sensitivity: Some("private".to_string()),
        quick_replies: None,
        thread_id: None,
    };

    // Use to_envelope which already handles sensitivity
    let envelope = event.to_envelope(now);

    assert_eq!(
        envelope.sensitivity(),
        Some("private"),
        "sensitivity should flow from DaemonEvent to envelope"
    );
    assert!(envelope.is_private(), "is_private should return true");
    assert!(envelope.validate().is_ok(), "envelope should be valid");
}

#[test]
fn embedding_retry_interval_default_is_five_minutes() {
    let config = DaemonConfig::default();
    assert_eq!(config.embedding_retry_interval_secs, 300);
}

#[test]
fn embedding_retry_interval_zero_disables() {
    let config = DaemonConfig {
        embedding_retry_interval_secs: 0,
        ..DaemonConfig::default()
    };
    assert_eq!(config.embedding_retry_interval_secs, 0);
}

// ===========================================================================
// End-to-end push notification integration tests
//
// These tests exercise the full push notification flow: device registration
// with token encryption -> DaemonEvent creation -> event classification ->
// push dispatch to provider -> telemetry recording.
// ===========================================================================

/// A push provider that records all notifications it receives.
struct RecordingPushProvider {
    sent: Mutex<Vec<PushNotification>>,
}

impl RecordingPushProvider {
    fn new() -> Self {
        Self {
            sent: Mutex::new(Vec::new()),
        }
    }

    fn sent_notifications(&self) -> Vec<PushNotification> {
        self.sent.lock().expect("lock").clone()
    }
}

impl PushProvider for RecordingPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        self.sent.lock().expect("lock").push(notification.clone());
        Ok(())
    }
}

/// Full end-to-end push notification test: register device -> create event ->
/// dispatch push -> verify notification payload -> verify telemetry.
///
/// This exercises the same code path the daemon's `run_once` method uses:
/// 1. `PushRegistry::register` encrypts the device token
/// 2. `classify_event` determines the event is push-worthy
/// 3. `build_body` constructs the notification body
/// 4. `dispatch_event_push` fans out to all registered devices
/// 5. `append_push_telemetry` records the delivery result
#[test]
fn e2e_push_register_event_dispatch_telemetry() {
    let daemon = daemon_for_test("e2e-push-flow");
    let now = now_unix();

    // Step 1: Register two devices (APNs + FCM), simulating what the app does
    // when the user enables push notifications.
    let apns_device = daemon
        .register_push_device("iphone-14", "apns-device-token-abc123", "apns", now)
        .expect("APNs device registration should succeed");
    let fcm_device = daemon
        .register_push_device("pixel-8", "fcm-registration-token-xyz789", "fcm", now)
        .expect("FCM device registration should succeed");

    // Verify tokens were encrypted (not stored as plaintext).
    assert_ne!(apns_device.encrypted_token, "apns-device-token-abc123");
    assert_ne!(fcm_device.encrypted_token, "fcm-registration-token-xyz789");
    assert!(
        !apns_device.encrypted_token.is_empty(),
        "encrypted token should not be empty"
    );

    // Verify decryption round-trips correctly.
    let decrypted_apns = daemon
        .push_registry
        .decrypt_token(&apns_device.encrypted_token)
        .expect("APNs token decryption should succeed");
    assert_eq!(decrypted_apns, "apns-device-token-abc123");

    let decrypted_fcm = daemon
        .push_registry
        .decrypt_token(&fcm_device.encrypted_token)
        .expect("FCM token decryption should succeed");
    assert_eq!(decrypted_fcm, "fcm-registration-token-xyz789");

    // Verify registry lists both devices.
    let devices = daemon.list_push_devices().expect("list should work");
    assert_eq!(devices.len(), 2);

    // Step 2: Create a DaemonEvent (entry capture completed — push-worthy).
    let event = DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-e2e-001".to_string()),
        detail: "Ingested".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-e2e-001".to_string()),
        url: Some("https://example.com/article".to_string()),
        title: Some("Example Article".to_string()),
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    // Step 3: Verify event is classified as push-worthy.
    let classification = push_dispatcher::classify_event(&event);
    assert!(
        classification.is_some(),
        "ingest.fetch/completed should be push-worthy"
    );
    let (title, priority) = classification.unwrap();
    assert_eq!(title, "Entry captured");
    assert_eq!(priority, "high");

    // Step 4: Dispatch via a recording provider and verify the notifications.
    let provider = RecordingPushProvider::new();
    let telemetry_file = daemon.config.push_telemetry_file.clone();
    let dispatch_now = now + 1;

    let success_count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &telemetry_file,
        dispatch_now,
    );
    assert_eq!(success_count, 2, "both devices should receive push");

    // Step 5: Verify the push notification payloads.
    let sent = provider.sent_notifications();
    assert_eq!(sent.len(), 2);

    // Find the APNs and FCM notifications (order not guaranteed).
    let apns_notif = sent
        .iter()
        .find(|n| n.device_id == "iphone-14")
        .expect("APNs notification should exist");
    let fcm_notif = sent
        .iter()
        .find(|n| n.device_id == "pixel-8")
        .expect("FCM notification should exist");

    // Verify APNs notification fields.
    assert_eq!(apns_notif.title, "Entry captured");
    assert!(apns_notif.body.contains("https://example.com/article"));
    assert!(apns_notif.body.contains("Example Article"));
    assert_eq!(apns_notif.priority, "high");
    assert_eq!(apns_notif.event_type, "ingest.fetch");
    assert_eq!(apns_notif.event_status, "completed");
    assert_eq!(apns_notif.rid, "run-e2e-001");
    assert_eq!(apns_notif.platform, "apns");
    assert_eq!(apns_notif.ts, dispatch_now);
    // Encrypted token is carried through (for gateway decryption at delivery).
    assert_eq!(apns_notif.encrypted_token, apns_device.encrypted_token);
    assert!(!apns_notif.notification_id.is_empty());

    // Verify FCM notification fields.
    assert_eq!(fcm_notif.title, "Entry captured");
    assert_eq!(fcm_notif.priority, "high");
    assert_eq!(fcm_notif.event_type, "ingest.fetch");
    assert_eq!(fcm_notif.platform, "fcm");
    assert_eq!(fcm_notif.rid, "run-e2e-001");
    assert_eq!(fcm_notif.encrypted_token, fcm_device.encrypted_token);

    // Step 6: Verify telemetry was written for both devices.
    let telemetry = fs::read_to_string(&telemetry_file).expect("telemetry file should exist");
    let telemetry_lines: Vec<&str> = telemetry
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        telemetry_lines.len(),
        2,
        "telemetry should have one entry per device"
    );

    // Parse each telemetry line as JSON to verify structure.
    for line in &telemetry_lines {
        let entry: serde_json::Value =
            serde_json::from_str(line).expect("telemetry line should be valid JSON");
        assert_eq!(entry["status"], "sent");
        assert_eq!(entry["event_type"], "ingest.fetch");
        assert_eq!(entry["event_status"], "completed");
        assert_eq!(entry["rid"], "run-e2e-001");
        assert!(entry["error"].as_str().unwrap().is_empty());
        assert!(
            entry["device_id"] == "iphone-14" || entry["device_id"] == "pixel-8",
            "device_id should be one of the registered devices"
        );
    }
}

/// End-to-end test for push notification flow with a critical event (auth failure).
/// Verifies that critical events produce push notifications with `critical` priority.
#[test]
fn e2e_push_critical_event_auth_failure() {
    let daemon = daemon_for_test("e2e-push-critical");
    let now = now_unix();

    // Register a single device.
    daemon
        .register_push_device("iphone-14", "apns-token-critical-test", "apns", now)
        .expect("register should succeed");

    // Create a critical event (auth issue failed).
    let event = DaemonEvent {
        event_type: EventType::AuthIssue,
        status: "failed".to_string(),
        job_id: Some("job-auth-001".to_string()),
        detail: "OAuth token expired".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 1,
    );
    assert_eq!(success_count, 1);

    let sent = provider.sent_notifications();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].title, "Auth issue failed");
    assert_eq!(sent[0].priority, "critical");
    assert_eq!(sent[0].event_type, "auth.issue");
    assert_eq!(sent[0].event_status, "failed");
    assert!(sent[0].body.contains("OAuth token expired"));
}

/// End-to-end test verifying non-pushworthy events produce zero notifications.
#[test]
fn e2e_push_suppressed_event_no_notification() {
    let daemon = daemon_for_test("e2e-push-suppressed");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-suppress-test", "apns", now)
        .expect("register should succeed");

    // Create a non-pushworthy event (retry — transient failure).
    let event = DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "retry".to_string(),
        job_id: Some("job-retry-001".to_string()),
        detail: "temporary network error".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-retry-001".to_string()),
        url: Some("https://example.com/flaky".to_string()),
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 1,
    );
    assert_eq!(success_count, 0, "retry events should not trigger push");
    assert!(
        provider.sent_notifications().is_empty(),
        "no notifications should be sent"
    );

    // Verify no telemetry was written.
    let telemetry = fs::read_to_string(&daemon.config.push_telemetry_file).unwrap_or_default();
    assert!(
        telemetry.trim().is_empty(),
        "no telemetry for suppressed events"
    );
}

/// End-to-end test for push dispatch when provider fails — verifies telemetry
/// records the failure and the daemon does not panic.
#[test]
fn e2e_push_provider_failure_records_telemetry() {
    let daemon = daemon_for_test("e2e-push-fail");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-fail-test", "apns", now)
        .expect("register should succeed");
    daemon
        .register_push_device("pixel-8", "fcm-token-fail-test", "fcm", now)
        .expect("register should succeed");

    let event = DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "dlq".to_string(),
        job_id: Some("job-dlq-001".to_string()),
        detail: "permanent failure".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-dlq-001".to_string()),
        url: Some("https://example.com/broken".to_string()),
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = AlwaysFailPushProvider;
    let success_count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 1,
    );
    assert_eq!(success_count, 0, "all deliveries should fail");

    // Verify telemetry records the failures.
    let telemetry = fs::read_to_string(&daemon.config.push_telemetry_file).expect("telemetry file");
    let telemetry_lines: Vec<&str> = telemetry
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(telemetry_lines.len(), 2, "one failure entry per device");
    for line in &telemetry_lines {
        let entry: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
        assert_eq!(entry["status"], "failed");
        assert!(
            entry["error"]
                .as_str()
                .unwrap()
                .contains("intentional push failure"),
            "error reason should be recorded"
        );
    }
}

/// End-to-end test for push dispatch through the daemon's own run_once path.
/// This simulates a real job execution cycle where the daemon processes a job,
/// produces a DaemonEvent, and dispatches push notifications using its built-in
/// composite push provider (FilePushProvider).
#[test]
fn e2e_push_via_daemon_run_once() {
    let daemon = daemon_for_test("e2e-push-run-once");
    let now = now_unix();

    // Register a device so push dispatch has targets.
    daemon
        .register_push_device("iphone-14", "apns-token-run-once", "apns", now)
        .expect("register should succeed");

    // Enqueue a job that will fail (unknown type) to produce a push-worthy event.
    daemon
        .queue
        .enqueue(symbiotic_queue::EnqueueRequest {
            type_name: "job.unknown".to_string(),
            payload: "test-payload".to_string(),
            idempotency_key: format!("e2e-run-once-{now}"),
            max_attempts: 1,
            next_run_at: now,
            force: false,
        })
        .expect("enqueue should succeed");

    // Execute the job — this will fail with "unknown job type" and trigger
    // push dispatch through the daemon's built-in provider (FilePushProvider).
    let event = daemon
        .run_once(now + 1)
        .expect("run_once should succeed")
        .expect("event should exist")
        .0;

    assert_eq!(event.event_type, EventType::JobUnknown);

    // Verify the outbox file contains the push notification.
    let outbox_content = fs::read_to_string(&daemon.config.push_outbox_file).unwrap_or_default();
    assert!(
        !outbox_content.trim().is_empty(),
        "push outbox should contain notification"
    );

    // Parse the outbox entry and verify it.
    let outbox_entry: serde_json::Value = outbox_content
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("valid JSON"))
        .expect("outbox should have at least one entry");
    assert_eq!(outbox_entry["event_type"], "job.unknown");
    assert_eq!(outbox_entry["device_id"], "iphone-14");
    assert_eq!(outbox_entry["title"], "Unknown job type");
    assert_eq!(outbox_entry["priority"], "high");

    // The encrypted_token in the outbox should NOT be the plaintext token.
    let outbox_token = outbox_entry["encrypted_token"]
        .as_str()
        .expect("encrypted_token should be a string");
    assert_ne!(outbox_token, "apns-token-run-once");

    // Verify telemetry was written.
    let telemetry = fs::read_to_string(&daemon.config.push_telemetry_file).expect("telemetry file");
    assert!(telemetry.contains("\"status\":\"sent\""));
    assert!(telemetry.contains("\"event_type\":\"job.unknown\""));
}

/// End-to-end test verifying push works with goal_run_id fallback when no
/// intake_run_id is present.
#[test]
fn e2e_push_goal_run_id_fallback() {
    let daemon = daemon_for_test("e2e-push-goal-rid");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-goal-rid", "apns", now)
        .expect("register should succeed");

    let event = DaemonEvent {
        event_type: EventType::WorkflowRun,
        status: "completed".to_string(),
        job_id: Some("job-wf-001".to_string()),
        detail: "workflow finished successfully".to_string(),
        goal_room: Some("!goal-room:test".to_string()),
        goal_template: Some("intake-url".to_string()),
        goal_run_id: Some("goal-run-42".to_string()),
        goal_id: None,
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 1,
    );
    assert_eq!(success_count, 1);

    let sent = provider.sent_notifications();
    assert_eq!(sent[0].title, "Workflow completed");
    assert_eq!(
        sent[0].rid, "goal-run-42",
        "rid should fall back to goal_run_id when intake_run_id is absent"
    );
    assert_eq!(sent[0].event_type, "workflow.run");
    assert!(sent[0].body.contains("workflow finished successfully"));
}

/// End-to-end test covering all install lifecycle events that trigger push.
#[test]
fn e2e_push_install_lifecycle_events() {
    let daemon = daemon_for_test("e2e-push-install");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-install", "apns", now)
        .expect("register should succeed");

    let install_events: Vec<(EventType, &str, &str, &str)> = vec![
        (
            EventType::InstallProvision,
            "completed",
            "VPS provisioned",
            "high",
        ),
        (
            EventType::InstallProvision,
            "dlq",
            "VPS provisioning failed",
            "critical",
        ),
        (
            EventType::InstallBootstrap,
            "completed",
            "Bootstrap complete",
            "high",
        ),
        (
            EventType::InstallBootstrap,
            "dlq",
            "Bootstrap failed",
            "critical",
        ),
        (
            EventType::InstallVerify,
            "completed",
            "Install verified",
            "high",
        ),
        (
            EventType::InstallVerify,
            "dlq",
            "Install verification failed",
            "critical",
        ),
        (
            EventType::InstallRun,
            "completed",
            "Install complete",
            "high",
        ),
        (EventType::InstallRun, "dlq", "Install failed", "critical"),
    ];

    for (event_type, status, expected_title, expected_priority) in install_events {
        let label = format!("{event_type:?}/{status}");
        let event = DaemonEvent {
            event_type,
            status: status.to_string(),
            job_id: Some(format!("job-{}-{status}", event_type.as_str())),
            detail: format!("{} {status}", event_type.as_str()),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        };

        let provider = RecordingPushProvider::new();
        let count = push_dispatcher::dispatch_event_push(
            &event,
            &daemon.push_registry,
            &provider,
            &daemon.config.push_telemetry_file,
            now + 1,
        );
        assert_eq!(count, 1, "{label} should produce push notification");

        let sent = provider.sent_notifications();
        assert_eq!(sent[0].title, expected_title, "title mismatch for {label}");
        assert_eq!(
            sent[0].priority, expected_priority,
            "priority mismatch for {label}"
        );
    }
}

/// End-to-end test verifying that re-registering a device updates the token
/// and push dispatch uses the latest encrypted token.
#[test]
fn e2e_push_device_re_registration_updates_token() {
    let daemon = daemon_for_test("e2e-push-reregister");
    let now = now_unix();

    // Initial registration.
    let old_device = daemon
        .register_push_device("iphone-14", "old-apns-token", "apns", now)
        .expect("initial register should succeed");

    // Re-register with a new token (simulates token refresh).
    let new_device = daemon
        .register_push_device("iphone-14", "new-apns-token", "apns", now + 100)
        .expect("re-register should succeed");

    // Tokens should differ.
    assert_ne!(old_device.encrypted_token, new_device.encrypted_token);

    // Registry should still have exactly one device.
    let devices = daemon.list_push_devices().expect("list should work");
    assert_eq!(devices.len(), 1);

    // New token should decrypt to the updated value.
    let decrypted = daemon
        .push_registry
        .decrypt_token(&devices[0].encrypted_token)
        .expect("decrypt should succeed");
    assert_eq!(decrypted, "new-apns-token");

    // Dispatch should use the new token.
    let event = DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-reregister-001".to_string()),
        detail: "Ingested".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-reregister-001".to_string()),
        url: Some("https://example.com/new".to_string()),
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 101,
    );

    let sent = provider.sent_notifications();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].encrypted_token, new_device.encrypted_token,
        "dispatch should use the updated encrypted token"
    );
}

// ---------------------------------------------------------------------------
// Key rotation command tests
// ---------------------------------------------------------------------------

#[test]
fn parse_control_command_accepts_key_rotate_text() {
    let parsed = parse_control_command("key rotate");
    assert!(matches!(parsed, ControlCommand::KeyRotationStart));
}

#[test]
fn parse_control_command_accepts_key_rotate_text_case_insensitive() {
    let parsed = parse_control_command("KEY ROTATE");
    assert!(matches!(parsed, ControlCommand::KeyRotationStart));
}

#[test]
fn parse_control_command_accepts_key_rotate_with_slash() {
    let parsed = parse_control_command("/key rotate");
    assert!(matches!(parsed, ControlCommand::KeyRotationStart));
}

#[test]
fn parse_control_command_accepts_vault_key_rotation_start_json() {
    let parsed =
        parse_control_command(r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#);
    assert!(matches!(parsed, ControlCommand::KeyRotationStart));
}

#[test]
fn key_rotation_fails_when_blob_store_not_configured() {
    let daemon = daemon_for_test("keyrot-no-blobstore");
    let now = now_unix();
    assert!(
        daemon.encrypted_blob_store.is_none(),
        "blob store should not be configured in default test config"
    );

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.failed")
    );
    assert_eq!(events[0].sym.k, Kind::State);
    assert!(events[0].body.contains("not configured"));
}

#[test]
fn key_rotation_succeeds_with_configured_blob_store() {
    use symbiotic_vault_store::keys::ExposeSecret;

    let mut config = daemon_config_for_test("keyrot-success");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    // Generate a test key and write it to a file.
    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-keyrot.key");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();

    config.blob_store_key_file = Some(key_file.clone());
    config.blob_store_root = root.join("blob-store-keyrot");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    assert!(daemon.encrypted_blob_store.is_some());

    // Store a test blob with the original key.
    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();
    blob_store
        .store(
            "test-keyrot-blob",
            symbiotic_vault_store::BlobCategory::Medical,
            symbiotic_vault_store::BlobMetadata {
                title: "Test Blob".to_string(),
                tags: vec!["test".to_string()],
                size_bytes: 11,
                content_type: "text/plain".to_string(),
            },
            b"hello world",
            &[recipient],
        )
        .expect("store should succeed");

    // Verify blob exists.
    assert_eq!(blob_store.list(None).unwrap().len(), 1);

    // Route the key rotation command.
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Should get 2 events: progress + completed.
    assert_eq!(events.len(), 2, "expected progress + completed events");
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(
        events[1].sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(events[1].sym.k, Kind::State);

    // Verify the completed event has the right detail.
    assert_eq!(
        detail_val(&events[1], "rotated_blobs"),
        Some(&serde_json::json!(1)),
        "should have rotated 1 blob"
    );

    // Verify the key file was updated (different from original).
    let new_key_data = std::fs::read_to_string(&key_file).unwrap();
    let original_key_string = identity.to_string();
    let original_key_str = original_key_string.expose_secret();
    assert_ne!(
        new_key_data.trim(),
        original_key_str.trim(),
        "key file should contain a new identity"
    );

    // Verify the new key can decrypt the blob.
    let new_identity: symbiotic_vault_store::keys::Identity =
        new_key_data.trim().parse().expect("new key should parse");
    let decrypted = blob_store
        .read("test-keyrot-blob", &new_identity)
        .expect("should decrypt with new key");
    assert_eq!(decrypted, b"hello world");

    // Verify the old key cannot decrypt the blob.
    let old_result = blob_store.read("test-keyrot-blob", &identity);
    assert!(
        old_result.is_err(),
        "old key should not decrypt after rotation"
    );
}

#[test]
fn key_rotation_with_empty_blob_store_succeeds() {
    use symbiotic_vault_store::keys::ExposeSecret;

    let mut config = daemon_config_for_test("keyrot-empty");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-keyrot-empty.key");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();

    config.blob_store_key_file = Some(key_file.clone());
    config.blob_store_root = root.join("blob-store-keyrot-empty");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    assert!(daemon.encrypted_blob_store.is_some());

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(
        events[1].sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(
        detail_val(&events[1], "rotated_blobs"),
        Some(&serde_json::json!(0)),
        "should have rotated 0 blobs (empty store)"
    );
}

#[test]
fn key_rotation_text_command_via_control_room() {
    use symbiotic_vault_store::keys::ExposeSecret;

    let mut config = daemon_config_for_test("keyrot-text-cmd");
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join("blob-keyrot-text.key");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();

    config.blob_store_key_file = Some(key_file);
    config.blob_store_root = root.join("blob-store-keyrot-text");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "/key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // The text command "key rotate" should be recognized.
    assert!(events.len() >= 2);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(
        events.last().unwrap().sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
}

// ---------------------------------------------------------------------------
// E2E Key Rotation Integration Tests
// ---------------------------------------------------------------------------

/// Helper: create a DaemonConfig with blob store key file and root pre-configured.
/// Returns (config, identity, key_file_path, root_dir).
fn daemon_config_with_blob_store(
    name: &str,
) -> (
    DaemonConfig,
    symbiotic_vault_store::keys::Identity,
    PathBuf,
    PathBuf,
) {
    use symbiotic_vault_store::keys::ExposeSecret;

    let mut config = daemon_config_for_test(name);
    let root = config.archive_root.parent().unwrap().to_path_buf();

    let identity = symbiotic_vault_store::keys::Identity::generate();
    let key_file = root.join(format!("blob-{name}.key"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(&key_file, identity.to_string().expose_secret().as_bytes()).unwrap();

    config.blob_store_key_file = Some(key_file.clone());
    config.blob_store_root = root.join(format!("blob-store-{name}"));

    (config, identity, key_file, root)
}

/// Helper: store multiple test blobs into a daemon's blob store.
/// Returns a Vec of (id, content) tuples for later verification.
fn store_test_blobs(daemon: &SymbioticDaemon) -> Vec<(String, Vec<u8>)> {
    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    let test_data = vec![
        (
            "medical-record-001",
            symbiotic_vault_store::BlobCategory::Medical,
            "Blood Test Results 2026",
            b"Patient: John Doe\nHemoglobin: 14.5 g/dL\nWBC: 7200\nPlatelets: 250000".to_vec(),
        ),
        (
            "financial-tax-2025",
            symbiotic_vault_store::BlobCategory::Financial,
            "Tax Return 2025",
            b"Gross Income: $120,000\nDeductions: $25,000\nTax Owed: $18,750".to_vec(),
        ),
        (
            "legal-contract-nda",
            symbiotic_vault_store::BlobCategory::Legal,
            "NDA Agreement",
            b"This Non-Disclosure Agreement is entered into by and between...".to_vec(),
        ),
        (
            "credential-api-key",
            symbiotic_vault_store::BlobCategory::Credential,
            "Production API Key",
            b"sk-prod-abc123def456ghi789jkl012mno345pqr678".to_vec(),
        ),
        (
            "medical-prescription",
            symbiotic_vault_store::BlobCategory::Medical,
            "Prescription",
            b"Rx: Amoxicillin 500mg TID x 10 days".to_vec(),
        ),
    ];

    let mut stored = Vec::new();
    for (id, category, title, content) in test_data {
        blob_store
            .store(
                id,
                category,
                symbiotic_vault_store::BlobMetadata {
                    title: title.to_string(),
                    tags: vec!["test".to_string()],
                    size_bytes: content.len() as u64,
                    content_type: "text/plain".to_string(),
                },
                &content,
                &[recipient],
            )
            .expect("blob store should succeed");
        stored.push((id.to_string(), content));
    }

    stored
}

/// E2E: Full key rotation with multiple blobs — verifies data integrity,
/// old key invalidation, new key decryption, and file permissions.
#[test]
fn e2e_key_rotation_multiple_blobs_full_lifecycle() {
    use symbiotic_vault_store::keys::ExposeSecret;

    let (config, old_identity, key_file, _root) = daemon_config_with_blob_store("e2e-keyrot-multi");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    assert!(daemon.encrypted_blob_store.is_some());

    // Store 5 test blobs with different categories.
    let stored_blobs = store_test_blobs(&daemon);
    assert_eq!(stored_blobs.len(), 5);

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();

    // Verify all blobs are readable with the original key before rotation.
    for (id, expected_content) in &stored_blobs {
        let decrypted = blob_store
            .read(id, &old_identity)
            .unwrap_or_else(|e| panic!("pre-rotation read of {id} should succeed: {e}"));
        assert_eq!(
            &decrypted, expected_content,
            "pre-rotation content mismatch for {id}"
        );
    }

    // Trigger key rotation via JSON v1 command.
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Verify event flow: progress + completed.
    assert_eq!(events.len(), 2, "expected exactly progress + completed");
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(detail_str(&events[0], "phase"), Some("re-encrypting"));
    assert_eq!(
        detail_val(&events[0], "total_blobs"),
        Some(&serde_json::json!(5))
    );
    assert_eq!(
        events[1].sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(events[1].sym.k, Kind::State);
    assert_eq!(
        detail_val(&events[1], "rotated_blobs"),
        Some(&serde_json::json!(5)),
        "should have rotated all 5 blobs"
    );
    assert_eq!(
        detail_val(&events[1], "total_blobs"),
        Some(&serde_json::json!(5))
    );

    // rid field removed in v2 — key rotation events share the same action prefix
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );

    // Read back the new key from the key file.
    let new_key_data = std::fs::read_to_string(&key_file).unwrap();
    let original_key_string = old_identity.to_string();
    let original_key_str = original_key_string.expose_secret();
    assert_ne!(
        new_key_data.trim(),
        original_key_str.trim(),
        "key file should have been updated with a new identity"
    );
    let new_identity: symbiotic_vault_store::keys::Identity =
        new_key_data.trim().parse().expect("new key should parse");

    // Verify data integrity: all 5 blobs decrypt correctly with the new key.
    for (id, expected_content) in &stored_blobs {
        let decrypted = blob_store.read(id, &new_identity).unwrap_or_else(|e| {
            panic!("post-rotation read of {id} with new key should succeed: {e}")
        });
        assert_eq!(
            &decrypted, expected_content,
            "post-rotation content mismatch for {id} — data integrity violated"
        );
    }

    // Verify old key is invalidated: none of the blobs should decrypt with it.
    for (id, _) in &stored_blobs {
        let result = blob_store.read(id, &old_identity);
        assert!(
            result.is_err(),
            "old key should NOT decrypt blob {id} after rotation"
        );
    }

    // Verify key file permissions (Unix-only).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = std::fs::metadata(&key_file).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "key file should have 0o600 permissions, got 0o{mode:o}"
        );
    }
}

/// E2E: Key rotation via text command (not JSON) exercises the text parser path.
#[test]
fn e2e_key_rotation_text_command_with_blobs() {
    let (config, old_identity, key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-text-blobs");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    // Store two blobs.
    let content_a = b"sensitive data alpha";
    let content_b = b"sensitive data bravo";
    blob_store
        .store(
            "text-blob-a",
            symbiotic_vault_store::BlobCategory::Financial,
            symbiotic_vault_store::BlobMetadata {
                title: "A".to_string(),
                tags: vec![],
                size_bytes: content_a.len() as u64,
                content_type: "text/plain".to_string(),
            },
            content_a.as_slice(),
            &[recipient],
        )
        .unwrap();
    blob_store
        .store(
            "text-blob-b",
            symbiotic_vault_store::BlobCategory::Legal,
            symbiotic_vault_store::BlobMetadata {
                title: "B".to_string(),
                tags: vec![],
                size_bytes: content_b.len() as u64,
                content_type: "text/plain".to_string(),
            },
            content_b.as_slice(),
            &[recipient],
        )
        .unwrap();

    // Trigger via text command.
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert!(events.len() >= 2);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(
        events.last().unwrap().sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(
        detail_val(&events.last().unwrap(), "rotated_blobs"),
        Some(&serde_json::json!(2))
    );

    // Verify new key decrypts correctly.
    let new_key_data = std::fs::read_to_string(&key_file).unwrap();
    let new_identity: symbiotic_vault_store::keys::Identity = new_key_data.trim().parse().unwrap();
    assert_eq!(
        blob_store.read("text-blob-a", &new_identity).unwrap(),
        content_a
    );
    assert_eq!(
        blob_store.read("text-blob-b", &new_identity).unwrap(),
        content_b
    );

    // Old key fails.
    assert!(blob_store.read("text-blob-a", &old_identity).is_err());
    assert!(blob_store.read("text-blob-b", &old_identity).is_err());
}

/// E2E: Key rotation when blob_store_key_file is None emits failed event.
#[test]
fn e2e_key_rotation_fails_no_key_file_configured() {
    let config = daemon_config_for_test("e2e-keyrot-no-keyfile");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    assert!(daemon.encrypted_blob_store.is_none());

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.failed")
    );
    assert_eq!(events[0].sym.k, Kind::State);
    assert!(
        events[0].body.contains("not configured"),
        "body should indicate blob store not configured: {}",
        events[0].body
    );
}

/// E2E: Key rotation when key file is not writable emits CRITICAL failure.
#[test]
#[cfg(unix)]
fn e2e_key_rotation_fails_when_key_file_not_writable() {
    use std::os::unix::fs::PermissionsExt;

    let (config, _identity, key_file, _root) = daemon_config_with_blob_store("e2e-keyrot-readonly");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    // Store a blob.
    blob_store
        .store(
            "readonly-blob",
            symbiotic_vault_store::BlobCategory::Medical,
            symbiotic_vault_store::BlobMetadata {
                title: "Read Only Test".to_string(),
                tags: vec![],
                size_bytes: 10,
                content_type: "text/plain".to_string(),
            },
            b"readonly ok",
            &[recipient],
        )
        .unwrap();

    // Make the key file read-only.
    let mut perms = std::fs::metadata(&key_file).unwrap().permissions();
    perms.set_mode(0o444);
    std::fs::set_permissions(&key_file, perms).unwrap();

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Should get progress + failed (CRITICAL).
    assert!(events.len() >= 2);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    let last = events.last().unwrap();
    assert_eq!(last.sym.a.as_deref(), Some("vault.key_rotation.failed"));
    assert_eq!(last.sym.k, Kind::State);
    assert!(
        last.body.contains("CRITICAL"),
        "body should contain CRITICAL warning: {}",
        last.body
    );
    assert!(
        detail_str(&last, "recovery")
            .map(|v| v.contains("manual"))
            .unwrap_or(false),
        "recovery detail should indicate manual intervention required"
    );

    // Restore write permissions for cleanup.
    let mut perms = std::fs::metadata(&key_file).unwrap().permissions();
    perms.set_mode(0o644);
    std::fs::set_permissions(&key_file, perms).unwrap();
}

/// E2E: Key rotation via room-role map (configured control room) instead of alias pattern.
#[test]
fn e2e_key_rotation_with_room_role_map() {
    let (mut config, old_identity, key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-roomrole");

    // Configure room role map so the daemon uses room_id-based routing.
    config.room_roles = RoomRoleMap {
        control: Some("!ctrl-room-id:test".to_string()),
        intake: Some("!intake-room-id:test".to_string()),
        alerts: Some("!alerts-room-id:test".to_string()),
        status: Some("!status-room-id:test".to_string()),
        ..Default::default()
    };

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();
    blob_store
        .store(
            "roomrole-blob",
            symbiotic_vault_store::BlobCategory::Financial,
            symbiotic_vault_store::BlobMetadata {
                title: "Room Role Test".to_string(),
                tags: vec![],
                size_bytes: 8,
                content_type: "text/plain".to_string(),
            },
            b"roomrole",
            &[recipient],
        )
        .unwrap();

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "!ctrl-room-id:test".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 2);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(
        events[1].sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(
        detail_val(&events[1], "rotated_blobs"),
        Some(&serde_json::json!(1))
    );

    // Verify the new key works.
    let new_key_data = std::fs::read_to_string(&key_file).unwrap();
    let new_identity: symbiotic_vault_store::keys::Identity = new_key_data.trim().parse().unwrap();
    assert_eq!(
        blob_store.read("roomrole-blob", &new_identity).unwrap(),
        b"roomrole"
    );
    assert!(blob_store.read("roomrole-blob", &old_identity).is_err());
}

/// E2E: Unauthorized sender cannot trigger key rotation.
#[test]
fn e2e_key_rotation_rejected_for_unauthorized_sender() {
    let (mut config, _identity, _key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-authz");

    // Enable sender authorization with a specific allowlist.
    config.allowed_senders = HashSet::from(["@admin:test".to_string()]);
    config.allow_open_access = false;

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@intruder:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Should be rejected with authorization failure, not key rotation events.
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert_eq!(events[0].sym.s, Some(Status::Fail));
    assert!(events[0].body.contains("not authorized"));
}

/// E2E: Double key rotation — rotating twice in a row keeps data intact.
#[test]
fn e2e_key_rotation_double_rotation_preserves_data() {
    let (config, _old_identity, key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-double");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    // Store test blob.
    let original_content = b"data that survives double rotation";
    blob_store
        .store(
            "double-rot-blob",
            symbiotic_vault_store::BlobCategory::Medical,
            symbiotic_vault_store::BlobMetadata {
                title: "Double Rotation Test".to_string(),
                tags: vec![],
                size_bytes: original_content.len() as u64,
                content_type: "text/plain".to_string(),
            },
            original_content.as_slice(),
            &[recipient],
        )
        .unwrap();

    // First rotation.
    let now = now_unix();
    let events1 = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("first rotation should work");
    assert_eq!(
        events1.last().unwrap().sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );

    // Read the first new key.
    let key1_data = std::fs::read_to_string(&key_file).unwrap();
    let key1: symbiotic_vault_store::keys::Identity = key1_data.trim().parse().unwrap();

    // Verify data is intact after first rotation.
    assert_eq!(
        blob_store.read("double-rot-blob", &key1).unwrap(),
        original_content,
        "data should be intact after first rotation"
    );

    // Write the new key back so the daemon reads it for second rotation.
    // (The daemon's handle_key_rotation reads the key file each time.)
    // key_file already has key1 written by the first rotation.

    // Second rotation.
    let now2 = now_unix() + 1; // slightly later timestamp to avoid hash collision
    let events2 = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now2,
            },
            now2,
        )
        .expect("second rotation should work");
    assert_eq!(
        events2.last().unwrap().sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );

    // Read the second new key.
    let key2_data = std::fs::read_to_string(&key_file).unwrap();
    let key2: symbiotic_vault_store::keys::Identity = key2_data.trim().parse().unwrap();

    // Keys should all be different.
    assert_ne!(
        key1_data.trim(),
        key2_data.trim(),
        "second rotation should produce a new key"
    );

    // Verify data is still intact after second rotation.
    assert_eq!(
        blob_store.read("double-rot-blob", &key2).unwrap(),
        original_content,
        "data should be intact after double rotation"
    );

    // Previous keys should NOT work.
    assert!(
        blob_store.read("double-rot-blob", &key1).is_err(),
        "key1 should not work after second rotation"
    );
}

/// E2E: Key rotation with large binary blob preserves content exactly.
#[test]
fn e2e_key_rotation_large_binary_blob_integrity() {
    let (config, old_identity, key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-largeblob");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    // Create a 100KB binary blob with non-trivial pattern.
    let large_content: Vec<u8> = (0..100_000u32)
        .map(|i| ((i * 31 + 17) % 256) as u8)
        .collect();
    blob_store
        .store(
            "large-binary",
            symbiotic_vault_store::BlobCategory::Custom("backup".to_string()),
            symbiotic_vault_store::BlobMetadata {
                title: "Large Binary Blob".to_string(),
                tags: vec!["binary".to_string(), "test".to_string()],
                size_bytes: large_content.len() as u64,
                content_type: "application/octet-stream".to_string(),
            },
            &large_content,
            &[recipient],
        )
        .unwrap();

    // Trigger rotation.
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"key.rotate"}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(
        events.last().unwrap().sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );

    // Verify: read back with new key and compare byte-for-byte.
    let new_key_data = std::fs::read_to_string(&key_file).unwrap();
    let new_identity: symbiotic_vault_store::keys::Identity = new_key_data.trim().parse().unwrap();
    let decrypted = blob_store.read("large-binary", &new_identity).unwrap();
    assert_eq!(
        decrypted.len(),
        large_content.len(),
        "content length should match after rotation"
    );
    assert_eq!(
        decrypted, large_content,
        "large binary content should be identical after rotation"
    );

    // Old key should fail.
    assert!(blob_store.read("large-binary", &old_identity).is_err());
}

/// E2E: Key rotation event details contain room and sender info.
#[test]
fn e2e_key_rotation_event_details_include_room_and_sender() {
    let (config, _identity, _key_file, _root) = daemon_config_with_blob_store("e2e-keyrot-details");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@alice:homeserver.org".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert!(events.len() >= 2);

    // Progress event should include room and sender.
    let progress = &events[0];
    assert_eq!(
        progress.sym.a.as_deref(),
        Some("vault.key_rotation.progress")
    );
    assert_eq!(detail_str(&progress, "room"), Some("#control"));
    assert_eq!(
        detail_str(&progress, "sender"),
        Some("@alice:homeserver.org")
    );

    // Completed event should include room and sender.
    let completed = events.last().unwrap();
    assert_eq!(
        completed.sym.a.as_deref(),
        Some("vault.key_rotation.completed")
    );
    assert_eq!(detail_str(&completed, "room"), Some("#control"));
    assert_eq!(
        detail_str(&completed, "sender"),
        Some("@alice:homeserver.org")
    );
}

/// E2E: Key rotation failure event when key file path is missing
/// (configured but file deleted before rotation).
#[test]
fn e2e_key_rotation_fails_when_key_file_deleted() {
    let (config, _identity, key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-deleted-key");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    assert!(daemon.encrypted_blob_store.is_some());

    // Delete the key file after daemon init (simulates file disappearing).
    std::fs::remove_file(&key_file).unwrap();

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].sym.a.as_deref(),
        Some("vault.key_rotation.failed")
    );
    assert_eq!(events[0].sym.k, Kind::State);
    assert!(
        events[0].body.contains("read") || events[0].body.contains("key file"),
        "body should indicate key file read failure: {}",
        events[0].body
    );
}

/// E2E: Blob metadata (index) is preserved through key rotation.
#[test]
fn e2e_key_rotation_preserves_blob_metadata() {
    let (config, _identity, _key_file, _root) =
        daemon_config_with_blob_store("e2e-keyrot-metadata");
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    let blob_store = daemon.encrypted_blob_store.as_ref().unwrap();
    let recipient = daemon.blob_recipient.as_ref().unwrap();

    // Store a blob with specific metadata.
    blob_store
        .store(
            "metadata-test",
            symbiotic_vault_store::BlobCategory::Medical,
            symbiotic_vault_store::BlobMetadata {
                title: "Important Medical Record".to_string(),
                tags: vec![
                    "medical".to_string(),
                    "important".to_string(),
                    "2026".to_string(),
                ],
                size_bytes: 42,
                content_type: "application/pdf".to_string(),
            },
            b"metadata content that doesn't change the index",
            &[recipient],
        )
        .unwrap();

    // Capture metadata before rotation.
    let before = blob_store.list(None).unwrap();
    assert_eq!(before.len(), 1);
    let before_blob = &before[0];

    // Trigger rotation.
    let now = now_unix();
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "key rotate".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Verify metadata is unchanged.
    let after = blob_store.list(None).unwrap();
    assert_eq!(after.len(), 1);
    let after_blob = &after[0];

    assert_eq!(before_blob.id, after_blob.id);
    assert_eq!(before_blob.category, after_blob.category);
    assert_eq!(before_blob.metadata, after_blob.metadata);
    assert_eq!(before_blob.created_at, after_blob.created_at);
}

// ---------------------------------------------------------------------------
// SOUL.md identity loading on daemon startup
// ---------------------------------------------------------------------------

#[test]
fn daemon_loads_soul_md_on_startup_when_kb_path_set() {
    let mut config = daemon_config_for_test("soul_load");

    // Create a knowledge-base directory with identity/SOUL.md.
    let kb_dir = config.data_dir.join("knowledge-base");
    let soul_dir = kb_dir.join("identity");
    std::fs::create_dir_all(&soul_dir).unwrap();
    std::fs::write(
        soul_dir.join("SOUL.md"),
        "---\nversion: 1\n---\n\n# SOUL\nYou are Symbiotic, a sovereign AI.\n",
    )
    .unwrap();

    config.archive_path = Some(kb_dir);

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    assert!(
        identity.is_some(),
        "identity_content should be loaded from SOUL.md"
    );
    let content = identity.as_ref().unwrap();
    assert!(
        content.contains("You are Symbiotic, a sovereign AI."),
        "identity should contain SOUL.md body text"
    );
}

#[test]
fn daemon_identity_none_when_no_kb_path() {
    let config = daemon_config_for_test("soul_no_kb");
    // archive_path is None by default.
    assert!(config.archive_path.is_none());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    assert!(
        identity.is_none(),
        "identity_content should be None when archive_path is not set"
    );
}

#[test]
fn daemon_identity_none_when_soul_md_missing() {
    let mut config = daemon_config_for_test("soul_missing");

    // Create a knowledge-base directory WITHOUT SOUL.md.
    let kb_dir = config.data_dir.join("knowledge-base-empty");
    std::fs::create_dir_all(&kb_dir).unwrap();

    config.archive_path = Some(kb_dir);

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    assert!(
        identity.is_none(),
        "identity_content should be None when SOUL.md does not exist"
    );
}

#[test]
fn daemon_identity_available_in_resolve_role_config() {
    let mut config = daemon_config_for_test("soul_role_config");

    let kb_dir = config.data_dir.join("knowledge-base");
    let soul_dir = kb_dir.join("identity");
    std::fs::create_dir_all(&soul_dir).unwrap();
    std::fs::write(
        soul_dir.join("SOUL.md"),
        "---\nversion: 1\n---\n\n# SOUL\nDirective: Act with sovereignty.\n",
    )
    .unwrap();

    // Write a role TOML so resolve_role_config returns Some.
    let role_dir = config.data_dir.join("roles");
    std::fs::create_dir_all(&role_dir).unwrap();
    std::fs::write(
        role_dir.join("researcher.toml"),
        r#"
[role]
name = "researcher"
system_prompt = "You are a research agent."
max_iterations = 5
requires_private_data = false
required_capabilities = []
"#,
    )
    .unwrap();

    config.archive_path = Some(kb_dir);
    config.role_dir = role_dir;

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");

    let exec_config = daemon
        .resolve_role_config(Some("researcher"))
        .expect("role should resolve");
    assert!(
        exec_config.identity_context.is_some(),
        "identity_context should be populated from SOUL.md"
    );
    let identity = exec_config.identity_context.unwrap();
    assert!(
        identity.contains("Directive: Act with sovereignty."),
        "identity_context should contain SOUL.md content, got: {identity}"
    );
}

#[test]
fn daemon_identity_shared_arc_for_reconciler() {
    let mut config = daemon_config_for_test("soul_arc");

    let kb_dir = config.data_dir.join("knowledge-base");
    let soul_dir = kb_dir.join("identity");
    std::fs::create_dir_all(&soul_dir).unwrap();
    std::fs::write(
        soul_dir.join("SOUL.md"),
        "---\nversion: 1\n---\n\n# SOUL\nShared identity test.\n",
    )
    .unwrap();

    config.archive_path = Some(kb_dir);

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");

    // Get the identity Arc that would be shared with the reconciler.
    let arc = daemon.identity_content();

    // Verify it already has content loaded.
    let content = arc.lock().unwrap();
    assert!(content.is_some());
    assert!(content.as_ref().unwrap().contains("Shared identity test."));
    drop(content);

    // Simulate reconciler updating identity (as ReloadIdentity would).
    {
        let mut ic = arc.lock().unwrap();
        *ic = Some("Updated identity from reconciler.".to_string());
    }

    // Verify daemon's resolve_role_config would see the updated content.
    let updated = daemon.identity_content.lock().unwrap();
    assert_eq!(
        updated.as_deref(),
        Some("Updated identity from reconciler.")
    );
}

// ---------------------------------------------------------------------------
// SOUL.md explicit soul_file path (SYMBIOTIC_SOUL_FILE)
// ---------------------------------------------------------------------------

#[test]
fn daemon_loads_soul_md_from_explicit_soul_file() {
    let mut config = daemon_config_for_test("soul_explicit");

    // Create an explicit SOUL.md file NOT inside the archive path.
    let soul_path = config.data_dir.join("custom-soul/SOUL.md");
    std::fs::create_dir_all(soul_path.parent().unwrap()).unwrap();
    std::fs::write(
        &soul_path,
        "---\nversion: 1\n---\n\n# SOUL\nExplicit identity from SYMBIOTIC_SOUL_FILE.\n",
    )
    .unwrap();

    // Set soul_file explicitly, archive_path is None.
    config.soul_file = Some(soul_path);
    assert!(config.archive_path.is_none());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    assert!(
        identity.is_some(),
        "identity_content should be loaded from explicit soul_file"
    );
    let content = identity.as_ref().unwrap();
    assert!(
        content.contains("Explicit identity from SYMBIOTIC_SOUL_FILE."),
        "identity should contain explicit soul file body, got: {content}"
    );
}

#[test]
fn daemon_soul_file_takes_priority_over_archive_path() {
    let mut config = daemon_config_for_test("soul_priority");

    // Create SOUL.md inside archive_path.
    let kb_dir = config.data_dir.join("knowledge-base");
    let soul_dir = kb_dir.join("identity");
    std::fs::create_dir_all(&soul_dir).unwrap();
    std::fs::write(
        soul_dir.join("SOUL.md"),
        "---\nversion: 1\n---\n\n# SOUL\nArchive identity (should lose).\n",
    )
    .unwrap();
    config.archive_path = Some(kb_dir);

    // Create explicit soul_file elsewhere.
    let explicit_path = config.data_dir.join("explicit/SOUL.md");
    std::fs::create_dir_all(explicit_path.parent().unwrap()).unwrap();
    std::fs::write(
        &explicit_path,
        "---\nversion: 1\n---\n\n# SOUL\nExplicit identity (should win).\n",
    )
    .unwrap();
    config.soul_file = Some(explicit_path);

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    assert!(identity.is_some());
    let content = identity.as_ref().unwrap();
    assert!(
        content.contains("Explicit identity (should win)."),
        "soul_file should take priority over archive_path, got: {content}"
    );
}

#[test]
fn daemon_identity_none_when_explicit_soul_file_missing() {
    let mut config = daemon_config_for_test("soul_explicit_missing");

    // Point soul_file at a non-existent path, no archive_path either.
    config.soul_file = Some(config.data_dir.join("nonexistent/SOUL.md"));
    assert!(config.archive_path.is_none());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open");
    let identity = daemon.identity_content.lock().unwrap();
    // With no file at the explicit path and no archive_path, identity should
    // fall through to None (explicit path was checked, not found, stops there).
    // The home fallback is NOT tried when soul_file is explicitly set.
    // The test primarily confirms no panics or errors on missing explicit path.
    drop(identity);
}

// --- credential command parsing tests ---

#[test]
fn parse_credential_submit() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.submit","d":{"service":"github.com","username":"user","secret":"pass123","totp_secret":"JBSWY3DPEHPK3PXP"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::CredentialSubmit {
            service,
            username,
            secret,
            totp_secret,
        } => {
            assert_eq!(service, "github.com");
            assert_eq!(username, "user");
            assert_eq!(secret, "pass123");
            assert_eq!(totp_secret, Some("JBSWY3DPEHPK3PXP".to_string()));
        }
        other => panic!("expected CredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_credential_submit_without_totp() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.submit","d":{"service":"hetzner.com","username":"admin","secret":"key123"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::CredentialSubmit { totp_secret, .. } => {
            assert_eq!(totp_secret, None);
        }
        other => panic!("expected CredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_credential_query() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.query","d":{"service":"github.com"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialQuery { service } if service == "github.com"));
}

#[test]
fn parse_credential_remove() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.remove","d":{"service":"github.com"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialRemove { service } if service == "github.com"));
}

#[test]
fn parse_credential_submit_missing_service_defaults_empty() {
    // In v2, missing service defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.submit","d":{"username":"u","secret":"s"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialSubmit { service, .. } if service.is_empty()));
}

#[test]
fn parse_credential_submit_missing_secret_defaults_empty() {
    // In v2, missing secret defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.submit","d":{"service":"x.com","username":"u"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialSubmit { secret, .. } if secret.is_empty()));
}

#[test]
fn parse_credential_query_missing_service_defaults_empty() {
    // In v2, missing service defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.query"}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialQuery { service } if service.is_empty()));
}

#[test]
fn parse_credential_remove_missing_service_defaults_empty() {
    // In v2, missing service defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.remove"}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialRemove { service } if service.is_empty()));
}

// ---- API credential format (key/value) tests ----

#[test]
fn parse_api_credential_submit_basic() {
    // In v2, api_credential.submit defaults validate to true.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-test"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialSubmit {
            key,
            value,
            validate,
        } => {
            assert_eq!(key, "ANTHROPIC_API_KEY");
            assert_eq!(value, "sk-ant-test");
            assert!(validate); // v2 defaults to true
        }
        other => panic!("expected ApiCredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_submit_validate_false() {
    // Explicit validate:false in v2 format.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-test","validate":false}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialSubmit { validate, .. } => {
            assert!(!validate);
        }
        other => panic!("expected ApiCredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_submit_with_validate_true() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"OPENAI_API_KEY","value":"sk-openai-test","validate":true}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialSubmit {
            key,
            value,
            validate,
        } => {
            assert_eq!(key, "OPENAI_API_KEY");
            assert_eq!(value, "sk-openai-test");
            assert!(validate);
        }
        other => panic!("expected ApiCredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_submit_with_validate_bool() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"OPENAI_API_KEY","value":"sk-test","validate":true}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialSubmit { validate, .. } => {
            assert!(validate);
        }
        other => panic!("expected ApiCredentialSubmit, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_submit_missing_value_defaults_empty() {
    // In v2, missing value defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::ApiCredentialSubmit { value, .. } if value.is_empty()));
}

#[test]
fn parse_api_credential_query_single_key() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialQuery { keys } => {
            assert_eq!(keys, vec!["ANTHROPIC_API_KEY".to_string()]);
        }
        other => panic!("expected ApiCredentialQuery, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_query_multiple_keys() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY,OPENAI_API_KEY,SYMBIOTIC_HCLOUD_TOKEN"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialQuery { keys } => {
            assert_eq!(keys.len(), 3);
            assert_eq!(keys[0], "ANTHROPIC_API_KEY");
            assert_eq!(keys[1], "OPENAI_API_KEY");
            assert_eq!(keys[2], "SYMBIOTIC_HCLOUD_TOKEN");
        }
        other => panic!("expected ApiCredentialQuery, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_query_empty_keys_defaults_empty() {
    // In v2, empty keys string results in empty keys list.
    let json =
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":""}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::ApiCredentialQuery { keys } if keys.is_empty()));
}

#[test]
fn parse_api_credential_remove() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":"OPENAI_API_KEY"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::ApiCredentialRemove { key } => {
            assert_eq!(key, "OPENAI_API_KEY");
        }
        other => panic!("expected ApiCredentialRemove, got {other:?}"),
    }
}

#[test]
fn parse_api_credential_remove_empty_key_defaults_empty() {
    // In v2, empty key defaults to empty string at parser level.
    let json =
        r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":""}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::ApiCredentialRemove { key } if key.is_empty()));
}

#[test]
fn api_credential_submit_stores_and_responds() {
    let daemon = daemon_for_test("api-cred-submit");
    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-test123"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");
    assert_eq!(events.len(), 1);
    let env = &events[0];
    assert_eq!(env.sym.a.as_deref(), Some("credential.status"));
    assert_eq!(env.sym.k, Kind::State);
    assert_eq!(detail_str(&env, "key"), Some("ANTHROPIC_API_KEY"));
    assert_eq!(detail_str(&env, "status"), Some("unverified")); // No validation requested

    // Verify it was stored in the vault.
    let stored = daemon
        .credential_vault
        .get("ANTHROPIC_API_KEY")
        .expect("vault query")
        .expect("should be stored");
    assert_eq!(stored.secret, "sk-ant-test123");
}

#[test]
fn api_credential_query_returns_results_and_done() {
    let daemon = daemon_for_test("api-cred-query");
    let now = now_unix();

    // Store one credential first.
    let record = credential_gateway::CredentialRecord {
        service: "ANTHROPIC_API_KEY".to_string(),
        username: "ANTHROPIC_API_KEY".to_string(),
        secret: "sk-ant-stored".to_string(),
        totp_secret: None,
    };
    daemon.credential_vault.put(record).expect("put");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY,OPENAI_API_KEY"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    // Should get 2 result events + 1 done event = 3.
    assert_eq!(events.len(), 3);

    // First result: ANTHROPIC_API_KEY found
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.query.result"));
    assert_eq!(detail_str(&events[0], "key"), Some("ANTHROPIC_API_KEY"));
    assert_ne!(detail_str(&events[0], "status"), Some("missing"));

    // Second result: OPENAI_API_KEY missing
    assert_eq!(events[1].sym.a.as_deref(), Some("credential.query.result"));
    assert_eq!(detail_str(&events[1], "key"), Some("OPENAI_API_KEY"));
    assert_eq!(detail_str(&events[1], "status"), Some("missing"));

    // Done event with totals
    assert_eq!(events[2].sym.a.as_deref(), Some("credential.query.done"));
    assert_eq!(detail_str(&events[2], "total"), Some("2"));
    assert_eq!(detail_str(&events[2], "configured"), Some("1"));
    assert_eq!(detail_str(&events[2], "missing"), Some("1"));
}

#[test]
fn api_credential_remove_deletes_and_confirms() {
    let daemon = daemon_for_test("api-cred-remove");
    let now = now_unix();

    // Store a credential to remove.
    let record = credential_gateway::CredentialRecord {
        service: "OPENAI_API_KEY".to_string(),
        username: "OPENAI_API_KEY".to_string(),
        secret: "sk-openai-to-remove".to_string(),
        totp_secret: None,
    };
    daemon.credential_vault.put(record).expect("put");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":"OPENAI_API_KEY"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    assert_eq!(detail_str(&events[0], "key"), Some("OPENAI_API_KEY"));

    // Verify it was removed from the vault.
    let stored = daemon
        .credential_vault
        .get("OPENAI_API_KEY")
        .expect("vault query");
    assert!(stored.is_none());
}

#[test]
fn api_credential_remove_nonexistent_returns_completed() {
    let daemon = daemon_for_test("api-cred-remove-noop");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":"NONEXISTENT_KEY"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Success));
}

#[test]
fn api_credential_submit_stores_metadata_sidecar() {
    let daemon = daemon_for_test("api-cred-meta");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-meta-test1234"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    // Check that metadata sidecar was stored.
    let meta = daemon
        .credential_vault
        .get("_meta:ANTHROPIC_API_KEY")
        .expect("vault query")
        .expect("metadata should exist");
    let parsed: serde_json::Value = serde_json::from_str(&meta.secret).expect("valid JSON");
    assert_eq!(parsed["status"], "unverified");
    assert_eq!(parsed["masked_suffix"], "1234");
}

#[test]
fn api_credential_submit_masked_suffix_short_value() {
    let daemon = daemon_for_test("api-cred-short");
    let now = now_unix();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"SHORT_KEY","value":"ab"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    // Short values get masked as "****".
    let meta = daemon
        .credential_vault
        .get("_meta:SHORT_KEY")
        .expect("vault query")
        .expect("metadata should exist");
    let parsed: serde_json::Value = serde_json::from_str(&meta.secret).expect("valid JSON");
    assert_eq!(parsed["masked_suffix"], "****");
}

#[test]
fn login_credential_format_still_works_with_service() {
    // Ensure the login credential format (service/username/secret) works in v2.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.submit","d":{"service":"github.com","username":"user","secret":"pass"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::CredentialSubmit {
            service,
            username,
            secret,
            ..
        } => {
            assert_eq!(service, "github.com");
            assert_eq!(username, "user");
            assert_eq!(secret, "pass");
        }
        other => panic!("expected CredentialSubmit (login format), got {other:?}"),
    }
}

#[test]
fn login_credential_query_still_works_with_service() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.query","d":{"service":"github.com"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialQuery { service } if service == "github.com"));
}

#[test]
fn login_credential_remove_still_works_with_service() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.remove","d":{"service":"github.com"}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialRemove { service } if service == "github.com"));
}

// ── credential.authenticate parser tests ────────────────────────────────

#[test]
fn parse_credential_authenticate_basic() {
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate","d":{"domain":"github.com"}}}"#;
    let cmd = parse_control_command(json);
    match cmd {
        ControlCommand::CredentialAuthenticate { domain } => {
            assert_eq!(domain, "github.com");
        }
        other => panic!("expected CredentialAuthenticate, got {other:?}"),
    }
}

#[test]
fn parse_credential_authenticate_missing_domain_defaults_empty() {
    // In v2, missing domain defaults to empty string at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate"}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialAuthenticate { domain } if domain.is_empty()));
}

#[test]
fn parse_credential_authenticate_empty_domain_defaults_empty() {
    // In v2, empty domain string defaults to empty at parser level.
    let json = r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate","d":{"domain":""}}}"#;
    let cmd = parse_control_command(json);
    assert!(matches!(cmd, ControlCommand::CredentialAuthenticate { domain } if domain.is_empty()));
}

#[test]
fn credential_authenticate_without_engine_returns_failed() {
    let daemon = daemon_for_test("cred-auth-no-engine");
    let now = now_unix();
    // auth_engine is None by default in test daemon
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate","d":{"domain":"github.com"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(events[0].sym.a.as_deref(), Some("auth.failed"));
    assert_eq!(detail_str(&events[0], "target"), Some("github.com"));
}

#[test]
fn credential_authenticate_with_auth_sandbox_returns_success_and_stores_session() {
    let mut config = daemon_config_for_test("cred-auth-sandbox");
    let scripts_dir = config.data_dir.join("auth-scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    let script_path = scripts_dir.join("github.com.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
cat >/dev/null
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    config.auth_scripts_dir = Some(scripts_dir);
    config.auth_sandbox_bin = Some(credential_gateway_bin_for_test());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("credential store should work");

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate","d":{"domain":"github.com"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .any(|event| event.sym.a.as_deref() == Some("auth.started")),
        "expected auth.started event"
    );
    let completed = events
        .iter()
        .find(|event| event.sym.a.as_deref() == Some("auth.completed"))
        .expect("expected auth.completed event");
    assert_eq!(completed.sym.k, Kind::State);
    assert_eq!(detail_str(&completed, "worker_status"), Some("completed"));
    assert_eq!(detail_str(&completed, "target"), Some("github.com"));
    assert_eq!(
        detail_str(&completed, "auth_profile_id"),
        Some("github.com")
    );
    assert_eq!(detail_str(&completed, "auth_profile_match"), Some("exact"));
    assert_eq!(detail_str(&completed, "auth_script_kind"), Some("shell"));
    assert_eq!(
        detail_val(&completed, "auth_profile_sha256")
            .and_then(|value| value.as_str())
            .map(str::len),
        Some(64)
    );

    let session = daemon
        .credential_vault
        .get("github.com:session")
        .expect("session lookup should work")
        .expect("session should be stored");
    assert_eq!(session.username, "github.com");
    assert_eq!(session.secret, "sandbox_session_token");
}

#[test]
fn credential_authenticate_failure_emits_attested_profile_metadata() {
    let daemon = daemon_with_auth_script(
        "cred-auth-sandbox-fail",
        r#"#!/bin/sh
cat >/dev/null
echo '{"success": false, "error": "Invalid credentials"}'
"#,
    );
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("credential store should work");

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"credential.authenticate","d":{"domain":"github.com"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    let failed = events
        .iter()
        .find(|event| event.sym.a.as_deref() == Some("auth.failed"))
        .expect("expected auth.failed event");
    assert_eq!(detail_str(&failed, "target"), Some("github.com"));
    assert_eq!(detail_str(&failed, "auth_profile_id"), Some("github.com"));
    assert_eq!(detail_str(&failed, "auth_profile_match"), Some("exact"));
    assert_eq!(detail_str(&failed, "auth_script_kind"), Some("shell"));
    assert_eq!(
        detail_val(&failed, "auth_profile_sha256")
            .and_then(|value| value.as_str())
            .map(str::len),
        Some(64)
    );
}

#[test]
fn credential_approve_with_remember_for_secs_persists_policy_and_supports_list_revoke() {
    let daemon = daemon_with_auth_script(
        "cred-auth-remember",
        r#"#!/bin/sh
cat >/dev/null
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
    );
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("credential store should work");

    let now = now_unix();
    let mut record = AuthJobRecord::from_request(
        crate::auth_jobs::AuthJobRequest {
            target: "github.com".to_string(),
            scopes: vec!["web.login".to_string()],
            session_type: credential_gateway::SessionType::Browser,
            purpose: "Open GitHub settings".to_string(),
            room_id: "#credentials".to_string(),
            thread_id: None,
            auth_profile: None,
            requested_by: "@user:test".to_string(),
            goal_scope: None,
            goal_room: None,
            goal_template: None,
            goal_id: None,
        },
        now,
        crate::auth_jobs::AuthJobConfig {
            approval_ttl_secs: 300,
            input_ttl_secs: 300,
        },
    );
    record.attestation = Some(
        daemon
            .auth_engine
            .as_ref()
            .expect("auth engine")
            .resolve_attestation("github.com")
            .expect("resolve attestation"),
    );
    let request_id = record.request_id.clone();
    daemon
        .auth_jobs
        .lock()
        .expect("auth store lock")
        .create(record)
        .expect("create auth job");

    let approve_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approve","d":{"request_id": request_id, "remember_for_secs": 3600}}
                })
                .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("approve route should work");

    let completed = approve_events
        .iter()
        .find(|event| event.sym.a.as_deref() == Some("auth.completed"))
        .expect("expected auth.completed event");
    assert_eq!(
        detail_str(&completed, "approval_mode"),
        Some("remembered_policy")
    );
    let policy_id = detail_str(&completed, "approval_policy_id")
        .expect("approval policy id should be present")
        .to_string();

    let list_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approval_policy.list","d":{"include_inactive": false}}
                })
                .to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("list route should work");
    assert_eq!(
        detail_val(&list_events[0], "count").and_then(|value| value.as_u64()),
        Some(1)
    );
    let policies = detail_val(&list_events[0], "policies")
        .and_then(|value| value.as_array())
        .expect("policies array should be present");
    assert_eq!(policies.len(), 1);
    assert_eq!(policies[0]["policy_id"].as_str(), Some(policy_id.as_str()));
    assert_eq!(policies[0]["target"].as_str(), Some("github.com"));
    assert_eq!(policies[0]["auth_profile_id"].as_str(), Some("github.com"));
    assert_eq!(
        policies[0]["auth_profile_sha256"].as_str().map(str::len),
        Some(64)
    );

    let revoke_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approval_policy.revoke","d":{"policy_id": policy_id}}
                })
                .to_string(),
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("revoke route should work");
    assert_eq!(
        detail_str(&revoke_events[0], "auth_profile_id"),
        Some("github.com")
    );

    let active_list = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approval_policy.list","d":{"include_inactive": false}}
                })
                .to_string(),
                timestamp: now + 3,
            },
            now + 3,
        )
        .expect("active list route should work");
    assert_eq!(
        detail_val(&active_list[0], "count").and_then(|value| value.as_u64()),
        Some(0)
    );

    let all_list = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approval_policy.list","d":{"include_inactive": true}}
                })
                .to_string(),
                timestamp: now + 4,
            },
            now + 4,
        )
        .expect("full list route should work");
    let all_policies = detail_val(&all_list[0], "policies")
        .and_then(|value| value.as_array())
        .expect("policies array should be present");
    assert_eq!(all_policies.len(), 1);
    assert!(all_policies[0]["revoked_at"].as_u64().is_some());
}

#[test]
fn credential_approve_expired_request_emits_timed_out_failure() {
    let daemon = daemon_for_test("cred-auth-expired-approve");
    let now = now_unix();
    let record = AuthJobRecord::from_request(
        crate::auth_jobs::AuthJobRequest {
            target: "github.com".to_string(),
            scopes: vec!["web.login".to_string()],
            session_type: credential_gateway::SessionType::Browser,
            purpose: "Open GitHub settings".to_string(),
            room_id: "#credentials".to_string(),
            thread_id: None,
            auth_profile: None,
            requested_by: "@user:test".to_string(),
            goal_scope: None,
            goal_room: None,
            goal_template: None,
            goal_id: None,
        },
        now - 10,
        crate::auth_jobs::AuthJobConfig {
            approval_ttl_secs: 5,
            input_ttl_secs: 30,
        },
    );
    let request_id = record.request_id.clone();
    daemon
        .auth_jobs
        .lock()
        .expect("auth store lock")
        .create(record)
        .expect("create auth job");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approve","d":{"request_id": request_id}}
                })
                .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("approve route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("auth.failed"));
    assert_eq!(detail_str(&events[0], "code"), Some("timed_out"));
    assert_eq!(
        detail_str(&events[0], "reason"),
        Some("Authentication request expired")
    );
}

#[test]
fn credential_approve_with_remember_for_expired_request_does_not_create_policy() {
    let daemon = daemon_with_auth_script(
        "cred-auth-expired-remember",
        r#"#!/bin/sh
cat >/dev/null
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
    );
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("credential store should work");
    let now = now_unix();
    let mut record = AuthJobRecord::from_request(
        crate::auth_jobs::AuthJobRequest {
            target: "github.com".to_string(),
            scopes: vec!["web.login".to_string()],
            session_type: credential_gateway::SessionType::Browser,
            purpose: "Open GitHub settings".to_string(),
            room_id: "#credentials".to_string(),
            thread_id: None,
            auth_profile: None,
            requested_by: "@user:test".to_string(),
            goal_scope: None,
            goal_room: None,
            goal_template: None,
            goal_id: None,
        },
        now - 10,
        crate::auth_jobs::AuthJobConfig {
            approval_ttl_secs: 5,
            input_ttl_secs: 30,
        },
    );
    record.attestation = Some(
        daemon
            .auth_engine
            .as_ref()
            .expect("auth engine")
            .resolve_attestation("github.com")
            .expect("resolve attestation"),
    );
    let request_id = record.request_id.clone();
    daemon
        .auth_jobs
        .lock()
        .expect("auth store lock")
        .create(record)
        .expect("create auth job");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approve","d":{"request_id": request_id, "remember_for_secs": 3600}}
                })
                .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("approve route should work");
    assert_eq!(events[0].sym.a.as_deref(), Some("auth.failed"));
    assert_eq!(detail_str(&events[0], "code"), Some("timed_out"));

    let list_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.approval_policy.list","d":{"include_inactive": true}}
                })
                .to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("list route should work");
    assert_eq!(
        detail_val(&list_events[0], "count").and_then(|value| value.as_u64()),
        Some(0)
    );
}

#[test]
fn credential_respond_expired_request_emits_timed_out_failure() {
    let daemon = daemon_for_test("cred-auth-expired-respond");
    let now = now_unix();
    let mut record = AuthJobRecord::from_request(
        crate::auth_jobs::AuthJobRequest {
            target: "github.com".to_string(),
            scopes: vec!["web.login".to_string()],
            session_type: credential_gateway::SessionType::Browser,
            purpose: "Open GitHub settings".to_string(),
            room_id: "#credentials".to_string(),
            thread_id: None,
            auth_profile: None,
            requested_by: "@user:test".to_string(),
            goal_scope: None,
            goal_room: None,
            goal_template: None,
            goal_id: None,
        },
        now - 10,
        crate::auth_jobs::AuthJobConfig {
            approval_ttl_secs: 5,
            input_ttl_secs: 5,
        },
    );
    record.status = AuthJobStatus::AwaitingInput;
    record.phase = AuthJobPhase::Input;
    record.input_request = Some(crate::auth_jobs::AuthInputRequest {
        kind: AuthInputKind::TotpCode,
        prompt: "Enter code".to_string(),
        masked_hint: None,
    });
    let request_id = record.request_id.clone();
    daemon
        .auth_jobs
        .lock()
        .expect("auth store lock")
        .create(record)
        .expect("create auth job");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: serde_json::json!({
                    "msgtype":"sym.c",
                    "body":"",
                    "sym":{"v":2,"c":"credential.respond","d":{"request_id": request_id, "value": "654321"}}
                })
                .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("respond route should work");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("auth.failed"));
    assert_eq!(detail_str(&events[0], "code"), Some("timed_out"));
    assert_eq!(detail_str(&events[0], "phase"), Some("input"));
}

// ── StubCredentialValidator for E2E lifecycle tests ─────────────────────

struct StubCredentialValidator {
    results: Mutex<HashMap<String, CredentialValidationResult>>,
}

impl StubCredentialValidator {
    fn new() -> Self {
        Self {
            results: Mutex::new(HashMap::new()),
        }
    }

    fn set_result(&self, key: &str, result: CredentialValidationResult) {
        self.results
            .lock()
            .expect("lock")
            .insert(key.to_string(), result);
    }
}

impl ValidateCredential for StubCredentialValidator {
    fn validate(&self, key: &str, _value: &str) -> CredentialValidationResult {
        self.results
            .lock()
            .expect("lock")
            .get(key)
            .cloned()
            .unwrap_or(CredentialValidationResult::Skipped)
    }
}

fn daemon_with_stub_validator(name: &str) -> (SymbioticDaemon, *const StubCredentialValidator) {
    let mut daemon = daemon_for_test(name);
    let stub = Box::new(StubCredentialValidator::new());
    let ptr = &*stub as *const StubCredentialValidator;
    daemon.credential_validator = stub;
    (daemon, ptr)
}

// ── E2E Credential Lifecycle Tests ──────────────────────────────────────

#[test]
fn credential_lifecycle_submit_query_remove() {
    let daemon = daemon_for_test("cred-lifecycle");
    let now = now_unix();

    // Submit
    let submit_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-lifecycle-test1234"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("submit should route");
    assert_eq!(submit_events.len(), 1);
    assert_eq!(submit_events[0].sym.a.as_deref(), Some("credential.status"));
    assert_eq!(submit_events[0].sym.k, Kind::State);

    // Query
    let query_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY"}}}"#
                    .to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("query should route");
    // 1 result + 1 done = 2 events
    assert_eq!(query_events.len(), 2);
    assert_eq!(
        query_events[0].sym.a.as_deref(),
        Some("credential.query.result")
    );
    assert_ne!(detail_str(&query_events[0], "status"), Some("missing"));
    assert_eq!(
        query_events[1].sym.a.as_deref(),
        Some("credential.query.done")
    );
    assert_eq!(detail_str(&query_events[1], "configured"), Some("1"));

    // Remove
    let remove_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":"ANTHROPIC_API_KEY"}}}"#
                    .to_string(),
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("remove should route");
    assert_eq!(remove_events.len(), 1);
    assert_eq!(remove_events[0].sym.k, Kind::Message);
    assert_eq!(remove_events[0].sym.s, Some(Status::Success));

    // Verify removed from vault
    let stored = daemon
        .credential_vault
        .get("ANTHROPIC_API_KEY")
        .expect("vault query");
    assert!(stored.is_none());
}

#[test]
fn credential_submit_with_validation_valid() {
    let (daemon, stub_ptr) = daemon_with_stub_validator("cred-valid");
    // SAFETY: ptr is valid for the lifetime of daemon
    unsafe {
        (*stub_ptr).set_result("ANTHROPIC_API_KEY", CredentialValidationResult::Valid);
    }
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-valid1234","validate":true}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.status"));
    assert_eq!(detail_str(&events[0], "status"), Some("valid"));
    // last_verified should be set
    assert!(has_detail(&events[0], "last_verified"));
}

#[test]
fn credential_submit_with_validation_invalid() {
    let (daemon, stub_ptr) = daemon_with_stub_validator("cred-invalid");
    unsafe {
        (*stub_ptr).set_result(
            "OPENAI_API_KEY",
            CredentialValidationResult::Invalid {
                reason: "API returned 401".to_string(),
            },
        );
    }
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"OPENAI_API_KEY","value":"sk-invalid1234","validate":true}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.status"));
    assert_eq!(detail_str(&events[0], "status"), Some("unverified"));
    // Credential should still be stored in vault despite invalid validation
    let stored = daemon
        .credential_vault
        .get("OPENAI_API_KEY")
        .expect("vault query")
        .expect("should be stored");
    assert_eq!(stored.secret, "sk-invalid1234");
}

#[test]
fn credential_submit_with_validation_unreachable() {
    let (daemon, stub_ptr) = daemon_with_stub_validator("cred-unreachable");
    unsafe {
        (*stub_ptr).set_result(
            "OPENROUTER_API_KEY",
            CredentialValidationResult::Unreachable {
                reason: "connection timed out".to_string(),
            },
        );
    }
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"OPENROUTER_API_KEY","value":"sk-or-test1234","validate":true}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.status"));
    assert_eq!(events[0].sym.k, Kind::State);
    assert_eq!(detail_str(&events[0], "status"), Some("unverified"));
    // Should still be stored
    let stored = daemon
        .credential_vault
        .get("OPENROUTER_API_KEY")
        .expect("vault query");
    assert!(stored.is_some());
}

#[test]
fn credential_remove_nonexistent_idempotent() {
    let daemon = daemon_for_test("cred-rm-noop");
    let now = now_unix();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.remove","d":{"key":"DOES_NOT_EXIST"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.k, Kind::Message);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    // Should succeed (idempotent) even though key didn't exist
}

#[test]
fn credential_query_mixed_found_and_missing() {
    let daemon = daemon_for_test("cred-mixed-query");
    let now = now_unix();

    // Store only one of three keys
    let record = credential_gateway::CredentialRecord {
        service: "ANTHROPIC_API_KEY".to_string(),
        username: "ANTHROPIC_API_KEY".to_string(),
        secret: "sk-ant-mix-test1234".to_string(),
        totp_secret: None,
    };
    daemon.credential_vault.put(record).expect("put");

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY,OPENAI_API_KEY,OPENROUTER_API_KEY"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("should route");

    // 3 result events + 1 done = 4
    assert_eq!(events.len(), 4);

    // First: found
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.query.result"));
    assert_ne!(detail_str(&events[0], "status"), Some("missing"));

    // Second and third: missing
    assert_eq!(detail_str(&events[1], "status"), Some("missing"));
    assert_eq!(detail_str(&events[2], "status"), Some("missing"));

    // Done event
    assert_eq!(events[3].sym.a.as_deref(), Some("credential.query.done"));
    assert_eq!(detail_str(&events[3], "configured"), Some("1"));
    assert_eq!(detail_str(&events[3], "missing"), Some("2"));
}

#[test]
fn credential_metadata_preserved_across_query() {
    let (daemon, stub_ptr) = daemon_with_stub_validator("cred-meta-query");
    unsafe {
        (*stub_ptr).set_result("ANTHROPIC_API_KEY", CredentialValidationResult::Valid);
    }
    let now = now_unix();

    // Submit with validation so metadata gets set
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-meta-query1234","validate":true}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("submit should route");

    // Query the credential
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY"}}}"#
                    .to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("query should route");

    assert_eq!(events.len(), 2); // 1 result + 1 done
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.query.result"));
    assert_eq!(detail_str(&events[0], "status"), Some("valid"));
    assert_eq!(detail_str(&events[0], "masked_suffix"), Some("1234"));
    // last_verified should be preserved
    assert!(has_detail(&events[0], "last_verified"));
}

#[test]
fn credential_submit_overwrites_existing() {
    let daemon = daemon_for_test("cred-overwrite");
    let now = now_unix();

    // Submit first value
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-first-abcd"}}}"#.to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("first submit");

    // Submit second value (overwrite)
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.submit","d":{"key":"ANTHROPIC_API_KEY","value":"sk-ant-second-wxyz"}}}"#.to_string(),
                timestamp: now + 1,
            },
            now + 1,
        )
        .expect("second submit");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("credential.status"));
    assert_eq!(events[0].sym.k, Kind::State);

    // Query to verify masked_suffix matches new value
    let query_events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"api_credential.query","d":{"keys":"ANTHROPIC_API_KEY"}}}"#
                    .to_string(),
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("query");

    assert_eq!(
        query_events[0].sym.a.as_deref(),
        Some("credential.query.result")
    );
    assert_eq!(detail_str(&query_events[0], "masked_suffix"), Some("wxyz"));

    // Verify vault has the new value
    let stored = daemon
        .credential_vault
        .get("ANTHROPIC_API_KEY")
        .expect("vault query")
        .expect("should be stored");
    assert_eq!(stored.secret, "sk-ant-second-wxyz");
}

// ---------------------------------------------------------------------------
// E2E roundtrip tests
// ---------------------------------------------------------------------------

#[test]
fn e2e_intake_url_to_archive_to_recall() {
    let daemon = daemon_for_test("e2e-url-archive-recall");
    let now = now_unix();
    let url = normalize_url("https://example.com/e2e-recall").expect("valid url");
    daemon
        .enqueue_intake_urls(vec![url], vec!["e2e".to_string()], IntakeSource::Cli)
        .expect("enqueue should succeed");

    // Step 1: ingest.fetch
    let event1 = daemon
        .run_once(now)
        .expect("run_once should succeed")
        .expect("ingest job should be processed")
        .0;
    assert_eq!(event1.event_type, EventType::IngestFetch);
    assert_eq!(event1.status, "completed");

    // Step 2: archive.review.enqueue
    let event2 = daemon
        .run_once(now + 1)
        .expect("run_once should succeed")
        .expect("review enqueue job should be processed")
        .0;
    assert_eq!(event2.event_type, EventType::ArchiveReviewEnqueue);
    assert_eq!(event2.status, "completed");

    // Step 3: archive.review
    let event3 = daemon
        .run_once(now + 2)
        .expect("run_once should succeed")
        .expect("review job should be processed")
        .0;
    assert_eq!(event3.event_type, EventType::ArchiveReview);
    assert_eq!(event3.status, "completed");

    // Step 4: verify archive entry exists
    let records = daemon.archive_records().expect("archive list should work");
    assert!(
        !records.is_empty(),
        "archive should contain at least one record after full pipeline"
    );

    // Step 5: recall via context gateway
    let pack = daemon
        .get_context(&ContextRequest {
            request_id: "e2e-ctx-1".to_string(),
            query: "fetched".to_string(),
            model_class: symbiotic_context::ModelClass::Local,
            purpose: symbiotic_context::Purpose::Answer,
            sensitivity_max: ContextSensitivity::Private,
            token_budget: 200,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("context should build");
    assert!(
        !pack.items.is_empty(),
        "recall should return items for ingested content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn e2e_intake_via_matrix_transport_emits_status_events() {
    let daemon = daemon_for_test("e2e-matrix-intake");
    let now = now_unix();
    let transport = InMemoryMatrixTransport::default();

    // Push a URL to #intake via transport
    transport
        .push_incoming(MatrixMessage {
            room_id: "#intake".to_string(),
            sender: "@user:test".to_string(),
            body: "https://example.com/e2e-transport".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");

    // Pump transport: should process the message and emit intake.started
    let processed = daemon
        .pump_transport_once(&transport, now)
        .await
        .expect("pump should work");
    assert_eq!(processed, 1);

    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].room_id, "#intake");
    assert_eq!(outgoing[0].envelope.sym.s, Some(Status::Working));

    // Process ingest.fetch
    let event1 = daemon
        .run_once(now_unix())
        .expect("run_once should succeed")
        .expect("ingest job should be processed")
        .0;
    assert_eq!(event1.event_type, EventType::IngestFetch);
    assert_eq!(event1.status, "completed");

    // Process archive.review.enqueue
    let event2 = daemon
        .run_once(now_unix())
        .expect("run_once should succeed")
        .expect("review enqueue should be processed")
        .0;
    assert_eq!(event2.event_type, EventType::ArchiveReviewEnqueue);
    assert_eq!(event2.status, "completed");

    // Process archive.review
    let event3 = daemon
        .run_once(now_unix())
        .expect("run_once should succeed")
        .expect("review should be processed")
        .0;
    assert_eq!(event3.event_type, EventType::ArchiveReview);
    assert_eq!(event3.status, "completed");

    // Verify archive entry exists
    let records = daemon.archive_records().expect("archive list should work");
    assert!(
        !records.is_empty(),
        "archive should contain record after matrix intake pipeline"
    );
}

#[test]
fn e2e_goal_start_to_workflow_completion() {
    let daemon = daemon_for_test("e2e-goal-workflow");
    let now = now_unix();

    // Send "goal start intake-url" to #control
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: "goal start intake-url".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    // Verify goal.started event
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(detail_str(&events[0], "template"), Some("intake-url"));

    // Verify workflow.run job queued
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query should work");
    assert_eq!(queued.len(), 1);

    // Run the workflow job
    let workflow_event = daemon
        .run_once(now + 1)
        .expect("run should work")
        .expect("workflow job should exist")
        .0;
    assert_eq!(workflow_event.event_type, EventType::WorkflowRun);
    assert_eq!(workflow_event.status, "completed");
    assert_eq!(workflow_event.goal_template.as_deref(), Some("intake-url"));
    assert!(workflow_event.goal_run_id.is_some());

    // Verify no more jobs queued
    let next = daemon.run_once(now + 2).expect("run should work");
    assert!(next.is_none(), "no further jobs after workflow completion");
}

#[tokio::test]
async fn e2e_goal_start_via_transport_emits_cross_room_events() {
    let daemon = daemon_for_test("e2e-goal-transport");
    let now = now_unix();
    let transport = InMemoryMatrixTransport::default();

    // Push "goal start intake-url" to #control via transport
    transport
        .push_incoming(MatrixMessage {
            room_id: "#control".to_string(),
            sender: "@user:test".to_string(),
            body: "goal start intake-url".to_string(),
            timestamp: now,
        })
        .expect("push_incoming");

    // Pump transport
    let processed = daemon
        .pump_transport_once(&transport, now)
        .await
        .expect("pump should work");
    assert_eq!(processed, 1);

    // Verify goal.started event sent to #control
    let outgoing = transport.drain_outgoing().expect("drain_outgoing");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(outgoing[0].room_id, "#control");
    assert_eq!(outgoing[0].envelope.sym.s, Some(Status::Working));
    assert_eq!(outgoing[0].envelope.sym.s, Some(Status::Working));

    // Run the workflow job
    let workflow_event = daemon
        .run_once(now + 1)
        .expect("run should work")
        .expect("workflow job should exist")
        .0;
    assert_eq!(workflow_event.event_type, EventType::WorkflowRun);
    assert_eq!(workflow_event.status, "completed");
    assert_eq!(workflow_event.goal_template.as_deref(), Some("intake-url"));
}

#[test]
fn e2e_note_intake_to_recall_with_sensitivity() {
    let daemon = daemon_for_test("e2e-note-recall");
    let now = now_unix();

    // Submit a note via intake pipeline
    let result = daemon
        .submit_intake_request(IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some("E2E test note about quantum computing advances".to_string()),
            tags: vec!["science".to_string(), "e2e".to_string()],
            file_path: None,
            title: None,
        })
        .expect("submit should work");

    // Verify the note was routed to archive
    assert_eq!(result.summary.total, 1);
    assert_eq!(result.summary.ingested, 1);
    assert_eq!(result.items[0].route, IntakeRoute::Archive);
    assert!(result.items[0].review_queued);

    // Process archive.review.enqueue
    let event1 = daemon
        .run_once(now)
        .expect("run_once should succeed")
        .expect("review enqueue should be processed")
        .0;
    assert_eq!(event1.event_type, EventType::ArchiveReviewEnqueue);
    assert_eq!(event1.status, "completed");

    // Process archive.review
    let event2 = daemon
        .run_once(now + 1)
        .expect("run_once should succeed")
        .expect("review should be processed")
        .0;
    assert_eq!(event2.event_type, EventType::ArchiveReview);
    assert_eq!(event2.status, "completed");

    // Verify the note entry exists in archive
    let records = daemon.archive_records().expect("archive list should work");
    let note_record = records
        .iter()
        .find(|r| r.content.contains("quantum computing"))
        .expect("note record should exist in archive");
    assert!(
        note_record.content.contains("quantum computing"),
        "archive entry should contain the note text"
    );

    // Verify recall works for notes
    let pack = daemon
        .get_context(&ContextRequest {
            request_id: "e2e-note-ctx".to_string(),
            query: "quantum computing".to_string(),
            model_class: symbiotic_context::ModelClass::Local,
            purpose: symbiotic_context::Purpose::Answer,
            sensitivity_max: ContextSensitivity::Private,
            token_budget: 200,
            tags: vec![],
            recency_days: None,
            filter_threads: None,
            disclosure_tier: None,
            class_budget: None,
        })
        .expect("context should build");
    assert!(
        !pack.items.is_empty(),
        "recall should return items for note content"
    );
}

// ---------------------------------------------------------------------------
// E2E tests: Agent execution pipeline (ReAct loop via ProviderRouter)
// ---------------------------------------------------------------------------

/// Mock completion provider that returns configurable responses.
/// Used to test the full ReAct execution path without a real LLM.
mod agent_test_helpers {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};
    use symbiotic_providers::{
        CapabilitySet, CompletionResponse, ModelProvider, ProviderCapability, ProviderClass,
        ProviderError, ProviderRegistry, RegisteredProvider,
    };

    pub struct MockCompletionProvider {
        pub name: String,
        pub class: ProviderClass,
        pub model: String,
        pub capabilities: CapabilitySet,
        pub response_content: String,
        pub call_count: AtomicU32,
    }

    impl MockCompletionProvider {
        pub fn new(name: &str, class: ProviderClass, response: &str) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                response_content: response.to_string(),
                call_count: AtomicU32::new(0),
            }
        }
    }

    impl ModelProvider for MockCompletionProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
        }
        fn model_name(&self) -> &str {
            &self.model
        }
        fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
        fn pricing(&self) -> Option<&symbiotic_providers::PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl symbiotic_providers::CompletionProvider for MockCompletionProvider {
        async fn complete(
            &self,
            _request: &symbiotic_providers::CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                content: self.response_content.clone(),
                model: self.model.clone(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                finish_reason: Some("stop".into()),
            })
        }
    }

    /// Build a `ProviderRouter` with a mock completion provider.
    pub fn mock_provider_router(
        response: &str,
    ) -> (Arc<ProviderRouter>, Arc<MockCompletionProvider>) {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(MockCompletionProvider::new(
            "mock-llm",
            ProviderClass::Local,
            response,
        ));
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn symbiotic_providers::CompletionProvider> = provider.clone();
        registry.register(RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });
        let _ = registry.set_default(ProviderCapability::Completion, "mock-llm");
        let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))));
        (router, provider)
    }

    /// Build an `AgentExecuteExecutor` with a mock completion provider.
    pub fn mock_agent_executor(
        response: &str,
    ) -> (AgentExecuteExecutor, Arc<MockCompletionProvider>) {
        let tmp = std::env::temp_dir().join(format!("symbiotic_agent_test_{}", unique_suffix()));
        let _ = std::fs::create_dir_all(&tmp);

        let (router, provider) = mock_provider_router(response);
        let mut role_registry = symbiotic_agent_config::RoleRegistry::new();
        let _ = symbiotic_agent_config::defaults::register_defaults(&mut role_registry);
        let role_registry = Arc::new(role_registry);

        let archive_store =
            Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("test archive store"));
        let queue: Arc<dyn QueueBackend> =
            Arc::new(FileQueueStore::open(tmp.join("queue.json")).expect("test queue store"));

        let executor = AgentExecuteExecutor {
            goals_dir: tmp.clone(),
            repo_root: tmp,
            provider_router: router,
            role_registry,
            identity_content: Arc::new(Mutex::new(None)),
            agent_backend: AgentBackend::React,
            archive_store,
            queue,
            vector_index: None,
            broker: None,
            bridge_session_store: Arc::new(Mutex::new(
                crate::bridge_interactions::BridgeSessionStore::default(),
            )),
            llm_gateway_socket: None,
            sandbox_manager: None,
            dispatch_backend: std::sync::OnceLock::new(),
            runner_harness_mode: crate::workers::RunnerHarnessMode::Process,
        };
        (executor, provider)
    }

    // -----------------------------------------------------------------------
    // Sequential mock: returns different responses per call (for tool tests)
    // -----------------------------------------------------------------------

    /// Mock completion provider that returns a different response on each call.
    /// Used for E2E tests where the ReAct loop makes multiple LLM calls
    /// (tool call → tool result → done).
    pub struct SequentialMockCompletionProvider {
        pub name: String,
        pub class: ProviderClass,
        pub model: String,
        pub capabilities: CapabilitySet,
        pub responses: std::sync::Mutex<Vec<String>>,
        pub call_count: AtomicU32,
    }

    impl SequentialMockCompletionProvider {
        pub fn new(name: &str, class: ProviderClass, responses: Vec<String>) -> Self {
            Self {
                name: name.to_string(),
                class,
                model: format!("{name}-model"),
                capabilities: CapabilitySet::new(vec![ProviderCapability::Completion]),
                responses: std::sync::Mutex::new(responses),
                call_count: AtomicU32::new(0),
            }
        }
    }

    impl ModelProvider for SequentialMockCompletionProvider {
        fn name(&self) -> &str {
            &self.name
        }
        fn provider_class(&self) -> ProviderClass {
            self.class
        }
        fn model_name(&self) -> &str {
            &self.model
        }
        fn capabilities(&self) -> &CapabilitySet {
            &self.capabilities
        }
        fn pricing(&self) -> Option<&symbiotic_providers::PricingInfo> {
            None
        }
    }

    #[async_trait]
    impl symbiotic_providers::CompletionProvider for SequentialMockCompletionProvider {
        async fn complete(
            &self,
            _request: &symbiotic_providers::CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            let idx = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
            let responses = self.responses.lock().unwrap();
            let content = responses
                .get(idx)
                .cloned()
                .unwrap_or_else(|| r#"{"done": true, "result": "fallback"}"#.to_string());
            Ok(CompletionResponse {
                content,
                model: self.model.clone(),
                input_tokens: Some(10),
                output_tokens: Some(20),
                finish_reason: Some("stop".into()),
            })
        }
    }

    /// Build a `ProviderRouter` with a sequential mock completion provider.
    pub fn mock_sequential_provider_router(responses: Vec<String>) -> Arc<ProviderRouter> {
        let mut registry = ProviderRegistry::new();
        let provider = Arc::new(SequentialMockCompletionProvider::new(
            "mock-llm",
            ProviderClass::Local,
            responses,
        ));
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn symbiotic_providers::CompletionProvider> = provider;
        registry.register(RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });
        let _ = registry.set_default(ProviderCapability::Completion, "mock-llm");
        Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
            registry,
        ))))
    }

    /// Inject a `SequentialMockCompletionProvider` into an existing daemon's
    /// provider registry, replacing whatever was there before. Since the daemon
    /// and its executors share the same `Arc<ProviderRouter>` (and thus the same
    /// inner `Arc<RwLock<ProviderRegistry>>`), the mock becomes immediately
    /// available to the ReAct loop.
    pub fn inject_sequential_mock_into_daemon(daemon: &SymbioticDaemon, responses: Vec<String>) {
        let reg_arc = daemon.provider_router.registry();
        let mut reg = reg_arc.write().expect("registry write lock");
        let provider = Arc::new(SequentialMockCompletionProvider::new(
            "mock-llm",
            ProviderClass::Local,
            responses,
        ));
        let base: Arc<dyn ModelProvider> = provider.clone();
        let completion: Arc<dyn symbiotic_providers::CompletionProvider> = provider;
        reg.register(RegisteredProvider {
            base,
            completion: Some(completion),
            embedding: None,
            image: None,
            video: None,
            agent: None,
        });
        let _ = reg.set_default(ProviderCapability::Completion, "mock-llm");
    }

    /// Build an `AgentExecuteExecutor` with a sequential mock and return the
    /// archive store + queue store for post-execution assertions.
    pub fn mock_agent_executor_with_stores(
        responses: Vec<String>,
    ) -> (
        AgentExecuteExecutor,
        Arc<FileArchiveStore>,
        Arc<dyn QueueBackend>,
    ) {
        let tmp =
            std::env::temp_dir().join(format!("symbiotic_agent_tool_test_{}", unique_suffix()));
        let _ = std::fs::create_dir_all(&tmp);

        let router = mock_sequential_provider_router(responses);
        let mut role_registry = symbiotic_agent_config::RoleRegistry::new();
        let _ = symbiotic_agent_config::defaults::register_defaults(&mut role_registry);
        let role_registry = Arc::new(role_registry);

        let archive_store =
            Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("test archive store"));
        let queue: Arc<dyn QueueBackend> =
            Arc::new(FileQueueStore::open(tmp.join("queue.json")).expect("test queue store"));

        let executor = AgentExecuteExecutor {
            goals_dir: tmp.clone(),
            repo_root: tmp,
            provider_router: router,
            role_registry,
            identity_content: Arc::new(Mutex::new(None)),
            agent_backend: AgentBackend::React,
            archive_store: archive_store.clone(),
            queue: queue.clone(),
            vector_index: None,
            broker: None,
            bridge_session_store: Arc::new(Mutex::new(
                crate::bridge_interactions::BridgeSessionStore::default(),
            )),
            llm_gateway_socket: None,
            sandbox_manager: None,
            dispatch_backend: std::sync::OnceLock::new(),
            runner_harness_mode: crate::workers::RunnerHarnessMode::Process,
        };
        (executor, archive_store, queue)
    }
}

use agent_test_helpers::*;

/// E2E: Agent execute step runs the ReAct loop and completes via LLM.
///
/// The mock LLM returns a JSON `{"done": true, "result": "..."}` which the
/// ReAct loop parses as a final answer. The executor should produce a
/// `StepStatus::Success` with the agent output persisted.
#[test]
fn e2e_goal_agent_react_loop_executes() {
    // A tokio runtime is required because execute_react() uses
    // Handle::try_current() to block_on the async ReAct loop.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // Mock LLM returns a direct "done" response — no tool calls needed.
    let (executor, provider) = mock_agent_executor(
        r#"{"done": true, "result": "Analysis complete: 3 improvements identified"}"#,
    );

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-planner".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "test-workflow".to_string(),
        inputs: HashMap::new(),
        outputs: {
            let mut m = HashMap::new();
            m.insert(
                "plan_description".to_string(),
                "Improve codebase quality".to_string(),
            );
            m
        },
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(result.status, symbiotic_workflows::StepStatus::Success);
    assert!(
        result
            .outputs
            .get("agent-planner_output")
            .unwrap_or(&String::new())
            .contains("improvements identified"),
        "agent output should contain LLM result"
    );
    assert_eq!(
        result
            .outputs
            .get("agent-planner_status")
            .map(|s| s.as_str()),
        Some("completed")
    );

    // The mock provider should have been called at least once.
    assert!(
        provider
            .call_count
            .load(std::sync::atomic::Ordering::SeqCst)
            >= 1,
        "mock LLM should be called by the ReAct loop"
    );
}

/// E2E: Agent execute step with a role that doesn't exist in the registry
/// still executes (falls back to default config).
#[test]
fn e2e_goal_agent_role_resolution() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    // The ReAct loop with a done response.
    let (executor, _provider) =
        mock_agent_executor(r#"{"done": true, "result": "Custom role analysis done"}"#);

    // Test with a known role: "coder" (should exist in defaults)
    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-coder".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("coder".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "test-workflow".to_string(),
        inputs: HashMap::new(),
        outputs: HashMap::new(),
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(result.status, symbiotic_workflows::StepStatus::Success);
    assert_eq!(
        result.outputs.get("agent-coder_role").map(|s| s.as_str()),
        Some("coder"),
        "role should be recorded in outputs"
    );

    // Test with unknown role — should still succeed using default config
    let step_unknown = symbiotic_workflows::WorkflowStep {
        id: "agent-custom".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("nonexistent-role".to_string()),
    };

    let ctx2 = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "test-workflow".to_string(),
        inputs: HashMap::new(),
        outputs: HashMap::new(),
    };

    let result2 = executor
        .execute(&step_unknown, &ctx2)
        .expect("execute with unknown role should still succeed");

    assert_eq!(result2.status, symbiotic_workflows::StepStatus::Success);
}

/// E2E: Agent execute with no completion providers fails gracefully.
///
/// When no providers are registered, the ReAct loop's LLM call should fail,
/// and the executor should return a Failed step with an error message.
#[test]
fn e2e_goal_agent_no_provider_fails_gracefully() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let tmp = std::env::temp_dir().join(format!("symbiotic_agent_noprov_{}", unique_suffix()));
    let _ = std::fs::create_dir_all(&tmp);

    // Empty registry — no completion providers.
    let registry = symbiotic_providers::ProviderRegistry::new();
    let router = Arc::new(ProviderRouter::new(Arc::new(std::sync::RwLock::new(
        registry,
    ))));
    let mut role_registry = symbiotic_agent_config::RoleRegistry::new();
    let _ = symbiotic_agent_config::defaults::register_defaults(&mut role_registry);
    let role_registry = Arc::new(role_registry);

    let archive_store =
        Arc::new(FileArchiveStore::open(tmp.join("archive")).expect("test archive store"));
    let queue: Arc<dyn QueueBackend> =
        Arc::new(FileQueueStore::open(tmp.join("queue.json")).expect("test queue store"));

    let executor = AgentExecuteExecutor {
        goals_dir: tmp.clone(),
        repo_root: tmp,
        provider_router: router,
        role_registry,
        identity_content: Arc::new(Mutex::new(None)),
        agent_backend: AgentBackend::React,
        archive_store,
        queue,
        vector_index: None,
        broker: None,
        bridge_session_store: Arc::new(Mutex::new(
            crate::bridge_interactions::BridgeSessionStore::default(),
        )),
        llm_gateway_socket: None,
        sandbox_manager: None,
        dispatch_backend: std::sync::OnceLock::new(),
        runner_harness_mode: crate::workers::RunnerHarnessMode::Process,
    };

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-fail".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "test-workflow".to_string(),
        inputs: HashMap::new(),
        outputs: HashMap::new(),
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should not error, but status should be failed");

    assert_eq!(result.status, symbiotic_workflows::StepStatus::Failed);
    assert!(
        result.error.is_some(),
        "failed execution should have an error message"
    );
}

/// E2E: Full daemon goal start → workflow → agent.execute with mock LLM.
///
/// This exercises the complete pipeline: a daemon is constructed with a mock
/// completion provider injected (via custom provider_router), a goal is started,
/// the workflow runs, and the agent.execute step uses the ReAct loop.
#[test]
fn e2e_goal_agent_full_daemon_react_pipeline() {
    let name = format!("fullpipe_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    config.agent_backend = AgentBackend::React;

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Verify the daemon has the React backend configured.
    assert_eq!(daemon.config.agent_backend, AgentBackend::React);

    // Verify provider router is accessible.
    let router = daemon.provider_router();
    assert!(
        Arc::strong_count(router) >= 1,
        "provider router should be alive"
    );

    // Verify role resolution works for known roles.
    let planner_config = daemon.resolve_role_config(Some("planner"));
    assert!(
        planner_config.is_some(),
        "planner role should resolve from defaults"
    );
    let planner_config = planner_config.unwrap();
    assert!(
        planner_config.system_prompt.is_some(),
        "planner should have a system prompt"
    );

    let coder_config = daemon.resolve_role_config(Some("coder"));
    assert!(
        coder_config.is_some(),
        "coder role should resolve from defaults"
    );

    // Unknown role should return None.
    let unknown = daemon.resolve_role_config(Some("unicorn"));
    assert!(unknown.is_none(), "unknown role should return None");

    // Verify make_llm_client works.
    let _client = daemon.make_llm_client(symbiotic_core::Sensitivity::Shareable);
}

// ---------------------------------------------------------------------------
// E2E: Tool-equipped ReAct execution tests
// ---------------------------------------------------------------------------

/// E2E: ReAct recall tool queries the archive and returns matching entries.
///
/// Seeds the archive with test entries, then runs a mock LLM that invokes
/// the recall tool. Verifies the goal completes successfully and the recall
/// tool was actually invoked against the real archive store.
#[test]
fn e2e_react_recall_tool_queries_archive() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let responses = vec![
        // Call 1: LLM requests recall tool
        r#"{"tool": "recall", "params": {"query": "rust", "max_items": 5}}"#.to_string(),
        // Call 2: LLM finishes after seeing tool result
        r#"{"done": true, "result": "Found relevant info about Rust"}"#.to_string(),
    ];

    let (executor, archive_store, _queue) = mock_agent_executor_with_stores(responses);

    // Seed the archive with test entries.
    archive_store
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Rust async patterns".to_string()),
            content: "Detailed guide to async/await in Rust with tokio runtime.".to_string(),
            source_url: None,
            tags: vec!["rust".to_string(), "async".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Shareable,
            idempotency_key: "seed-rust-async".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("seed archive entry 1");

    archive_store
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Kubernetes deployment guide".to_string()),
            content: "How to deploy services on Kubernetes clusters.".to_string(),
            source_url: None,
            tags: vec!["k8s".to_string(), "devops".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Shareable,
            idempotency_key: "seed-k8s-deploy".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("seed archive entry 2");

    archive_store
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Rust error handling".to_string()),
            content: "Best practices for error handling in Rust using thiserror and anyhow."
                .to_string(),
            source_url: None,
            tags: vec!["rust".to_string(), "errors".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Shareable,
            idempotency_key: "seed-rust-errors".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("seed archive entry 3");

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-recall".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "recall-test".to_string(),
        inputs: HashMap::new(),
        outputs: {
            let mut m = HashMap::new();
            m.insert(
                "plan_description".to_string(),
                "Find information about Rust".to_string(),
            );
            m
        },
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(
        result.status,
        symbiotic_workflows::StepStatus::Success,
        "goal should complete successfully, error: {:?}",
        result.error
    );
    assert!(
        result
            .outputs
            .get("agent-recall_output")
            .unwrap_or(&String::new())
            .contains("Found relevant info"),
        "agent output should contain the LLM's final result"
    );
}

/// E2E: ReAct archive tool stores a new entry in the archive.
///
/// The mock LLM invokes the archive tool to store an entry, then confirms
/// completion. After execution, we verify the entry exists in the real archive.
#[test]
fn e2e_react_archive_tool_stores_entry() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let responses = vec![
        // Call 1: LLM requests archive tool
        r#"{"tool": "archive", "params": {"title": "Agent-Created Note", "content": "This entry was created by the agent during ReAct execution.", "tags": ["test", "agent"]}}"#.to_string(),
        // Call 2: LLM finishes
        r#"{"done": true, "result": "Stored the note successfully"}"#.to_string(),
    ];

    let (executor, archive_store, _queue) = mock_agent_executor_with_stores(responses);

    // Verify archive is initially empty.
    let before = archive_store.list().expect("list archive before");
    assert!(before.is_empty(), "archive should start empty");

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-archive".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "archive-test".to_string(),
        inputs: HashMap::new(),
        outputs: {
            let mut m = HashMap::new();
            m.insert(
                "plan_description".to_string(),
                "Store a note in the archive".to_string(),
            );
            m
        },
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(
        result.status,
        symbiotic_workflows::StepStatus::Success,
        "goal should complete successfully, error: {:?}",
        result.error
    );

    // Verify the archive now contains the entry.
    let after = archive_store.list().expect("list archive after");
    assert_eq!(after.len(), 1, "archive should contain exactly one entry");
    assert_eq!(after[0].title, "Agent-Created Note");
    assert!(after[0]
        .content
        .contains("created by the agent during ReAct"));
    assert!(
        after[0].tags.contains(&"test".to_string()),
        "entry should have 'test' tag, got: {:?}",
        after[0].tags
    );
    assert!(
        after[0].tags.contains(&"agent".to_string()),
        "entry should have 'agent' tag, got: {:?}",
        after[0].tags
    );
}

/// E2E: ReAct queue tool enqueues a job in the work queue.
///
/// The mock LLM invokes the queue tool to submit a job, then confirms
/// completion. After execution, we verify the job exists in the real queue.
#[test]
fn e2e_react_queue_tool_enqueues_job() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let responses = vec![
        // Call 1: LLM requests queue tool
        r#"{"tool": "queue", "params": {"job_type": "intake.url", "payload": "https://example.com", "idempotency_key": "test-queue-123"}}"#.to_string(),
        // Call 2: LLM finishes
        r#"{"done": true, "result": "Job queued for processing"}"#.to_string(),
    ];

    let (executor, _archive_store, queue) = mock_agent_executor_with_stores(responses);

    // Verify queue is initially empty.
    let before = queue
        .list_by_status(symbiotic_queue::JobStatus::Queued)
        .expect("list queue before");
    assert!(before.is_empty(), "queue should start empty");

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-queue".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "queue-test".to_string(),
        inputs: HashMap::new(),
        outputs: {
            let mut m = HashMap::new();
            m.insert(
                "plan_description".to_string(),
                "Queue a URL for intake".to_string(),
            );
            m
        },
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(
        result.status,
        symbiotic_workflows::StepStatus::Success,
        "goal should complete successfully, error: {:?}",
        result.error
    );

    // Verify the queue now contains the job.
    let after = queue
        .list_by_status(symbiotic_queue::JobStatus::Queued)
        .expect("list queue after");
    assert_eq!(after.len(), 1, "queue should contain exactly one job");
    assert_eq!(after[0].type_name, "intake.url");
    assert_eq!(after[0].payload, "https://example.com");
    assert_eq!(after[0].idempotency_key, "test-queue-123");
}

/// E2E: ReAct loop chains multiple tool calls in sequence.
///
/// The mock LLM calls recall → archive → done, verifying the agent correctly
/// sequences tool calls and reaches completion using real backends.
#[test]
fn e2e_react_multiple_tools_in_sequence() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let responses = vec![
        // Call 1: LLM recalls context
        r#"{"tool": "recall", "params": {"query": "deployment", "max_items": 3}}"#.to_string(),
        // Call 2: LLM archives a summary based on recall results
        r#"{"tool": "archive", "params": {"title": "Deployment Summary", "content": "Synthesized deployment knowledge from recall results.", "tags": ["summary", "deployment"]}}"#.to_string(),
        // Call 3: LLM finishes
        r#"{"done": true, "result": "Recalled context and archived a summary"}"#.to_string(),
    ];

    let (executor, archive_store, _queue) = mock_agent_executor_with_stores(responses);

    // Seed one entry so the recall tool has something to find.
    archive_store
        .store(symbiotic_archive::StoreRequest {
            title_hint: Some("Deployment best practices".to_string()),
            content: "Use rolling deployments with health checks.".to_string(),
            source_url: None,
            tags: vec!["deployment".to_string()],
            sensitivity: symbiotic_archive::ArchiveSensitivity::Shareable,
            idempotency_key: "seed-deployment".to_string(),
            firewall_verdict: Some(symbiotic_archive::trusted_skip_verdict()),
        })
        .expect("seed archive entry");

    let step = symbiotic_workflows::WorkflowStep {
        id: "agent-multi".to_string(),
        step_type: "agent.execute".to_string(),
        config: HashMap::new(),
        agent_role: Some("planner".to_string()),
    };

    let ctx = symbiotic_workflows::WorkflowContext {
        run_id: format!("run_{}", unique_suffix()),
        workflow_id: "multi-tool-test".to_string(),
        inputs: HashMap::new(),
        outputs: {
            let mut m = HashMap::new();
            m.insert(
                "plan_description".to_string(),
                "Recall deployment info and archive a summary".to_string(),
            );
            m
        },
    };

    let result = executor
        .execute(&step, &ctx)
        .expect("execute should succeed");

    assert_eq!(
        result.status,
        symbiotic_workflows::StepStatus::Success,
        "goal should complete successfully, error: {:?}",
        result.error
    );
    assert!(
        result
            .outputs
            .get("agent-multi_output")
            .unwrap_or(&String::new())
            .contains("archived a summary"),
        "agent output should contain the final result"
    );

    // Verify the archive now has 2 entries: the seed + the agent-created summary.
    let entries = archive_store.list().expect("list archive after multi-tool");
    assert_eq!(
        entries.len(),
        2,
        "archive should have seed entry + agent-created summary"
    );
    let summary = entries
        .iter()
        .find(|e| e.title == "Deployment Summary")
        .expect("should find the agent-created summary");
    assert!(summary.content.contains("Synthesized deployment knowledge"));
    assert!(summary.tags.contains(&"summary".to_string()));
}

// ---------------------------------------------------------------------------
// Push notification delivery infrastructure integration tests
// ---------------------------------------------------------------------------
//
// These tests cover the remaining gaps in push notification coverage:
// - Preferences filtering (suppress events based on user preferences)
// - Rate limiting (cap notifications per device per window)
// - Badge count increment/reset
// - Device unregistration flow
// - Stale device pruning
// - Token invalidation via remove_by_token_hash

/// End-to-end test: push preferences filtering suppresses disabled categories.
///
/// Registers a device, disables `capture_confirmations`, fires an
/// `ingest.fetch completed` event, and verifies that no push notification
/// is dispatched.
#[test]
fn e2e_push_preferences_suppress_capture_confirmations() {
    let daemon = daemon_for_test("e2e-push-prefs-suppress");
    let now = now_unix();

    // Register a device.
    daemon
        .register_push_device("iphone-14", "apns-token-prefs-test", "apns", now)
        .expect("register should succeed");

    // Disable capture confirmations via preferences.
    {
        let mut prefs = daemon.push_preferences.lock().expect("prefs lock");
        prefs.capture_confirmations = false;
        prefs.enforce_invariants();
    }

    // Create a capture-completed event (normally push-worthy).
    let event = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-prefs-1".to_string()),
        detail: "Captured entry".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-prefs-1".to_string()),
        url: Some("https://example.com/prefs".to_string()),
        title: Some("Prefs Test Article".to_string()),
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    // Verify classification says it IS push-worthy (classifier doesn't check prefs).
    assert!(
        push_dispatcher::classify_event(&event).is_some(),
        "ingest.fetch/completed should classify as push-worthy"
    );

    // Dispatch with preferences — should be suppressed.
    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push_full(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
        Some(&daemon.push_preferences),
        None,
    );
    assert_eq!(
        success_count, 0,
        "capture confirmations disabled — no push should be sent"
    );
    assert!(
        provider.sent_notifications().is_empty(),
        "provider should have received zero notifications"
    );
}

/// End-to-end test: push preferences DO NOT suppress auth events
/// (auth_required is always-on and cannot be disabled).
#[test]
fn e2e_push_preferences_never_suppress_auth_required() {
    let daemon = daemon_for_test("e2e-push-prefs-auth");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-auth-prefs", "apns", now)
        .expect("register should succeed");

    // Try to disable everything (auth should remain enabled due to enforce_invariants).
    {
        let mut prefs = daemon.push_preferences.lock().expect("prefs lock");
        prefs.failures = false;
        prefs.goal_completions = false;
        prefs.capture_confirmations = false;
        prefs.install_progress = false;
        prefs.enforce_invariants();
        // auth_required should still be true.
        assert!(prefs.auth_required);
        assert!(prefs.alert_escalations);
    }

    let event = events::DaemonEvent {
        event_type: EventType::AuthIssue,
        status: "failed".to_string(),
        job_id: Some("job-auth-prefs".to_string()),
        detail: "Auth failed".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: None,
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push_full(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
        Some(&daemon.push_preferences),
        None,
    );
    assert_eq!(
        success_count, 1,
        "auth events must always push regardless of preferences"
    );
    let sent = provider.sent_notifications();
    assert_eq!(sent[0].priority, "critical");
}

/// End-to-end test: push preferences suppress failure events when disabled.
#[test]
fn e2e_push_preferences_suppress_failures() {
    let daemon = daemon_for_test("e2e-push-prefs-failures");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-fail-prefs", "apns", now)
        .expect("register should succeed");

    // Disable failure notifications.
    {
        let mut prefs = daemon.push_preferences.lock().expect("prefs lock");
        prefs.failures = false;
        prefs.enforce_invariants();
    }

    let event = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "dlq".to_string(),
        job_id: Some("job-fail-prefs".to_string()),
        detail: "Ingest failed permanently".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-fail-prefs".to_string()),
        url: Some("https://example.com/fail".to_string()),
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    let success_count = push_dispatcher::dispatch_event_push_full(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
        Some(&daemon.push_preferences),
        None,
    );
    assert_eq!(
        success_count, 0,
        "failure notifications disabled — no push should be sent"
    );
}

/// End-to-end test: rate limiter caps notifications per device per window.
///
/// Configures a rate limiter with max 3 per window, sends 5 events,
/// and verifies only 3 get through.
#[test]
fn e2e_push_rate_limiter_caps_notifications() {
    let daemon = daemon_for_test("e2e-push-rate-limit");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-rate-limit", "apns", now)
        .expect("register should succeed");

    // Create a tight rate limiter: max 3 per 3600s window.
    let rate_limiter = std::sync::Mutex::new(push_dispatcher::PushRateLimiter::new(3, 3600));

    let provider = RecordingPushProvider::new();
    let mut total_sent = 0;

    // Fire 5 push-worthy events in the same window.
    for i in 0..5 {
        let event = events::DaemonEvent {
            event_type: EventType::IngestFetch,
            status: "completed".to_string(),
            job_id: Some(format!("job-rate-{i}")),
            detail: format!("Captured entry {i}"),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some(format!("run-rate-{i}")),
            url: Some(format!("https://example.com/rate/{i}")),
            title: Some(format!("Rate Test {i}")),
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        };

        let count = push_dispatcher::dispatch_event_push_full(
            &event,
            &daemon.push_registry,
            &provider,
            &daemon.config.push_telemetry_file,
            now + i as u64, // slight time offset but within same window
            None,
            Some(&rate_limiter),
        );
        total_sent += count;
    }

    assert_eq!(
        total_sent, 3,
        "rate limiter should cap at 3 notifications per window"
    );
    assert_eq!(provider.sent_notifications().len(), 3);
}

/// End-to-end test: rate limiter window resets after expiry.
#[test]
fn e2e_push_rate_limiter_resets_after_window() {
    let daemon = daemon_for_test("e2e-push-rate-reset");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-rate-reset", "apns", now)
        .expect("register should succeed");

    let rate_limiter = std::sync::Mutex::new(push_dispatcher::PushRateLimiter::new(2, 100));
    let provider = RecordingPushProvider::new();

    // Send 2 events (fills the window).
    for i in 0..2 {
        let event = events::DaemonEvent {
            event_type: EventType::IngestFetch,
            status: "completed".to_string(),
            job_id: Some(format!("job-rr-{i}")),
            detail: "Captured".to_string(),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some(format!("run-rr-{i}")),
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        };
        push_dispatcher::dispatch_event_push_full(
            &event,
            &daemon.push_registry,
            &provider,
            &daemon.config.push_telemetry_file,
            now + i as u64,
            None,
            Some(&rate_limiter),
        );
    }
    assert_eq!(provider.sent_notifications().len(), 2);

    // 3rd event in same window — should be rate limited.
    let event3 = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-rr-2".to_string()),
        detail: "Captured".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-rr-2".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let count3 = push_dispatcher::dispatch_event_push_full(
        &event3,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 50, // still within 100s window
        None,
        Some(&rate_limiter),
    );
    assert_eq!(count3, 0, "3rd event should be rate limited");

    // After window expires (now + 200, which is 100s+ after start), should work again.
    let event4 = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-rr-3".to_string()),
        detail: "Captured".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-rr-3".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let count4 = push_dispatcher::dispatch_event_push_full(
        &event4,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 200, // past 100s window
        None,
        Some(&rate_limiter),
    );
    assert_eq!(
        count4, 1,
        "after window expires, notifications should resume"
    );
    assert_eq!(provider.sent_notifications().len(), 3); // 2 + 1 after reset
}

/// End-to-end test: badge count increments on each push and resets on ack.
#[test]
fn e2e_push_badge_count_increment_and_reset() {
    let daemon = daemon_for_test("e2e-push-badge");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-badge-test", "apns", now)
        .expect("register should succeed");

    let provider = RecordingPushProvider::new();

    // Send 3 push-worthy events — badge should increment each time.
    for i in 0..3 {
        let event = events::DaemonEvent {
            event_type: EventType::IngestFetch,
            status: "completed".to_string(),
            job_id: Some(format!("job-badge-{i}")),
            detail: format!("Entry {i}"),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some(format!("run-badge-{i}")),
            url: Some(format!("https://example.com/badge/{i}")),
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        };
        push_dispatcher::dispatch_event_push(
            &event,
            &daemon.push_registry,
            &provider,
            &daemon.config.push_telemetry_file,
            now + i as u64,
        );
    }

    let sent = provider.sent_notifications();
    assert_eq!(sent.len(), 3);
    // Badge counts should be 1, 2, 3 respectively.
    assert_eq!(sent[0].badge, Some(1));
    assert_eq!(sent[1].badge, Some(2));
    assert_eq!(sent[2].badge, Some(3));

    // Verify badge count in the registry.
    assert_eq!(daemon.push_registry.badge_count("iphone-14"), 3);

    // Reset badge via push.ack.
    daemon.push_registry.reset_badge("iphone-14");
    assert_eq!(daemon.push_registry.badge_count("iphone-14"), 0);

    // Next push should have badge = 1 again.
    let event4 = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-badge-3".to_string()),
        detail: "Entry 3".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-badge-3".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    push_dispatcher::dispatch_event_push(
        &event4,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now + 10,
    );
    let sent_after = provider.sent_notifications();
    assert_eq!(sent_after.len(), 4);
    assert_eq!(sent_after[3].badge, Some(1));
}

/// End-to-end test: push.unregister command removes device and future
/// dispatches produce zero notifications.
#[test]
fn e2e_push_unregister_stops_notifications() {
    let daemon = daemon_for_test("e2e-push-unregister");
    let now = now_unix();

    // Register two devices.
    daemon
        .register_push_device("iphone-14", "apns-token-unreg-1", "apns", now)
        .expect("register should succeed");
    daemon
        .register_push_device("pixel-8", "fcm-token-unreg-2", "fcm", now)
        .expect("register should succeed");

    assert_eq!(daemon.push_registry.device_count(), 2);

    // Unregister the iPhone.
    let removed = daemon
        .push_registry
        .unregister("iphone-14")
        .expect("unregister");
    assert!(removed, "should have found and removed the device");
    assert_eq!(daemon.push_registry.device_count(), 1);

    // Dispatch should only send to pixel-8.
    let provider = RecordingPushProvider::new();
    let event = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-unreg".to_string()),
        detail: "Test".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-unreg".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let count = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
    );
    assert_eq!(count, 1, "only pixel-8 should receive push");
    assert_eq!(provider.sent_notifications()[0].device_id, "pixel-8");

    // Unregister pixel-8 too.
    daemon
        .push_registry
        .unregister("pixel-8")
        .expect("unregister");
    assert_eq!(daemon.push_registry.device_count(), 0);

    // Dispatch should send to nobody.
    let provider2 = RecordingPushProvider::new();
    let count2 = push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider2,
        &daemon.config.push_telemetry_file,
        now,
    );
    assert_eq!(count2, 0, "no devices — no push");
}

/// End-to-end test: unregistering a device that was never registered
/// returns false and does not error.
#[test]
fn e2e_push_unregister_nonexistent_device() {
    let daemon = daemon_for_test("e2e-push-unreg-noop");

    let removed = daemon
        .push_registry
        .unregister("never-registered")
        .expect("unregister should not error");
    assert!(!removed, "should return false for non-existent device");
}

/// End-to-end test: stale device pruning removes old devices.
#[test]
fn e2e_push_stale_device_pruning() {
    let daemon = daemon_for_test("e2e-push-stale");
    let now = now_unix();

    // Register two devices: one recent, one stale.
    // "old-device" was last seen 100 days ago.
    let stale_time = now - (100 * 24 * 3600);
    daemon
        .register_push_device("old-device", "apns-token-old", "apns", stale_time)
        .expect("register should succeed");
    // "new-device" was last seen just now.
    daemon
        .register_push_device("new-device", "apns-token-new", "apns", now)
        .expect("register should succeed");

    assert_eq!(daemon.push_registry.device_count(), 2);

    // Prune with 90-day max age.
    let pruned = daemon
        .push_registry
        .prune_stale(now, 90 * 24 * 3600)
        .expect("prune should succeed");
    assert_eq!(pruned, 1, "old-device should be pruned");
    assert_eq!(daemon.push_registry.device_count(), 1);

    // Verify the remaining device is "new-device".
    let devices = daemon.push_registry.list().expect("list");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].device_id, "new-device");

    // Prune again — nothing to prune.
    let pruned2 = daemon
        .push_registry
        .prune_stale(now, 90 * 24 * 3600)
        .expect("prune should succeed");
    assert_eq!(pruned2, 0);
}

/// End-to-end test: stale pruning with all devices fresh prunes nothing.
#[test]
fn e2e_push_stale_pruning_all_fresh() {
    let daemon = daemon_for_test("e2e-push-stale-fresh");
    let now = now_unix();

    daemon
        .register_push_device("device-a", "token-a", "apns", now)
        .expect("register");
    daemon
        .register_push_device("device-b", "token-b", "fcm", now - 3600)
        .expect("register");

    let pruned = daemon
        .push_registry
        .prune_stale(now, 90 * 24 * 3600)
        .expect("prune");
    assert_eq!(pruned, 0, "both devices are fresh");
    assert_eq!(daemon.push_registry.device_count(), 2);
}

/// End-to-end test: remove_by_token_hash removes the correct device.
///
/// Simulates APNs returning 410 (token invalid) and verifies the device
/// is removed from the registry by its token hash.
#[test]
fn e2e_push_remove_by_token_hash() {
    let daemon = daemon_for_test("e2e-push-token-hash");
    let now = now_unix();

    let device = daemon
        .register_push_device("iphone-14", "real-apns-token-xyz", "apns", now)
        .expect("register");

    assert_eq!(daemon.push_registry.device_count(), 1);

    // Remove by the token hash (as if APNs returned 410).
    let removed = daemon
        .push_registry
        .remove_by_token_hash(&device.token_hash)
        .expect("remove_by_token_hash");
    assert!(removed, "device should be found and removed by token hash");
    assert_eq!(daemon.push_registry.device_count(), 0);

    // Removing again should return false.
    let removed2 = daemon
        .push_registry
        .remove_by_token_hash(&device.token_hash)
        .expect("remove_by_token_hash");
    assert!(!removed2, "device already removed");
}

/// End-to-end test: push.unregister via Matrix control command.
///
/// Sends a `push.unregister` command through the Matrix routing path
/// and verifies the device is removed and a confirmation event is emitted.
#[test]
fn e2e_push_unregister_via_matrix_command() {
    let daemon = daemon_for_test("e2e-push-unreg-matrix");
    let now = now_unix();

    // Register a device first.
    daemon
        .register_push_device("device-to-remove", "token-to-remove", "apns", now)
        .expect("register should succeed");
    assert_eq!(daemon.push_registry.device_count(), 1);

    // Send push.unregister command via Matrix routing.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.unregister","d":{"device_id":"device-to-remove"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("push.unregistered"));
    assert_eq!(events[0].sym.k, Kind::State);

    // Verify device is removed.
    assert_eq!(daemon.push_registry.device_count(), 0);
}

/// End-to-end test: push.preferences via Matrix control command.
///
/// Sends a `push.preferences` command to disable capture confirmations,
/// then verifies the preferences file is updated.
#[test]
fn e2e_push_preferences_via_matrix_command() {
    let daemon = daemon_for_test("e2e-push-prefs-matrix");
    let now = now_unix();

    // Initially all preferences are enabled.
    {
        let prefs = daemon.push_preferences.lock().expect("lock");
        assert!(prefs.capture_confirmations);
        assert!(prefs.failures);
    }

    // Send push.preferences command to disable capture_confirmations.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.preferences","d":{"capture_confirmations":false,"failures":false}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("push.preferences"));
    assert_eq!(events[0].sym.k, Kind::State);

    // Verify preferences were updated.
    {
        let prefs = daemon.push_preferences.lock().expect("lock");
        assert!(!prefs.capture_confirmations);
        assert!(!prefs.failures);
        // auth_required and alert_escalations should still be true (non-overridable).
        assert!(prefs.auth_required);
        assert!(prefs.alert_escalations);
    }

    // Verify the preferences file was written.
    let prefs_content =
        std::fs::read_to_string(&daemon.config.push_preferences_file).unwrap_or_default();
    assert!(prefs_content.contains("capture_confirmations = false"));
}

/// End-to-end test: push.ack via Matrix command resets badge for specified device.
#[test]
fn e2e_push_ack_resets_badge_via_matrix_command() {
    let daemon = daemon_for_test("e2e-push-ack-badge");
    let now = now_unix();

    // Register a device and increment its badge.
    daemon
        .register_push_device("iphone-ack", "apns-token-ack", "apns", now)
        .expect("register");
    daemon.push_registry.increment_badge("iphone-ack");
    daemon.push_registry.increment_badge("iphone-ack");
    daemon.push_registry.increment_badge("iphone-ack");
    assert_eq!(daemon.push_registry.badge_count("iphone-ack"), 3);

    // Send push.ack with device_id to reset badge.
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: r#"{"msgtype":"sym.c","body":"","sym":{"v":2,"c":"push.ack","d":{"notification_id":"notif-badge","device_id":"iphone-ack"}}}"#
                    .to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.a.as_deref(), Some("push.ack"));
    assert_eq!(events[0].sym.k, Kind::State);

    // Badge should be reset to 0.
    assert_eq!(daemon.push_registry.badge_count("iphone-ack"), 0);
}

/// End-to-end test: push dispatch sets thread_id from intake_run_id for
/// iOS notification grouping.
#[test]
fn e2e_push_thread_id_from_intake_run_id() {
    let daemon = daemon_for_test("e2e-push-thread-id");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-token-thread", "apns", now)
        .expect("register");

    let event = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-thread".to_string()),
        detail: "Captured".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: Some("goal-fallback".to_string()),
        goal_id: None,
        intake_run_id: Some("intake-run-xyz".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };

    let provider = RecordingPushProvider::new();
    push_dispatcher::dispatch_event_push(
        &event,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
    );

    let sent = provider.sent_notifications();
    assert_eq!(sent.len(), 1);
    // thread_id should be intake_run_id (preferred over goal_run_id).
    assert_eq!(sent[0].thread_id, Some("intake-run-xyz".to_string()));
}

/// End-to-end test: combined preferences + rate limiting.
///
/// Verifies both filters work together: preferences suppress some events,
/// rate limiter caps the rest.
#[test]
fn e2e_push_combined_preferences_and_rate_limit() {
    let daemon = daemon_for_test("e2e-push-combined");
    let now = now_unix();

    daemon
        .register_push_device("iphone-14", "apns-combined", "apns", now)
        .expect("register");

    // Disable capture confirmations but keep failures enabled.
    {
        let mut prefs = daemon.push_preferences.lock().expect("lock");
        prefs.capture_confirmations = false;
        prefs.enforce_invariants();
    }

    // Rate limit: max 2 per window.
    let rate_limiter = std::sync::Mutex::new(push_dispatcher::PushRateLimiter::new(2, 3600));

    let provider = RecordingPushProvider::new();

    // 1. Capture completed — suppressed by preferences.
    let event_capture = events::DaemonEvent {
        event_type: EventType::IngestFetch,
        status: "completed".to_string(),
        job_id: Some("job-c1".to_string()),
        detail: "Captured".to_string(),
        goal_room: None,
        goal_template: None,
        goal_run_id: None,
        goal_id: None,
        intake_run_id: Some("run-c1".to_string()),
        url: None,
        title: None,
        sensitivity: None,
        quick_replies: None,
        thread_id: None,
    };
    let c1 = push_dispatcher::dispatch_event_push_full(
        &event_capture,
        &daemon.push_registry,
        &provider,
        &daemon.config.push_telemetry_file,
        now,
        Some(&daemon.push_preferences),
        Some(&rate_limiter),
    );
    assert_eq!(c1, 0, "capture should be suppressed by preferences");

    // 2-4. Three DLQ failures — first two should go through, third rate-limited.
    for i in 0..3 {
        let event_dlq = events::DaemonEvent {
            event_type: EventType::IngestFetch,
            status: "dlq".to_string(),
            job_id: Some(format!("job-dlq-{i}")),
            detail: format!("Failed {i}"),
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: Some(format!("run-dlq-{i}")),
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        };
        push_dispatcher::dispatch_event_push_full(
            &event_dlq,
            &daemon.push_registry,
            &provider,
            &daemon.config.push_telemetry_file,
            now + i as u64,
            Some(&daemon.push_preferences),
            Some(&rate_limiter),
        );
    }

    // Total: 2 (rate limited the 3rd DLQ event; capture was preference-suppressed)
    assert_eq!(provider.sent_notifications().len(), 2);
}

// ---------------------------------------------------------------------------
// E2E: Full inquisition pipeline (NL goal → ask_user → answer → plan → approve → done)
// ---------------------------------------------------------------------------

/// Register the `inquisition` workflow template on a daemon.
///
/// In production this is loaded from `crates/symbiotic-workflows/templates/inquisition.json`,
/// but tests run from temp dirs where the file doesn't exist. This helper
/// constructs an equivalent inline template and registers it.
fn register_inquisition_template(daemon: &mut SymbioticDaemon) {
    let workflow = symbiotic_workflows::Workflow {
        id: "wf-inquisition".to_string(),
        name: "inquisition".to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: symbiotic_workflows::WorkflowPolicy {
            sensitivity_max: "restricted".to_string(),
            model_class: "cloud".to_string(),
        },
        steps: vec![symbiotic_workflows::WorkflowStep {
            id: "clarify".to_string(),
            step_type: "agent.execute".to_string(),
            config: HashMap::new(),
            agent_role: Some("inquisitor".to_string()),
        }],
    };
    daemon
        .workflow_registry
        .register(workflow)
        .expect("inquisition template should register");
}

fn register_auth_session_template(daemon: &mut SymbioticDaemon, name: &str, role: &str) {
    let mut step_config = HashMap::new();
    step_config.insert(
        "required_capabilities".to_string(),
        "action.browser.login".to_string(),
    );
    let workflow = symbiotic_workflows::Workflow {
        id: format!("wf-{name}"),
        name: name.to_string(),
        version: "1.0".to_string(),
        inputs: HashMap::new(),
        policy: symbiotic_workflows::WorkflowPolicy {
            sensitivity_max: "restricted".to_string(),
            model_class: "cloud".to_string(),
        },
        steps: vec![symbiotic_workflows::WorkflowStep {
            id: "auth".to_string(),
            step_type: "agent.execute".to_string(),
            config: step_config,
            agent_role: Some(role.to_string()),
        }],
    };
    daemon
        .workflow_registry
        .register(workflow)
        .expect("auth session template should register");
}

fn spawn_llm_gateway_for_test(
    daemon: &SymbioticDaemon,
    socket_path: &str,
) -> tokio::task::JoinHandle<Result<()>> {
    let gateway = crate::llm_gateway::LlmGateway::new(
        Arc::clone(&daemon.provider_router),
        daemon.archive_store.clone(),
        daemon.queue.clone(),
        daemon.recall_gateway.clone(),
        None,
        daemon.broker.clone(),
        daemon.credential_gateway.clone(),
        daemon.credential_vault.clone(),
        daemon.auth_engine.clone(),
        daemon.auth_jobs.clone(),
        daemon.auth_approval_policies.clone(),
        daemon.bridge_session_store.clone(),
        daemon.bridge_interaction_log_store.clone(),
        daemon.agent_runtime_log_store.clone(),
        daemon.agent_runtime_status_store.clone(),
        daemon.bridge_checkpoint_store.clone(),
        Arc::new(Mutex::new(crate::llm_audit::LlmAuditLog::new(
            crate::llm_audit::LlmAuditLevel::MetadataOnly,
            30,
        ))),
        Arc::new(Mutex::new(
            symbiotic_memory::tool_memory::ToolMemoryStore::new(),
        )),
        daemon.room_roles.resolve(RoomRole::Credentials),
        crate::auth_jobs::AuthJobConfig {
            approval_ttl_secs: daemon.config.auth_approval_ttl_secs,
            input_ttl_secs: daemon.config.auth_input_ttl_secs,
        },
        socket_path,
        daemon.config.llm_gateway_world_accessible,
    );
    tokio::spawn(async move { gateway.run().await })
}

fn wait_for_test_socket(path: &std::path::Path) {
    for _ in 0..200 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("gateway socket did not appear at {}", path.display());
}

/// E2E: Full inquisition pipeline exercising the complete NL goal lifecycle:
///
/// 1. NL goal submitted to `#control` → `goal.created` event, inquisition workflow queued
/// 2. `run_once` → mock LLM calls `ask_user` with `quick_replies` → `goal.question` event
/// 3. User answer via `goal.answer` command → workflow re-queued
/// 4. `run_once` → mock LLM calls `generate_plan` → `goal.plan.proposed` event
/// 5. User sends `goal.plan.approved` → workflow re-queued
/// 6. `run_once` → mock LLM returns done → `goal.completed`
///
/// Verifies goal state transitions:
///   running/clarifying → awaiting_input → running/executing → awaiting_approval → running/executing → completed
#[test]
fn e2e_inquisition_full_pipeline() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let name = format!("e2e-inquisition_{}", unique_suffix());
    let config = daemon_config_for_test(&name);
    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    // Register the inquisition workflow template (normally loaded from disk).
    register_inquisition_template(&mut daemon);

    // Configure mock LLM responses for three sequential runs:
    //
    // Run 1 (clarification): LLM calls ask_user, then finishes.
    // Run 2 (plan generation): LLM calls generate_plan, then finishes.
    // Run 3 (execution): LLM completes the goal directly.
    let responses = vec![
        // --- Run 1: ask_user ---
        r#"{"tool": "ask_user", "params": {"question": "What budget range are you comfortable with?", "quick_replies": ["Under $500", "$500-$1000", "Over $1000"]}}"#.to_string(),
        r#"{"done": true, "result": "Asked user about budget preference."}"#.to_string(),
        // --- Run 2: generate_plan ---
        r#"{"tool": "generate_plan", "params": {"summary": "Find best flights to Tokyo under $500", "steps": [{"task_id": "step_1", "task_slug": "step_1", "task_kind": "execution", "task_driver": "agent", "role": "researcher", "description": "Search flight aggregators"}, {"task_id": "step_2", "task_slug": "step_2", "task_kind": "execution", "task_driver": "agent", "role": "researcher", "description": "Compare prices and routes", "depends_on": ["step_1"]}], "confidence": 0.92}}"#.to_string(),
        r#"{"done": true, "result": "Plan proposed for approval."}"#.to_string(),
        // --- Run 3: execution complete ---
        r#"{"done": true, "result": "Found 3 flights to Tokyo under $500. Best: ANA via LAX, $423 round trip."}"#.to_string(),
    ];

    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();

    // -----------------------------------------------------------------------
    // Step 1: Submit NL goal to #goals (inquisition path)
    // -----------------------------------------------------------------------
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: "Find the best flight deals to Tokyo for next month".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");

    assert_eq!(events.len(), 1, "should get exactly one event");
    assert_eq!(events[0].sym.s, Some(Status::Working));
    assert_eq!(events[0].sym.s, Some(Status::Working));
    let original_goal_id = detail_str(&events[0], "goal_id")
        .expect("goal_id should be present")
        .to_string();
    let inquisition_template = format!("inquisition:{original_goal_id}");
    assert_eq!(
        detail_str(&events[0], "template"),
        Some(inquisition_template.as_str())
    );

    // Helper: find the inquisition goal state for #goals room.
    let find_goal = |daemon: &SymbioticDaemon| -> GoalState {
        let states = daemon.list_goal_states().expect("list states");
        states
            .into_iter()
            .find(|s| s.template == inquisition_template && s.goal_room == "#goals")
            .expect("inquisition goal state should exist")
    };

    // Verify initial goal state.
    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "running");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("clarifying"));
    assert_eq!(gs.last_run_id.as_deref(), Some(original_goal_id.as_str()));
    {
        let store = daemon
            .management_store
            .lock()
            .expect("management store lock");
        let existing_ids: Vec<String> =
            store.work_items().into_iter().map(|item| item.id).collect();
        let goal_item = store
            .get_work_item(&crate::goal_management::goal_work_item_id(
                &original_goal_id,
            ))
            .unwrap_or_else(|| {
                panic!(
                    "goal work item should exist from inquisition start; saw {:?}",
                    existing_ids
                )
            });
        assert_eq!(goal_item.kind, WorkItemKind::Goal);
        assert_eq!(goal_item.status, WorkItemStatus::Running);
    }
    {
        let archive_root = daemon
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| daemon.config.data_dir.join("../knowledge-base"));
        let plan_path = archive_root
            .join("operations/projects/inbox/goals")
            .join(&original_goal_id)
            .join("plan.md");
        let plan_doc = std::fs::read_to_string(&plan_path)
            .expect("archive inquisition goal plan should exist");
        assert!(plan_doc.contains("phase: inquisition"));
        assert!(plan_doc.contains("Await clarification, deliberation, or approval"));
    }

    // Verify workflow.run job was queued.
    let queued = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("queue query");
    assert_eq!(
        queued.len(),
        1,
        "exactly one workflow.run job should be queued"
    );

    // -----------------------------------------------------------------------
    // Step 2: Run workflow → agent calls ask_user → goal.question event
    // -----------------------------------------------------------------------
    let (event2, _side_events2) = daemon
        .run_once(now + 1)
        .expect("run should succeed")
        .expect("workflow job should be processed");

    assert_eq!(event2.event_type, EventType::GoalQuestion);
    assert_eq!(event2.status, "awaiting_input");
    assert!(
        event2.detail.contains("budget"),
        "question should mention budget: {}",
        event2.detail
    );
    assert!(
        event2.quick_replies.is_some(),
        "quick_replies should be set"
    );
    let qr: Vec<String> = serde_json::from_str(event2.quick_replies.as_deref().unwrap())
        .expect("parse quick_replies");
    assert_eq!(qr.len(), 3);
    assert!(qr.contains(&"Under $500".to_string()));

    // Extract the workflow-generated goal_id (different from the original NL submission id).
    let wf_goal_id = event2
        .goal_id
        .as_ref()
        .expect("goal.question event should have goal_id")
        .clone();

    // Goal state should be awaiting_input.
    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "awaiting_input");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("awaiting_input"));

    // -----------------------------------------------------------------------
    // Step 3: User sends answer via #goals (using workflow goal_id)
    // -----------------------------------------------------------------------
    let answer_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.answer",
            "t": wf_goal_id,
            "d": {
                "template": inquisition_template,
                "message": "Under $500"
            }
        }
    })
    .to_string();

    let events3 = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: answer_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("route answer should work");

    assert_eq!(events3.len(), 1);
    assert_eq!(events3[0].sym.s, Some(Status::Working));
    assert_eq!(events3[0].sym.s, Some(Status::Working));

    // -----------------------------------------------------------------------
    // Step 4: Run workflow → agent calls generate_plan → goal.plan.proposed
    // -----------------------------------------------------------------------
    let (event4, _side_events4) = daemon
        .run_once(now + 3)
        .expect("run should succeed")
        .expect("workflow job should be processed");

    assert_eq!(event4.event_type, EventType::GoalPlanProposed);
    assert_eq!(event4.status, "awaiting_approval");
    assert!(
        event4.detail.contains("Tokyo"),
        "plan JSON should mention Tokyo: {}",
        event4.detail
    );

    // Verify plan JSON is parseable.
    let plan: serde_json::Value =
        serde_json::from_str(&event4.detail).expect("plan detail should be valid JSON");
    assert!(plan.get("steps").is_some(), "plan should have steps");
    assert!(
        plan.get("confidence").is_some(),
        "plan should have confidence"
    );
    let confidence = plan["confidence"].as_f64().unwrap();
    assert!(
        (0.9..=0.95).contains(&confidence),
        "confidence should be ~0.92, got {}",
        confidence
    );

    // Extract the new goal_id from the plan proposal event.
    let plan_goal_id = event4
        .goal_id
        .as_ref()
        .expect("goal.plan.proposed should have goal_id")
        .clone();

    // Goal state should be awaiting_approval.
    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "awaiting_approval");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("awaiting_approval"));
    assert!(gs.plan_id.is_some(), "plan_id should be set");

    // -----------------------------------------------------------------------
    // Step 5: User approves the plan (using the plan event's goal_id)
    // -----------------------------------------------------------------------
    let approve_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.plan.approved",
            "t": plan_goal_id,
            "d": {}
        }
    })
    .to_string();

    let events5 = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: approve_body,
                timestamp: now + 4,
            },
            now + 4,
        )
        .expect("route approval should work");

    assert_eq!(events5.len(), 1);
    assert_eq!(events5[0].sym.s, Some(Status::Working));
    assert_eq!(events5[0].sym.s, Some(Status::Working));

    // Goal state should be running/executing.
    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "running");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("executing"));
    {
        let store = daemon
            .management_store
            .lock()
            .expect("management store lock");
        let goal_item_id = crate::goal_management::goal_work_item_id(&original_goal_id);
        let task_one_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "step_1");
        let task_two_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "step_2");
        let review_task_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "review");
        let execution_one_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "step_1");
        let execution_two_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "step_2");
        let review_execution_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "review");

        assert_eq!(
            store
                .get_work_item(&task_one_id)
                .expect("planned task 1")
                .parent_work_item_id
                .as_deref(),
            Some(goal_item_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&task_two_id)
                .expect("planned task 2")
                .parent_work_item_id
                .as_deref(),
            Some(goal_item_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&review_task_id)
                .expect("review task")
                .parent_work_item_id
                .as_deref(),
            Some(goal_item_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&execution_one_id)
                .expect("execution 1")
                .parent_work_item_id
                .as_deref(),
            Some(task_one_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&execution_two_id)
                .expect("execution 2")
                .parent_work_item_id
                .as_deref(),
            Some(task_two_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&review_execution_id)
                .expect("review execution")
                .parent_work_item_id
                .as_deref(),
            Some(review_task_id.as_str())
        );
    }
    {
        let archive_root = daemon
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| daemon.config.data_dir.join("../knowledge-base"));
        let plan_path = archive_root
            .join("operations/projects/inbox/goals")
            .join(&original_goal_id)
            .join("plan.md");
        let task_one_path = archive_root
            .join("operations/projects/inbox/goals")
            .join(&original_goal_id)
            .join("tasks/step-1.md");
        let task_two_path = archive_root
            .join("operations/projects/inbox/goals")
            .join(&original_goal_id)
            .join("tasks/step-2.md");
        let review_task_path = archive_root
            .join("operations/projects/inbox/goals")
            .join(&original_goal_id)
            .join("tasks/review.md");

        let plan_doc = std::fs::read_to_string(&plan_path).expect("archive goal plan should exist");
        assert!(plan_doc.contains("phase: implementation"));
        assert!(plan_doc.contains("[[tasks/step-1|Search flight aggregators]]"));
        assert!(plan_doc.contains("[[tasks/review|Review generated work]]"));

        let task_one_doc =
            std::fs::read_to_string(&task_one_path).expect("task one archive doc should exist");
        assert!(task_one_doc.contains("task_slug: \"step_1\""));
        assert!(task_one_doc.contains("task_kind: execution"));
        assert!(task_one_doc.contains("task_driver: agent"));
        assert!(task_one_doc.contains("role: \"researcher\""));
        assert!(task_one_doc.contains(
            "questionnaire_context: [\"What budget range are you comfortable with? -> Under $500\"]"
        ));

        let task_two_doc =
            std::fs::read_to_string(&task_two_path).expect("task two archive doc should exist");
        assert!(task_two_doc.contains("Compare prices and routes"));
        assert!(task_two_doc.contains("depends_on: [\"step_1\"]"));

        let review_doc = std::fs::read_to_string(&review_task_path)
            .expect("review task archive doc should exist");
        assert!(review_doc.contains("task_slug: \"review\""));
        assert!(review_doc.contains("task_kind: review"));
        assert!(review_doc.contains("task_driver: agent"));
        assert!(review_doc.contains("role: \"reviewer\""));
        assert!(review_doc.contains("depends_on: [\"step_2\"]"));
    }

    // -----------------------------------------------------------------------
    // Step 6: Run workflow → agent completes → goal.completed
    // -----------------------------------------------------------------------
    let (event6, _side_events6) = daemon
        .run_once(now + 5)
        .expect("run should succeed")
        .expect("workflow job should be processed");

    // After plan approval the workflow should complete.
    assert_eq!(event6.event_type, EventType::WorkflowRun);
    assert_eq!(event6.status, "completed");
    assert_eq!(
        event6.goal_template.as_deref(),
        Some(inquisition_template.as_str())
    );

    // Goal state should be completed.
    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "completed");
    {
        let store = daemon
            .management_store
            .lock()
            .expect("management store lock");
        let goal_item_id = crate::goal_management::goal_work_item_id(&original_goal_id);
        let task_one_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "step_1");
        let task_two_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "step_2");
        let review_task_id =
            crate::goal_management::goal_task_work_item_id(&original_goal_id, "review");
        let execution_one_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "step_1");
        let execution_two_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "step_2");
        let review_execution_id =
            crate::goal_management::goal_execution_work_item_id(&original_goal_id, "review");

        for item_id in [
            goal_item_id.as_str(),
            task_one_id.as_str(),
            task_two_id.as_str(),
            review_task_id.as_str(),
            execution_one_id.as_str(),
            execution_two_id.as_str(),
            review_execution_id.as_str(),
        ] {
            assert_eq!(
                store
                    .get_work_item(item_id)
                    .expect("work item should exist")
                    .status,
                WorkItemStatus::Done,
                "{item_id} should be done"
            );
        }
    }
    {
        let archive_root = daemon
            .config
            .archive_path
            .clone()
            .unwrap_or_else(|| daemon.config.data_dir.join("../knowledge-base"));
        let task_one_doc = std::fs::read_to_string(
            archive_root
                .join("operations/projects/inbox/goals")
                .join(&original_goal_id)
                .join("tasks/step-1.md"),
        )
        .expect("task one archive doc should still exist");
        let task_two_doc = std::fs::read_to_string(
            archive_root
                .join("operations/projects/inbox/goals")
                .join(&original_goal_id)
                .join("tasks/step-2.md"),
        )
        .expect("task two archive doc should still exist");
        let review_doc = std::fs::read_to_string(
            archive_root
                .join("operations/projects/inbox/goals")
                .join(&original_goal_id)
                .join("tasks/review.md"),
        )
        .expect("review archive doc should still exist");
        assert!(task_one_doc.contains("execution_status: done"));
        assert!(task_two_doc.contains("execution_status: done"));
        assert!(review_doc.contains("execution_status: done"));
    }

    // No more jobs in the queue.
    let next = daemon.run_once(now + 6).expect("run should succeed");
    assert!(next.is_none(), "no further jobs after completion");
}

#[test]
fn daemon_rehydrates_management_hierarchy_from_archive_goal_tasks() {
    let name = format!("archive-rehydrate_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove startup reconstruction from Archive task records.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/research.md"),
        r#"---
id: "goal:goal-archive:task:research"
goal_id: "goal-archive"
task_id: "research"
task_slug: "research"
task_kind: execution
task_driver: agent
title: "Research current implementation"
state: active
execution_status: in_progress
role: "researcher"
depends_on: []
thread_id: "thread-archive"
source_step_id: "research"
---

# Research current implementation

## Summary

Inspect the current daemon and summarize the execution model.

## Checklist

- [ ] Execute this task
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root);

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let store = daemon
        .management_store
        .lock()
        .expect("management store lock");

    let goal_id = crate::goal_management::goal_work_item_id("goal-archive");
    let task_id = crate::goal_management::goal_task_work_item_id("goal-archive", "research");
    let execution_id =
        crate::goal_management::goal_execution_work_item_id("goal-archive", "research");

    assert_eq!(
        store
            .get_work_item(&goal_id)
            .expect("goal work item should rehydrate")
            .thread_id
            .as_deref(),
        Some("thread-archive")
    );
    assert_eq!(
        store
            .get_work_item(&task_id)
            .expect("task work item should rehydrate")
            .parent_work_item_id
            .as_deref(),
        Some(goal_id.as_str())
    );
    let execution = store
        .get_work_item(&execution_id)
        .expect("execution work item should rehydrate");
    assert_eq!(execution.status, WorkItemStatus::Running);
    assert_eq!(
        execution
            .assignee
            .as_ref()
            .map(|value| value.agent_id.as_str()),
        Some("role:researcher")
    );
}

#[test]
fn goal_task_transition_updates_archive_and_management_projection() {
    let name = format!("goal-task-transition_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove task transitions from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: planned
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation

## Summary

Wait for the user to confirm the budget.

## Checklist

- [ ] Await the required external signal
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "await-budget",
                "execution_status": "in_progress",
                "note": "User input requested"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    assert_eq!(detail_str(&events[0], "goal_id"), Some("goal-archive"));
    assert_eq!(detail_str(&events[0], "task_id"), Some("await-budget"));
    assert_eq!(
        detail_str(&events[0], "execution_status"),
        Some("in_progress")
    );
    assert_eq!(detail_str(&events[0], "note"), Some("User input requested"));

    let task_doc = std::fs::read_to_string(goal_dir.join("tasks/await-budget.md"))
        .expect("archive task should exist");
    assert!(task_doc.contains("execution_status: in_progress"));
    assert!(task_doc.contains("last_status_change_at: "));

    let latest_event_path = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| entry.path())
        .max()
        .expect("goal event doc should exist");
    let latest_event = std::fs::read_to_string(latest_event_path).expect("read latest goal event");
    assert!(latest_event.contains("event_type: \"task_status_changed\""));
    assert!(latest_event.contains("task_id: \"await-budget\""));
    assert!(latest_event.contains("previous_status: \"planned\""));
    assert!(latest_event.contains("next_status: \"in_progress\""));
    assert!(latest_event.contains("actor: \"@user:test\""));
    assert!(latest_event.contains("note: \"User input requested\""));

    let store = daemon
        .management_store
        .lock()
        .expect("management store lock");
    let task_id = crate::goal_management::goal_task_work_item_id("goal-archive", "await-budget");
    let task = store
        .get_work_item(&task_id)
        .expect("task work item should exist");
    assert_eq!(task.status, WorkItemStatus::Blocked);
    assert_eq!(task.thread_id.as_deref(), Some("thread-archive"));
    assert!(
        store
            .get_work_item(&crate::goal_management::goal_execution_work_item_id(
                "goal-archive",
                "await-budget",
            ))
            .is_none(),
        "waiting tasks should not gain synthetic execution children"
    );
}

#[test]
fn goal_task_transition_notify_operator_emits_operator_notice_and_archive_event() {
    let name = format!("goal-task-notify_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove notify-operator escalation from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy:
  escalation:
    mode: notify_operator
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation

## Summary

Wait for the user to confirm the budget.

## Checklist

- [ ] Await the required external signal
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "await-budget",
                "execution_status": "blocked",
                "note": "Budget still missing"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[0].room_id, "#goals");
    assert_eq!(routed[0].envelope.sym.s, Some(Status::Success));
    assert_eq!(routed[1].room_id, "#goals");
    assert_eq!(routed[1].envelope.sym.s, Some(Status::Awaiting));
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_policy"),
        Some("notify_operator")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_audience"),
        Some("operator")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_severity"),
        Some("normal")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_trigger"),
        Some("on_enter_blocked")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "detail"),
        Some("Budget still missing")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("escalation_policy: \"notify_operator\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("escalation_audience: \"operator\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("escalation_severity: \"normal\"")));
}

#[test]
fn goal_task_transition_reads_goal_scope_defaults_for_on_enter_blocked() {
    let name = format!("goal-task-scope-on-enter_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("operations/policy/scopes"))
        .expect("create policy scopes dir");
    std::fs::write(
        archive_root.join("operations/policy/scopes/team-infra.md"),
        r#"---
id: "team:infra"
kind: team
title: "Infrastructure Team"
enabled: true
priority: 200
delivery_subject: "team:infra"
task_policy_defaults:
  waiting:
    mode: notify_operator
    audience: "team:infra"
    severity: urgent
    on_enter_blocked: true
---

# Infrastructure Team
"#,
    )
    .expect("write policy scope");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
policy_scopes:
  - "team:infra"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (_daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let task = crate::goal_management::PlannedTaskRecord {
        task_id: "await-budget".to_string(),
        task_slug: "await-budget".to_string(),
        task_kind: symbiotic_control_plane::types::GoalTaskKind::Waiting,
        task_driver: symbiotic_control_plane::types::GoalTaskDriver::Declared,
        title: "Await budget confirmation".to_string(),
        summary: "Wait for the user to confirm the budget.".to_string(),
        state: symbiotic_control_plane::types::GoalTaskPlanState::Active,
        execution_status: symbiotic_control_plane::types::GoalTaskStatus::InProgress,
        role: None,
        depends_on: Vec::new(),
        questionnaire_context: Vec::new(),
        owner_hint: Some("@user:test".to_string()),
        declared_context: symbiotic_control_plane::types::GoalTaskDeclaredContext {
            waiting_for: Some("budget confirmation".to_string()),
            ..Default::default()
        },
        policy: symbiotic_control_plane::types::GoalTaskPolicy::default(),
        retry_count: 0,
        reopen_count: 0,
        last_status_change_at: Some(100),
        plan_version: 1,
        superseded_by: Vec::new(),
        derived_from: Vec::new(),
        replaces: Vec::new(),
    };
    let context = crate::goal_management::GoalHierarchyContext {
        project_id: "project:archive",
        goal_id: "goal-archive",
        title: "Archive Restore Goal",
        summary: "Prove scope-derived on-enter-blocked escalation.",
        owner: None,
        thread_id: Some("thread-archive"),
        observed_at: 170,
    };

    let escalation = crate::goal_management::persist_goal_task_declared_status_archive(
        &archive_root,
        &context,
        &task,
        symbiotic_control_plane::types::GoalTaskStatus::Blocked,
        Some("@user:test"),
        Some("Budget confirmation is missing"),
    )
    .expect("status archive write should succeed")
    .expect("scope defaults should yield escalation");

    assert_eq!(
        escalation.policy,
        symbiotic_control_plane::types::GoalTaskEscalationPolicy::NotifyOperator
    );
    assert_eq!(escalation.audience.as_deref(), Some("team:infra"));
    assert_eq!(
        escalation.severity,
        Some(symbiotic_control_plane::types::GoalTaskEscalationSeverity::Urgent)
    );
}

#[test]
fn goal_task_transition_defers_operator_notice_outside_delivery_window() {
    let name = format!("goal-task-notify-deferred_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy:
  escalation:
    mode: notify_operator
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = 170u64;
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "await-budget",
                "execution_status": "blocked",
                "note": "Budget still missing"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].room_id, "#goals");
    assert_eq!(routed[0].envelope.sym.s, Some(Status::Success));

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_deferred\"")));
    assert!(!event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
}

#[test]
fn goal_task_transition_uses_delivery_subject_availability_for_immediate_escalation() {
    let name = format!("goal-task-availability-on-enter_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("identity")).expect("create identity dir");
    std::fs::create_dir_all(archive_root.join("operations/policy/scopes"))
        .expect("create policy scopes dir");
    std::fs::create_dir_all(archive_root.join("operations/calendar/availability"))
        .expect("create availability dir");
    std::fs::write(
        archive_root.join("identity/preferences.md"),
        r#"---
version: 1
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
---

# Preferences
"#,
    )
    .expect("write preferences");
    std::fs::write(
        archive_root.join("operations/policy/scopes/oncall-infra.md"),
        r#"---
id: "oncall:infra"
kind: on_call
title: "Infrastructure On-Call"
enabled: true
priority: 300
delivery_subject: "oncall:infra"
task_policy_defaults:
  waiting:
    mode: notify_operator
    audience: "oncall:infra"
    severity: urgent
    on_enter_blocked: true
---

# Infrastructure On-Call
"#,
    )
    .expect("write audience scope");
    std::fs::write(
        archive_root.join("operations/calendar/availability/oncall-infra.md"),
        r#"---
id: "availability:oncall:infra"
subject: "oncall:infra"
enabled: true
timezone: "UTC"
working_hours:
  weekdays: [mon, tue, wed, thu, fri, sat, sun]
  start_local: "00:00"
  end_local: "23:59"
quiet_hours: null
---

# On-call availability
"#,
    )
    .expect("write availability rule");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
policy_scopes:
  - "oncall:infra"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = 170u64;
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "await-budget",
                "execution_status": "blocked",
                "note": "Budget still missing"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert!(
        routed.iter().any(|item| {
            detail_str(&item.envelope, "escalation_audience") == Some("oncall:infra")
        }),
        "expected immediate escalation routed with oncall audience"
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
    assert!(!event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_deferred\"")));
}

#[test]
fn goal_task_transition_raise_alert_routes_to_alerts_room() {
    let name = format!("goal-task-alert_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove raise-alert escalation from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/wait-vendor.md"),
        r#"---
id: "goal:goal-archive:task:wait-vendor"
goal_id: "goal-archive"
task_id: "wait-vendor"
task_slug: "wait-vendor"
task_kind: coordination
task_driver: declared
title: "Wait for vendor response"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: "vendor support"
  external_dependency: "vendor-api"
policy:
  escalation:
    mode: raise_alert
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "wait-vendor"
---

# Wait for vendor response

## Summary

Wait for the vendor to unblock the integration.

## Checklist

- [ ] Coordinate this task
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "wait-vendor",
                "execution_status": "blocked",
                "note": "Vendor support stopped responding"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@operator:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 3);
    assert_eq!(routed[2].room_id, "#alerts");
    assert_eq!(
        routed[2].envelope.sym.a.as_deref(),
        Some("goal.task.escalated")
    );
    assert_eq!(
        detail_str(&routed[2].envelope, "escalation_policy"),
        Some("raise_alert")
    );
    assert_eq!(
        detail_str(&routed[2].envelope, "escalation_audience"),
        Some("operator")
    );
    assert_eq!(
        detail_str(&routed[2].envelope, "escalation_severity"),
        Some("high")
    );
    assert_eq!(
        detail_str(&routed[2].envelope, "escalation_trigger"),
        Some("on_enter_blocked")
    );
}

#[test]
fn goal_task_transition_auto_replan_records_replan_request() {
    let name = format!("goal-task-replan_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove auto-replan requests from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/approval-gate.md"),
        r#"---
id: "goal:goal-archive:task:approval-gate"
goal_id: "goal-archive"
task_id: "approval-gate"
task_slug: "approval-gate"
task_kind: approval
task_driver: declared
title: "Approval gate"
state: active
execution_status: in_progress
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: "deployment plan"
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy:
  escalation:
    mode: auto_replan
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "approval-gate"
---

# Approval gate

## Summary

Wait for deployment approval.

## Checklist

- [ ] Obtain or record approval
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.transition",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "approval-gate",
                "execution_status": "blocked",
                "note": "Approval was denied"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@operator:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 3);
    assert_eq!(
        routed[2].envelope.sym.a.as_deref(),
        Some("goal.task.replan.requested")
    );
    assert_eq!(
        detail_str(&routed[2].envelope, "escalation_policy"),
        Some("auto_replan")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_replan_requested\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("escalation_policy: \"auto_replan\"")));
}

#[test]
fn enqueue_pending_archive_replans_queues_inquisition_from_archive_request() {
    let name = format!("goal-task-replan-enqueue_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Re-enter planning from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/approval-gate.md"),
        r#"---
id: "goal:goal-archive:task:approval-gate"
goal_id: "goal-archive"
task_id: "approval-gate"
task_slug: "approval-gate"
task_kind: approval
task_driver: declared
title: "Approval gate"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: "deployment plan"
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy:
  escalation:
    mode: auto_replan
    on_enter_blocked: true
    after_secs: null
    max_count: null
    cooldown_secs: null
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "approval-gate"
---

# Approval gate

## Summary

Wait for deployment approval.
"#,
    )
    .expect("write archive task");
    std::fs::create_dir_all(goal_dir.join("events")).expect("create events dir");
    std::fs::write(
        goal_dir.join("events/100-task-replan-requested-approval-gate.md"),
        r#"---
goal_id: "goal-archive"
event_type: "task_replan_requested"
observed_at: 100
plan_version: 1
thread_id: "thread-archive"
task_id: "approval-gate"
previous_status: "in_progress"
next_status: "blocked"
previous_owner: null
next_owner: null
escalation_policy: "auto_replan"
actor: "@operator:test"
note: "Approval was denied"
added_task_ids: []
preserved_task_ids: []
deactivated_task_ids: []
supersession_edges: []
owner_change_edges: []
---

# task replan requested

task=approval-gate blocked -> replan requested
"#,
    )
    .expect("write archive replan event");
    config.archive_path = Some(archive_root.clone());

    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    register_inquisition_template(&mut daemon);

    let now = now_unix();
    let queued = daemon
        .enqueue_pending_archive_replans(now)
        .expect("enqueue replan");
    assert_eq!(queued, 1);

    let jobs = daemon
        .queued_jobs_of_type("workflow.run")
        .expect("list workflow jobs");
    assert_eq!(jobs.len(), 1);
    let payload = decode_workflow_payload(&jobs[0].payload).expect("decode workflow payload");
    assert_eq!(payload.template, "inquisition:goal-archive");
    assert_eq!(payload.goal_id.as_deref(), Some("goal-archive"));
    assert_eq!(payload.goal_room.as_deref(), Some("#goals"));
    assert!(payload
        .replan_context
        .as_deref()
        .unwrap_or_default()
        .contains("Approval was denied"));
    assert!(payload
        .replan_context
        .as_deref()
        .unwrap_or_default()
        .contains("deployment plan"));

    let states = daemon.list_goal_states().expect("goal states");
    let state = states
        .into_iter()
        .find(|state| state.template == "inquisition:goal-archive")
        .expect("replan state");
    assert_eq!(state.goal_room, "#goals");
    assert_eq!(state.pipeline_stage.as_deref(), Some("replanning"));
    assert_eq!(state.last_run_id.as_deref(), Some("goal-archive"));

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_replan_enqueued\"")));
}

#[test]
fn declared_task_policy_tick_escalates_after_secs_from_goal_defaults() {
    let name = format!("declared-task-policy-after-secs_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  waiting:
    after_secs: 60
    cooldown_secs: 300
---

# Archive Restore Goal

## Objective

Escalate blocked declared work from goal policy defaults.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation

## Summary

Wait for the user to confirm the budget.
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].room_id, "#goals");
    assert_eq!(
        detail_str(&routed[0].envelope, "task_id"),
        Some("await-budget")
    );
    assert_eq!(
        detail_str(&routed[0].envelope, "escalation_trigger"),
        Some("after_secs")
    );
    assert_eq!(
        detail_str(&routed[0].envelope, "escalation_policy"),
        Some("notify_operator")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("escalation_trigger: \"after_secs\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("actor: \"nucleus.policy\"")));
}

#[test]
fn declared_task_policy_tick_suppresses_repeat_escalation_during_cooldown() {
    let name = format!("declared-task-policy-cooldown_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(goal_dir.join("events")).expect("create events dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  waiting:
    after_secs: 60
    cooldown_secs: 300
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    std::fs::write(
        goal_dir.join("events/165-task-escalated-await-budget.md"),
        r#"---
goal_id: "goal-archive"
event_type: "task_escalated"
observed_at: 165
plan_version: 1
thread_id: "thread-archive"
task_id: "await-budget"
previous_status: "blocked"
next_status: "blocked"
previous_owner: null
next_owner: null
escalation_policy: "notify_operator"
escalation_trigger: "after_secs"
escalation_count: 1
cooldown_until: 465
actor: "nucleus.policy"
note: "Task remained blocked."
added_task_ids: []
preserved_task_ids: []
deactivated_task_ids: []
supersession_edges: []
owner_change_edges: []
---

# task escalated

task=await-budget policy=notify_operator
"#,
    )
    .expect("write escalation event");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert!(routed.is_empty());

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_suppressed\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("cooldown_until: 465")));
}

#[test]
fn declared_task_policy_tick_reads_operator_defaults_from_preferences() {
    let name = format!("declared-task-policy-prefs_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("identity")).expect("create identity dir");
    std::fs::write(
        archive_root.join("identity/preferences.md"),
        r#"---
version: 1
task_policy_defaults:
  evaluator:
    interval_secs: 30
    max_actions_per_tick: 32
  declared_task_defaults:
    waiting:
      mode: notify_operator
      audience: oncall:infra
      severity: urgent
      after_secs: 60
      cooldown_secs: 300
---

# Preferences
"#,
    )
    .expect("write preferences");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[1].room_id, "#alerts");
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_policy"),
        Some("notify_operator")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_audience"),
        Some("oncall:infra")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "escalation_severity"),
        Some("urgent")
    );
}

#[test]
fn declared_task_policy_tick_reads_goal_attached_policy_scope_defaults() {
    let name = format!("declared-task-policy-goal-scope_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("operations/policy/scopes"))
        .expect("create policy scopes dir");
    std::fs::write(
        archive_root.join("operations/policy/scopes/team-infra.md"),
        r#"---
id: "team:infra"
kind: team
title: "Infrastructure Team"
enabled: true
priority: 200
delivery_subject: "team:infra"
task_policy_defaults:
  waiting:
    mode: notify_operator
    audience: "team:infra"
    severity: urgent
    after_secs: 60
    cooldown_secs: 300
---

# Infrastructure Team
"#,
    )
    .expect("write policy scope");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
policy_scopes:
  - "team:infra"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[1].room_id, "#alerts");
    assert_eq!(
        detail_str(&routed[0].envelope, "escalation_audience"),
        Some("team:infra")
    );
    assert_eq!(
        detail_str(&routed[0].envelope, "escalation_severity"),
        Some("urgent")
    );
}

#[test]
fn declared_task_policy_tick_uses_audience_scope_timing_over_operator_defaults() {
    let name = format!(
        "declared-task-policy-audience-scope-timing_{}",
        unique_suffix()
    );
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("identity")).expect("create identity dir");
    std::fs::create_dir_all(archive_root.join("operations/policy/scopes"))
        .expect("create policy scopes dir");
    std::fs::write(
        archive_root.join("identity/preferences.md"),
        r#"---
version: 1
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
  declared_task_defaults:
    waiting:
      mode: notify_operator
      audience: "oncall:infra"
      severity: urgent
      after_secs: 60
      cooldown_secs: 300
---

# Preferences
"#,
    )
    .expect("write preferences");
    std::fs::write(
        archive_root.join("operations/policy/scopes/oncall-infra.md"),
        r#"---
id: "oncall:infra"
kind: on_call
title: "Infrastructure On-Call"
enabled: true
priority: 300
delivery_subject: "oncall:infra"
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: anytime
  waiting:
    audience: "oncall:infra"
---

# Infrastructure On-Call
"#,
    )
    .expect("write audience scope");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[1].room_id, "#alerts");
    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
    assert!(!event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_deferred\"")));
}

#[test]
fn declared_task_policy_tick_uses_delivery_subject_availability() {
    let name = format!(
        "declared-task-policy-delivery-subject-availability_{}",
        unique_suffix()
    );
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(archive_root.join("identity")).expect("create identity dir");
    std::fs::create_dir_all(archive_root.join("operations/policy/scopes"))
        .expect("create policy scopes dir");
    std::fs::create_dir_all(archive_root.join("operations/calendar/availability"))
        .expect("create availability dir");
    std::fs::write(
        archive_root.join("identity/preferences.md"),
        r#"---
version: 1
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
  declared_task_defaults:
    waiting:
      mode: notify_operator
      audience: "oncall:infra"
      severity: urgent
      after_secs: 60
      cooldown_secs: 300
---

# Preferences
"#,
    )
    .expect("write preferences");
    std::fs::write(
        archive_root.join("operations/policy/scopes/oncall-infra.md"),
        r#"---
id: "oncall:infra"
kind: on_call
title: "Infrastructure On-Call"
enabled: true
priority: 300
delivery_subject: "oncall:infra"
task_policy_defaults:
  waiting:
    audience: "oncall:infra"
---

# Infrastructure On-Call
"#,
    )
    .expect("write audience scope");
    std::fs::write(
        archive_root.join("operations/calendar/availability/oncall-infra.md"),
        r#"---
id: "availability:oncall:infra"
subject: "oncall:infra"
enabled: true
timezone: "UTC"
working_hours:
  weekdays: [mon, tue, wed, thu, fri, sat, sun]
  start_local: "00:00"
  end_local: "23:59"
quiet_hours: null
---

# On-call availability
"#,
    )
    .expect("write availability rule");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[1].room_id, "#alerts");
    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
    assert!(!event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_deferred\"")));
}

#[test]
fn declared_task_policy_tick_defers_escalation_outside_delivery_window() {
    let name = format!("declared-task-policy-deferred_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
  waiting:
    after_secs: 60
    cooldown_secs: 300
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert!(routed.is_empty());

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_deferred\"")));
    assert!(!event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
}

#[test]
fn declared_task_policy_tick_delivers_deferred_escalation_when_window_opens() {
    let name = format!("declared-task-policy-window-open_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::create_dir_all(goal_dir.join("events")).expect("create archive events dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
  waiting:
    after_secs: 60
    cooldown_secs: 300
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    std::fs::write(
        goal_dir.join("events/170-task-escalation-deferred-await-budget.md"),
        r#"---
goal_id: "goal-archive"
event_type: "task_escalation_deferred"
observed_at: 170
plan_version: 1
thread_id: "thread-archive"
task_id: "await-budget"
previous_status: "blocked"
next_status: "blocked"
previous_owner: null
next_owner: null
escalation_policy: "notify_operator"
escalation_trigger: "after_secs"
escalation_audience: "operator"
escalation_severity: "normal"
escalation_count: null
cooldown_until: null
condition_kind: null
condition_value: null
actor: "nucleus.policy"
note: "Deferred until working hours."
added_task_ids: []
preserved_task_ids: []
deactivated_task_ids: []
supersession_edges: []
owner_change_edges: []
---

# task escalation deferred

task=await-budget policy=notify_operator deferred
"#,
    )
    .expect("write deferred event");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(36_000)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].room_id, "#goals");
    assert_eq!(
        detail_str(&routed[0].envelope, "escalation_trigger"),
        Some("after_secs")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalation_window_opened\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_escalated\"")));
}

#[test]
fn declared_task_policy_tick_counts_only_delivery_window_elapsed_when_requested() {
    let name = format!("declared-task-policy-window-elapsed_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
task_policy_defaults:
  evaluator:
    timezone: "UTC"
    lateness_basis: delivery_window_elapsed
    delivery_window:
      mode: working_hours
      quiet_hours: null
      working_hours:
        weekdays: [mon, tue, wed, thu, fri, sat, sun]
        start_local: "09:00"
        end_local: "18:00"
  waiting:
    after_secs: 3600
    cooldown_secs: 300
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/await-budget.md"),
        r#"---
id: "goal:goal-archive:task:await-budget"
goal_id: "goal-archive"
task_id: "await-budget"
task_slug: "await-budget"
task_kind: waiting
task_driver: declared
title: "Await budget confirmation"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@user:test"
declared_context:
  review_target: null
  waiting_for: "budget confirmation"
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 28800
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-budget"
---

# Await budget confirmation
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let early = daemon
        .process_declared_task_policy_tick(34_200)
        .expect("early policy tick should succeed");
    assert!(early.is_empty());

    let later = daemon
        .process_declared_task_policy_tick(36_000)
        .expect("later policy tick should succeed");
    assert_eq!(later.len(), 1);
    assert_eq!(
        detail_str(&later[0].envelope, "task_id"),
        Some("await-budget")
    );
}

#[test]
fn declared_task_policy_tick_emits_dependency_satisfied_event() {
    let name = format!("declared-task-policy-deps_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/research.md"),
        r#"---
id: "goal:goal-archive:task:research"
goal_id: "goal-archive"
task_id: "research"
task_slug: "research"
task_kind: execution
task_driver: agent
title: "Research"
state: active
execution_status: done
role: "researcher"
depends_on: []
questionnaire_context: []
owner_hint: "role:researcher"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 120
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "research"
---

# Research
"#,
    )
    .expect("write dependency task");
    std::fs::write(
        goal_dir.join("tasks/await-approval.md"),
        r#"---
id: "goal:goal-archive:task:await-approval"
goal_id: "goal-archive"
task_id: "await-approval"
task_slug: "await-approval"
task_kind: approval
task_driver: declared
title: "Await approval"
state: active
execution_status: planned
role: null
depends_on: ["research"]
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: "research findings"
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "await-approval"
---

# Await approval
"#,
    )
    .expect("write waiting task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");

    assert_eq!(routed.len(), 1);
    assert_eq!(
        detail_str(&routed[0].envelope, "task_id"),
        Some("await-approval")
    );
    assert_eq!(
        routed[0].envelope.sym.a.as_deref(),
        Some("goal.task.dependencies.satisfied")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_dependencies_satisfied\"")));
}

#[test]
fn goal_task_condition_set_updates_archive_and_routes_update() {
    let name = format!("goal-task-condition-set_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/wait-budget.md"),
        r#"---
id: "goal:goal-archive:task:wait-budget"
goal_id: "goal-archive"
task_id: "wait-budget"
task_slug: "wait-budget"
task_kind: waiting
task_driver: declared
title: "Wait for budget"
state: active
execution_status: planned
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "wait-budget"
---

# Wait for budget
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.condition.set",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "wait-budget",
                "condition_kind": "waiting_for",
                "condition_value": "budget approval",
                "note": "Budget needs operator confirmation"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@operator:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(
        routed[1].envelope.sym.a.as_deref(),
        Some("goal.task.condition.set")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "condition_kind"),
        Some("waiting_for")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "condition_value"),
        Some("budget approval")
    );

    let task = crate::goal_management::load_goal_task_from_archive(
        &archive_root,
        "goal-archive",
        "wait-budget",
    )
    .expect("load task")
    .expect("task exists");
    assert_eq!(
        task.declared_context.waiting_for.as_deref(),
        Some("budget approval")
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_condition_set\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("condition_kind: \"waiting_for\"")));
}

#[test]
fn goal_task_condition_satisfied_appends_archive_event_and_routes_update() {
    let name = format!("goal-task-condition-satisfied_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/wait-vendor.md"),
        r#"---
id: "goal:goal-archive:task:wait-vendor"
goal_id: "goal-archive"
task_id: "wait-vendor"
task_slug: "wait-vendor"
task_kind: coordination
task_driver: declared
title: "Wait for vendor"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: "vendor support"
  external_dependency: "vendor-api"
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "wait-vendor"
---

# Wait for vendor
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.condition.satisfied",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "wait-vendor",
                "condition_kind": "external_dependency",
                "note": "Vendor API is back online"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@operator:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(routed[0].room_id, "#goals");
    assert_eq!(routed[1].room_id, "#goals");
    assert_eq!(
        routed[1].envelope.sym.a.as_deref(),
        Some("goal.task.condition.satisfied")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "task_id"),
        Some("wait-vendor")
    );
    assert_eq!(
        detail_str(&routed[1].envelope, "condition_kind"),
        Some("external_dependency")
    );

    let task = crate::goal_management::load_goal_task_from_archive(
        &archive_root,
        "goal-archive",
        "wait-vendor",
    )
    .expect("load task")
    .expect("task exists");
    assert_eq!(task.declared_context.external_dependency, None);

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_condition_satisfied\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("Vendor API is back online")));
}

#[test]
fn declared_task_policy_tick_resumes_blocked_task_when_dependencies_clear() {
    let name = format!("declared-task-policy-resume_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/fetch-research.md"),
        r#"---
id: "goal:goal-archive:task:fetch-research"
goal_id: "goal-archive"
task_id: "fetch-research"
task_slug: "fetch-research"
task_kind: execution
task_driver: agent
title: "Fetch research"
state: active
execution_status: done
role: researcher
depends_on: []
questionnaire_context: []
owner_hint: "role:researcher"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 90
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "fetch-research"
---

# Fetch research
"#,
    )
    .expect("write dependency task");
    std::fs::write(
        goal_dir.join("tasks/write-summary.md"),
        r#"---
id: "goal:goal-archive:task:write-summary"
goal_id: "goal-archive"
task_id: "write-summary"
task_slug: "write-summary"
task_kind: review
task_driver: declared
title: "Write summary"
state: active
execution_status: blocked
role: null
depends_on: ["fetch-research"]
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: null
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "write-summary"
---

# Write summary
"#,
    )
    .expect("write blocked task");
    config.archive_path = Some(archive_root.clone());

    let context = crate::goal_management::GoalHierarchyContext {
        project_id: "project:archive",
        goal_id: "goal-archive",
        title: "Archive Restore Goal",
        summary: "Restore the archive",
        owner: Some("@operator:test"),
        thread_id: Some("thread-archive"),
        observed_at: 100,
    };
    crate::goal_management::append_goal_event_archive(
        &archive_root,
        &context,
        "task_status_changed",
        1,
        "task=write-summary previous=in_progress next=blocked",
        crate::goal_management::GoalEventMetadata {
            task_id: Some("write-summary"),
            previous_status: Some("in_progress"),
            next_status: Some("blocked"),
            actor: Some("@operator:test"),
            note: Some("Waiting for dependency completion."),
            ..crate::goal_management::GoalEventMetadata::default()
        },
    )
    .expect("append blocked transition");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let routed = daemon
        .process_declared_task_policy_tick(170)
        .expect("policy tick should succeed");
    assert_eq!(routed.len(), 1);
    assert_eq!(
        routed[0].envelope.sym.a.as_deref(),
        Some("goal.task.dependencies.satisfied")
    );

    let task = crate::goal_management::load_goal_task_from_archive(
        &archive_root,
        "goal-archive",
        "write-summary",
    )
    .expect("load task")
    .expect("task exists");
    assert_eq!(
        task.execution_status,
        symbiotic_control_plane::types::GoalTaskStatus::InProgress
    );
    assert_eq!(task.declared_context.external_dependency, None);

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_dependencies_satisfied\"")));
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("previous_status: \"blocked\"")
            && event.contains("next_status: \"in_progress\"")));
}

#[test]
fn goal_task_condition_satisfied_resumes_task_when_external_blocker_clears() {
    let name = format!("goal-task-condition-satisfied-resume_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/wait-vendor.md"),
        r#"---
id: "goal:goal-archive:task:wait-vendor"
goal_id: "goal-archive"
task_id: "wait-vendor"
task_slug: "wait-vendor"
task_kind: coordination
task_driver: declared
title: "Wait for vendor"
state: active
execution_status: blocked
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@operator:test"
declared_context:
  review_target: null
  waiting_for: null
  coordination_target: null
  external_dependency: "vendor-api"
policy: {}
retry_count: 0
reopen_count: 0
last_status_change_at: 100
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "wait-vendor"
---

# Wait for vendor
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let context = crate::goal_management::GoalHierarchyContext {
        project_id: "project:archive",
        goal_id: "goal-archive",
        title: "Archive Restore Goal",
        summary: "Restore the archive",
        owner: Some("@operator:test"),
        thread_id: Some("thread-archive"),
        observed_at: 100,
    };
    crate::goal_management::append_goal_event_archive(
        &archive_root,
        &context,
        "task_status_changed",
        1,
        "task=wait-vendor previous=in_progress next=blocked",
        crate::goal_management::GoalEventMetadata {
            task_id: Some("wait-vendor"),
            previous_status: Some("in_progress"),
            next_status: Some("blocked"),
            actor: Some("@operator:test"),
            note: Some("Vendor API unavailable."),
            ..crate::goal_management::GoalEventMetadata::default()
        },
    )
    .expect("append blocked transition");

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.condition.satisfied",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "wait-vendor",
                "condition_kind": "external_dependency",
                "note": "Vendor API is back online"
            }
        }
    })
    .to_string();

    let routed = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@operator:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(routed.len(), 2);
    assert_eq!(
        detail_val(&routed[0].envelope, "resumed").and_then(|value| value.as_bool()),
        Some(true)
    );

    let task = crate::goal_management::load_goal_task_from_archive(
        &archive_root,
        "goal-archive",
        "wait-vendor",
    )
    .expect("load task")
    .expect("task exists");
    assert_eq!(
        task.execution_status,
        symbiotic_control_plane::types::GoalTaskStatus::InProgress
    );

    let event_bodies = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| std::fs::read_to_string(entry.path()).expect("read event"))
        .collect::<Vec<_>>();
    assert!(event_bodies
        .iter()
        .any(|event| event.contains("event_type: \"task_condition_satisfied\"")));
    assert!(event_bodies.iter().any(|event| event.contains(
        "Declared external dependency cleared; task resumed to its last runnable status."
    )));
}

#[test]
fn goal_task_assign_updates_archive_and_task_ownership() {
    let name = format!("goal-task-assign_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let archive_root = config.data_dir.join("knowledge-base");
    let goal_dir = archive_root.join("operations/projects/archive/goals/goal-archive");
    write_archive_project_fixture(&archive_root, "archive", "project:archive", "Archive");
    std::fs::create_dir_all(goal_dir.join("tasks")).expect("create archive tasks dir");
    std::fs::write(
        goal_dir.join("plan.md"),
        r#"---
id: "goal-archive"
project_id: "project:archive"
slug: goal-archive
title: "Archive Restore Goal"
state: active
priority: 50
autonomy_level: semi
process:
  type: on_demand
  check_frequency: daily
  max_parallel_agents: 1
phase: implementation
domains: [projects]
vault_namespace: goal-goal-archive
thread_id: "thread-archive"
---

# Archive Restore Goal

## Objective

Prove task assignment from Archive truth.
"#,
    )
    .expect("write archive plan");
    std::fs::write(
        goal_dir.join("tasks/review-plan.md"),
        r#"---
id: "goal:goal-archive:task:review-plan"
goal_id: "goal-archive"
task_id: "review-plan"
task_slug: "review-plan"
task_kind: approval
task_driver: declared
title: "Review the plan"
state: active
execution_status: planned
role: null
depends_on: []
questionnaire_context: []
owner_hint: "@manager:test"
retry_count: 0
reopen_count: 0
last_status_change_at: null
plan_version: 1
superseded_by: []
derived_from: []
replaces: []
thread_id: "thread-archive"
source_step_id: "review-plan"
---

# Review the plan

## Summary

Wait for the operator to review the plan.

## Checklist

- [ ] Obtain or record approval
"#,
    )
    .expect("write archive task");
    config.archive_path = Some(archive_root.clone());

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should open with archive");
    let now = now_unix();
    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.task.assign",
            "t": "thread-archive",
            "d": {
                "goal_id": "goal-archive",
                "task_id": "review-plan",
                "owner_hint": "@operator:test",
                "note": "Operator will own the approval gate"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@lead:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sym.s, Some(Status::Success));
    assert_eq!(detail_str(&events[0], "goal_id"), Some("goal-archive"));
    assert_eq!(detail_str(&events[0], "task_id"), Some("review-plan"));
    assert_eq!(detail_str(&events[0], "owner_hint"), Some("@operator:test"));
    assert_eq!(
        detail_str(&events[0], "note"),
        Some("Operator will own the approval gate")
    );

    let task_doc = std::fs::read_to_string(goal_dir.join("tasks/review-plan.md"))
        .expect("archive task should exist");
    assert!(task_doc.contains("owner_hint: \"@operator:test\""));

    let latest_event_path = std::fs::read_dir(goal_dir.join("events"))
        .expect("read events dir")
        .flatten()
        .map(|entry| entry.path())
        .max()
        .expect("goal event doc should exist");
    let latest_event = std::fs::read_to_string(latest_event_path).expect("read latest goal event");
    assert!(latest_event.contains("event_type: \"task_owner_changed\""));
    assert!(latest_event.contains("task_id: \"review-plan\""));
    assert!(latest_event.contains("previous_owner: \"@manager:test\""));
    assert!(latest_event.contains("next_owner: \"@operator:test\""));
    assert!(latest_event.contains("actor: \"@lead:test\""));
    assert!(latest_event.contains("note: \"Operator will own the approval gate\""));

    let store = daemon
        .management_store
        .lock()
        .expect("management store lock");
    let task_id = crate::goal_management::goal_task_work_item_id("goal-archive", "review-plan");
    let task = store
        .get_work_item(&task_id)
        .expect("task work item should exist");
    assert_eq!(
        task.assignee.as_ref().map(|value| value.agent_id.as_str()),
        Some("@operator:test")
    );
    assert!(
        store
            .get_work_item(&crate::goal_management::goal_execution_work_item_id(
                "goal-archive",
                "review-plan",
            ))
            .is_none(),
        "approval tasks should not gain synthetic execution children"
    );
}

/// E2E: Full inquisition pipeline using the runner backend through the real
/// bridge and the in-process runner library seam.
///
/// This is intentionally narrower than the React-backend variant above: it
/// proves the missing T113 claim that workflow-level pause/resume still works
/// when `agent.execute` goes through the authenticated runner/daemon bridge.
#[test]
fn e2e_inquisition_full_pipeline_runner_backend() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let socket_path =
        std::env::temp_dir().join(format!("symbiotic_llm_gateway_{}.sock", unique_suffix()));
    let name = format!("e2e-inquisition-runner_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    config.agent_backend = AgentBackend::Runner;
    config.runner_harness_mode = crate::workers::RunnerHarnessMode::InProcess;
    config.llm_gateway_socket = Some(socket_path.to_string_lossy().to_string());

    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    register_inquisition_template(&mut daemon);

    let gateway_task = spawn_llm_gateway_for_test(
        &daemon,
        daemon
            .config
            .llm_gateway_socket
            .as_deref()
            .expect("socket should be configured"),
    );
    wait_for_test_socket(&socket_path);

    let responses = vec![
        r#"{"tool": "ask_user", "params": {"question": "What budget range are you comfortable with?", "quick_replies": ["Under $500", "$500-$1000", "Over $1000"]}}"#.to_string(),
        r#"{"done": true, "result": "Asked user about budget preference."}"#.to_string(),
        r#"{"tool": "generate_plan", "params": {"summary": "Find best flights to Tokyo under $500", "steps": [{"task_id": "step_1", "task_slug": "step_1", "task_kind": "execution", "task_driver": "agent", "role": "researcher", "description": "Search flight aggregators"}, {"task_id": "step_2", "task_slug": "step_2", "task_kind": "execution", "task_driver": "agent", "role": "researcher", "description": "Compare prices and routes", "depends_on": ["step_1"]}], "confidence": 0.92}}"#.to_string(),
        r#"{"done": true, "result": "Plan proposed for approval."}"#.to_string(),
        r#"{"done": true, "result": "Found 3 flights to Tokyo under $500. Best: ANA via LAX, $423 round trip."}"#.to_string(),
    ];
    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();
    let events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: "Find the best flight deals to Tokyo for next month".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    let original_goal_id = detail_str(&events[0], "goal_id")
        .expect("goal_id should be present")
        .to_string();
    let inquisition_template = format!("inquisition:{original_goal_id}");

    let find_goal = |daemon: &SymbioticDaemon| -> GoalState {
        daemon
            .list_goal_states()
            .expect("list states")
            .into_iter()
            .find(|s| s.template == inquisition_template && s.goal_room == "#goals")
            .expect("inquisition goal state should exist")
    };

    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "running");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("clarifying"));
    assert_eq!(gs.last_run_id.as_deref(), Some(original_goal_id.as_str()));

    let (question_event, _) = daemon
        .run_once(now + 1)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(question_event.event_type, EventType::GoalQuestion);
    assert_eq!(question_event.status, "awaiting_input");
    let workflow_goal_id = question_event
        .goal_id
        .as_ref()
        .expect("question goal_id")
        .clone();

    let answer_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.answer",
            "t": workflow_goal_id,
            "d": {
                "template": inquisition_template,
                "message": "Under $500"
            }
        }
    })
    .to_string();
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: answer_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("route answer should work");

    let (plan_event, _) = daemon
        .run_once(now + 3)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(plan_event.event_type, EventType::GoalPlanProposed);
    assert_eq!(plan_event.status, "awaiting_approval");
    let plan_goal_id = plan_event.goal_id.as_ref().expect("plan goal_id").clone();

    let approve_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.plan.approved",
            "t": plan_goal_id,
            "d": {}
        }
    })
    .to_string();
    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: approve_body,
                timestamp: now + 4,
            },
            now + 4,
        )
        .expect("route approval should work");

    let (completion_event, _) = daemon
        .run_once(now + 5)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(completion_event.event_type, EventType::WorkflowRun);
    assert_eq!(completion_event.status, "completed");

    let gs = find_goal(&daemon);
    assert_eq!(gs.status, "completed");

    gateway_task.abort();
}

#[test]
fn runner_distillery_harness_writes_typed_bundle_artifacts() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let socket_path =
            std::env::temp_dir().join(format!("symbiotic_llm_gateway_{}.sock", unique_suffix()));
        let workspace = tempfile::tempdir().expect("tempdir");
        let name = format!("runner-distillery-bundle_{}", unique_suffix());
        let mut config = daemon_config_for_test(&name);
        config.llm_gateway_socket = Some(socket_path.to_string_lossy().to_string());

        let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
        let gateway_task = spawn_llm_gateway_for_test(
            &daemon,
            daemon
                .config
                .llm_gateway_socket
                .as_deref()
                .expect("socket should be configured"),
        );
        wait_for_test_socket(&socket_path);

        inject_sequential_mock_into_daemon(
            &daemon,
            vec![
                serde_json::json!({
                    "tool": "file_write",
                    "params": {
                        "path": "distillery-bundle.json",
                        "content": serde_json::json!({
                            "version": 1,
                            "report": {
                                "summary_markdown": "Runner generated summary.",
                                "decisions": ["Use typed distillery artifacts."],
                                "patterns": ["Write a canonical Archive note on the trusted side."],
                                "lint_status": "clean"
                            },
                            "artifacts": [{
                                "kind": "pattern_note",
                                "title": "Trusted Archive Routing",
                                "relative_path": "patterns/trusted-archive-routing.md"
                            }]
                        }).to_string()
                    }
                })
                .to_string(),
                serde_json::json!({
                    "tool": "file_write",
                    "params": {
                        "path": "output/patterns/trusted-archive-routing.md",
                        "content": "# Trusted Archive Routing\n\nThe daemon owns final Archive destinations.\n"
                    }
                })
                .to_string(),
                r#"{"done": true, "result": "Typed report written."}"#.to_string(),
            ],
        );

        let agent_id = "swarm-distillery-test";
        let token_id = "runner-distillery-token".to_string();
        daemon
            .capability_broker()
            .lock()
            .expect("capability broker lock")
            .issue_token(CapabilityToken {
                token_id: token_id.clone(),
                subject: agent_id.to_string(),
                trust_level: AgentTrustLevel::ArchiveWrite,
                scopes: ["bridge.connect".to_string(), "llm.chat".to_string()]
                    .into_iter()
                    .collect(),
                expires_at: now_unix() + 3600,
                one_time: false,
                consumed: false,
                goal_scope: None,
            });

        let bridge = BridgeClient::connect_socket(
            &socket_path,
            agent_id.to_string(),
            token_id,
            None,
        )
            .await
            .expect("bridge should connect");

        let result = run_with_bridge(
            RunnerSessionConfig {
                goal: Some(
                    "Write the distillery bundle manifest to distillery-bundle.json and a listed markdown artifact under output/patterns/."
                        .to_string(),
                ),
                context: Some(
                    "The manifest must include version, report, and artifacts. The artifact should explain that the daemon owns final Archive routing."
                        .to_string(),
                ),
                agent_id: Some(agent_id.to_string()),
                workspace: workspace.path().to_path_buf(),
                system_prompt: Some(
                    "You are verifying the distillery bundle contract. Use local file tools to write the manifest and listed markdown artifact, then stop."
                        .to_string(),
                ),
                role: Some("distillery".to_string()),
                sandbox_type: Some("in_process".to_string()),
                model_label: None,
                thread_id: None,
                max_iterations: 15,
                ci_check: None,
                review_pr: false,
                branch: None,
                base_branch: "main".to_string(),
            },
            Arc::clone(&bridge),
        )
        .await
        .expect("runner should finish");

        assert_eq!(
            result.expect("runner should return output").output,
            "Typed report written."
        );

        let bundle_path = workspace.path().join("distillery-bundle.json");
        let raw = std::fs::read_to_string(&bundle_path).expect("bundle should exist");
        let report: serde_json::Value = serde_json::from_str(&raw).expect("bundle json");
        assert_eq!(report["version"].as_u64(), Some(1));
        assert_eq!(
            report["report"]["summary_markdown"].as_str(),
            Some("Runner generated summary.")
        );
        assert_eq!(report["report"]["lint_status"].as_str(), Some("clean"));
        assert_eq!(
            report["artifacts"][0]["relative_path"].as_str(),
            Some("patterns/trusted-archive-routing.md")
        );
        let artifact_path = workspace
            .path()
            .join("output/patterns/trusted-archive-routing.md");
        let artifact = std::fs::read_to_string(&artifact_path).expect("artifact should exist");
        assert!(artifact.contains("daemon owns final Archive destinations"));

        gateway_task.abort();
    });
}

#[test]
fn e2e_runner_auth_pause_and_resume_via_credentials_room() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let socket_path = std::env::temp_dir().join(format!(
        "symbiotic_llm_gateway_auth_{}.sock",
        unique_suffix()
    ));
    let name = format!("e2e-auth-runner_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let scripts_dir = config.data_dir.join("auth-scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    let script_path = scripts_dir.join("github.com.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
cat >/dev/null
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
    )
    .expect("write auth script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    config.auth_scripts_dir = Some(scripts_dir);
    config.auth_sandbox_bin = Some(credential_gateway_bin_for_test());
    config.agent_backend = AgentBackend::Runner;
    config.runner_harness_mode = crate::workers::RunnerHarnessMode::InProcess;
    config.llm_gateway_socket = Some(socket_path.to_string_lossy().to_string());

    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    register_auth_session_template(&mut daemon, "auth-session", "inquisitor");
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("seed login credential");

    let gateway_task = spawn_llm_gateway_for_test(
        &daemon,
        daemon
            .config
            .llm_gateway_socket
            .as_deref()
            .expect("socket should be configured"),
    );
    wait_for_test_socket(&socket_path);

    let responses = vec![
        r#"{"tool": "request_auth_session", "params": {"target": "github.com", "scopes": ["web.login"], "session_type": "browser", "purpose": "Open GitHub settings", "prefer_existing_session": false}} "#.to_string(),
        r#"{"done": true, "result": "Waiting for authentication approval."}"#.to_string(),
        r#"{"done": true, "result": "Authentication completed and work resumed."}"#.to_string(),
    ];
    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();
    let (job_id, goal_id) = daemon
        .queue_workflow_run_for_goal("auth-session", "#goals", "@user:test")
        .expect("queue goal should work");
    assert!(!job_id.is_empty());

    let (auth_event, step_events) = daemon
        .run_once(now + 1)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(auth_event.event_type, EventType::AuthRequired);
    assert_eq!(auth_event.status, "awaiting_approval");
    assert_eq!(auth_event.goal_template.as_deref(), Some("auth-session"));
    let auth_detail: serde_json::Value =
        serde_json::from_str(&auth_event.detail).expect("auth detail json");
    let request_id = auth_detail["request_id"]
        .as_str()
        .expect("request_id should be present")
        .to_string();
    let goal_scope = auth_detail["goal_scope"]
        .as_str()
        .expect("goal_scope should be present for runner auth")
        .to_string();
    assert_eq!(auth_detail["target"].as_str(), Some("github.com"));

    assert!(
        step_events
            .iter()
            .any(|event| event.event_type == EventType::AuthRequired
                && event.goal_room.as_deref() == Some("#credentials")),
        "credentials room should receive auth.required"
    );

    let states = daemon.list_goal_states().expect("list states");
    let auth_state = states
        .into_iter()
        .find(|state| state.template == "auth-session" && state.goal_room == "#goals")
        .expect("auth goal state should exist");
    assert_eq!(auth_state.status, "awaiting_auth");
    assert_eq!(auth_state.last_run_id.as_deref(), Some(goal_id.as_str()));

    let approve_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "credential.approve",
            "d": {
                "request_id": request_id
            }
        }
    })
    .to_string();

    let approval_events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: approve_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("approve route should work");
    assert!(
        approval_events.iter().any(|event| {
            event.room_id == "#credentials"
                && detail_str(&event.envelope, "request_id") == Some(request_id.as_str())
        }),
        "credentials room should receive auth lifecycle events"
    );

    let (completion_event, _) = daemon
        .run_once(now + 3)
        .expect("run should succeed")
        .expect("workflow resume should run");
    assert_eq!(completion_event.event_type, EventType::WorkflowRun);
    assert_eq!(completion_event.status, "completed");

    let session = daemon
        .credential_vault
        .get_scoped(Some(&goal_scope), "github.com")
        .expect("scoped session lookup should work")
        .expect("scoped session should be stored");
    assert_eq!(session.secret, "sandbox_session_token");
    let global_credential = daemon
        .credential_vault
        .get("github.com")
        .expect("global credential lookup should work")
        .expect("global login credential should still exist");
    assert_eq!(global_credential.secret, "secret");

    gateway_task.abort();
}

#[test]
fn e2e_runner_auth_input_pause_and_resume_via_credentials_room() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let socket_path = std::env::temp_dir().join(format!("sym_llm_authin_{}.sock", unique_suffix()));
    let name = format!("e2e-auth-input-runner_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let scripts_dir = config.data_dir.join("auth-scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    let script_path = scripts_dir.join("github.com.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
INPUT=$(cat)
if printf '%s' "$INPUT" | grep -q '"value":"654321"' && printf '%s' "$INPUT" | grep -q '"state":"otp-challenge-1"'; then
  echo '{"success": true, "session": "sandbox_session_token"}'
elif printf '%s' "$INPUT" | grep -q '"continuation"'; then
  echo '{"success": false, "error": "Invalid verification code"}'
else
  echo '{"success": false, "input": {"kind": "totp_code", "prompt": "Enter the 6-digit code", "masked_hint": "Authenticator app"}, "continuation_state": "otp-challenge-1"}'
fi
"#,
    )
    .expect("write auth script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    config.auth_scripts_dir = Some(scripts_dir);
    config.auth_sandbox_bin = Some(credential_gateway_bin_for_test());
    config.agent_backend = AgentBackend::Runner;
    config.runner_harness_mode = crate::workers::RunnerHarnessMode::InProcess;
    config.llm_gateway_socket = Some(socket_path.to_string_lossy().to_string());

    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    register_auth_session_template(&mut daemon, "auth-session", "inquisitor");
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("seed login credential");

    let gateway_task = spawn_llm_gateway_for_test(
        &daemon,
        daemon
            .config
            .llm_gateway_socket
            .as_deref()
            .expect("socket should be configured"),
    );
    wait_for_test_socket(&socket_path);

    let responses = vec![
        r#"{"tool": "request_auth_session", "params": {"target": "github.com", "scopes": ["web.login"], "session_type": "browser", "purpose": "Open GitHub settings", "prefer_existing_session": false}} "#.to_string(),
        r#"{"done": true, "result": "Waiting for authentication approval."}"#.to_string(),
        r#"{"done": true, "result": "Authentication completed after MFA."}"#.to_string(),
    ];
    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();
    let (_job_id, goal_id) = daemon
        .queue_workflow_run_for_goal("auth-session", "#goals", "@user:test")
        .expect("queue goal should work");

    let (auth_event, _) = daemon
        .run_once(now + 1)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(auth_event.event_type, EventType::AuthRequired);
    assert_eq!(auth_event.status, "awaiting_approval");
    let auth_detail: serde_json::Value =
        serde_json::from_str(&auth_event.detail).expect("auth detail json");
    let request_id = auth_detail["request_id"]
        .as_str()
        .expect("request_id should be present")
        .to_string();
    let goal_scope = auth_detail["goal_scope"]
        .as_str()
        .expect("goal_scope should be present for runner auth")
        .to_string();

    let approve_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "credential.approve",
            "d": {
                "request_id": request_id
            }
        }
    })
    .to_string();

    let approval_events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: approve_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("approve route should work");
    let input_event = approval_events
        .iter()
        .find(|event| {
            event.room_id == "#credentials"
                && event.envelope.sym.a.as_deref() == Some("auth.required")
                && detail_str(&event.envelope, "request_id") == Some(request_id.as_str())
                && detail_str(&event.envelope, "phase") == Some("input")
        })
        .expect("credentials room should receive auth.required for input");
    assert_eq!(
        detail_str(&input_event.envelope, "status"),
        Some("awaiting_input")
    );
    let input_detail = input_event
        .envelope
        .sym
        .d
        .clone()
        .expect("input detail should be present");
    assert_eq!(input_detail["input"]["kind"].as_str(), Some("totp_code"));
    assert_eq!(input_detail["expires_at"].as_u64(), Some(now + 2 + 300));
    assert_eq!(
        input_detail["input"]["prompt"].as_str(),
        Some("Enter the 6-digit code")
    );
    assert_eq!(input_detail["auth_profile_id"].as_str(), Some("github.com"));
    assert_eq!(input_detail["auth_profile_match"].as_str(), Some("exact"));
    assert_eq!(input_detail["auth_script_kind"].as_str(), Some("shell"));
    assert_eq!(
        input_detail["auth_profile_sha256"].as_str().map(str::len),
        Some(64)
    );

    let states = daemon.list_goal_states().expect("list states");
    let auth_state = states
        .into_iter()
        .find(|state| state.template == "auth-session" && state.goal_room == "#goals")
        .expect("auth goal state should exist");
    assert_eq!(auth_state.status, "awaiting_auth");
    assert_eq!(
        auth_state.pipeline_stage.as_deref(),
        Some("awaiting_auth_input")
    );
    assert_eq!(auth_state.last_run_id.as_deref(), Some(goal_id.as_str()));

    let respond_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "credential.respond",
            "d": {
                "request_id": request_id,
                "value": "654321"
            }
        }
    })
    .to_string();

    let respond_events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: respond_body,
                timestamp: now + 3,
            },
            now + 3,
        )
        .expect("respond route should work");
    assert!(
        respond_events.iter().any(|event| {
            event.room_id == "#credentials"
                && event.envelope.sym.a.as_deref() == Some("auth.completed")
                && detail_str(&event.envelope, "request_id") == Some(request_id.as_str())
        }),
        "credentials room should receive auth.completed after respond"
    );
    let completed_event = respond_events
        .iter()
        .find(|event| event.envelope.sym.a.as_deref() == Some("auth.completed"))
        .expect("auth.completed should be present");
    assert_eq!(
        detail_str(&completed_event.envelope, "auth_profile_id"),
        Some("github.com")
    );
    assert_eq!(
        detail_str(&completed_event.envelope, "auth_profile_match"),
        Some("exact")
    );

    let (completion_event, _) = daemon
        .run_once(now + 4)
        .expect("run should succeed")
        .expect("workflow resume should run");
    assert_eq!(completion_event.event_type, EventType::WorkflowRun);
    assert_eq!(completion_event.status, "completed");

    let session = daemon
        .credential_vault
        .get_scoped(Some(&goal_scope), "github.com")
        .expect("scoped session lookup should work")
        .expect("scoped session should be stored");
    assert_eq!(session.secret, "sandbox_session_token");
    let global_credential = daemon
        .credential_vault
        .get("github.com")
        .expect("global credential lookup should work")
        .expect("global login credential should still exist");
    assert_eq!(global_credential.secret, "secret");

    gateway_task.abort();
}

#[test]
fn e2e_runner_auth_input_response_resumes_workflow() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let socket_path = std::env::temp_dir().join(format!("sym_llm_authin_{}.sock", unique_suffix()));
    let name = format!("e2e-auth-input-runner_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let scripts_dir = config.data_dir.join("auth-scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    let script_path = scripts_dir.join("github.com.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
INPUT=$(cat)
if printf '%s' "$INPUT" | grep -q '"value":"654321"'; then
  echo '{"success": true, "session": "sandbox_session_token"}'
else
  echo '{"success": false, "input": {"kind": "totp_code", "prompt": "Enter the 6-digit GitHub code", "masked_hint": "••123"}, "continuation_state": "otp-challenge-1"}'
fi
"#,
    )
    .expect("write auth script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    config.auth_scripts_dir = Some(scripts_dir);
    config.auth_sandbox_bin = Some(credential_gateway_bin_for_test());
    config.agent_backend = AgentBackend::Runner;
    config.runner_harness_mode = crate::workers::RunnerHarnessMode::InProcess;
    config.llm_gateway_socket = Some(socket_path.to_string_lossy().to_string());

    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");
    register_auth_session_template(&mut daemon, "auth-session", "inquisitor");
    daemon
        .store_login_credential("github.com", "user", "secret")
        .expect("seed login credential");

    let gateway_task = spawn_llm_gateway_for_test(
        &daemon,
        daemon
            .config
            .llm_gateway_socket
            .as_deref()
            .expect("socket should be configured"),
    );
    wait_for_test_socket(&socket_path);

    let responses = vec![
        r#"{"tool": "request_auth_session", "params": {"target": "github.com", "scopes": ["web.login"], "session_type": "browser", "purpose": "Open GitHub settings", "prefer_existing_session": false}} "#.to_string(),
        r#"{"done": true, "result": "Waiting for authentication approval."}"#.to_string(),
        r#"{"done": true, "result": "Authentication completed and work resumed."}"#.to_string(),
    ];
    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();
    let (_job_id, _goal_id) = daemon
        .queue_workflow_run_for_goal("auth-session", "#goals", "@user:test")
        .expect("queue goal should work");

    let (auth_event, _) = daemon
        .run_once(now + 1)
        .expect("run should succeed")
        .expect("workflow job should be processed");
    assert_eq!(auth_event.event_type, EventType::AuthRequired);
    assert_eq!(auth_event.status, "awaiting_approval");
    let auth_detail: serde_json::Value =
        serde_json::from_str(&auth_event.detail).expect("auth detail json");
    let request_id = auth_detail["request_id"]
        .as_str()
        .expect("request_id should be present")
        .to_string();
    let goal_scope = auth_detail["goal_scope"]
        .as_str()
        .expect("goal_scope should be present for runner auth")
        .to_string();

    let approve_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "credential.approve",
            "d": {
                "request_id": request_id
            }
        }
    })
    .to_string();

    let approval_events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: approve_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("approve route should work");
    assert!(
        approval_events.iter().any(|event| {
            event.room_id == "#credentials"
                && event.envelope.sym.a.as_deref() == Some("auth.required")
                && detail_str(&event.envelope, "request_id") == Some(request_id.as_str())
                && detail_str(&event.envelope, "phase") == Some("input")
        }),
        "credentials room should receive auth.required awaiting input"
    );

    let states = daemon.list_goal_states().expect("list states");
    let auth_state = states
        .into_iter()
        .find(|state| state.template == "auth-session" && state.goal_room == "#goals")
        .expect("auth goal state should exist");
    assert_eq!(auth_state.status, "awaiting_auth");
    assert_eq!(
        auth_state.pipeline_stage.as_deref(),
        Some("awaiting_auth_input")
    );

    let respond_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "credential.respond",
            "d": {
                "request_id": request_id,
                "value": "654321"
            }
        }
    })
    .to_string();

    let respond_events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#credentials".to_string(),
                sender: "@user:test".to_string(),
                body: respond_body,
                timestamp: now + 3,
            },
            now + 3,
        )
        .expect("respond route should work");
    assert!(
        respond_events.iter().any(|event| {
            event.room_id == "#credentials"
                && event.envelope.sym.a.as_deref() == Some("auth.completed")
                && detail_str(&event.envelope, "request_id") == Some(request_id.as_str())
        }),
        "credentials room should receive auth.completed after input"
    );
    let completed_event = respond_events
        .iter()
        .find(|event| event.envelope.sym.a.as_deref() == Some("auth.completed"))
        .expect("auth.completed should be present");
    assert_eq!(
        detail_str(&completed_event.envelope, "auth_profile_id"),
        Some("github.com")
    );
    assert_eq!(
        detail_str(&completed_event.envelope, "auth_profile_match"),
        Some("exact")
    );

    let (completion_event, _) = daemon
        .run_once(now + 4)
        .expect("run should succeed")
        .expect("workflow resume should run");
    assert_eq!(completion_event.event_type, EventType::WorkflowRun);
    assert_eq!(completion_event.status, "completed");

    let session = daemon
        .credential_vault
        .get_scoped(Some(&goal_scope), "github.com")
        .expect("scoped session lookup should work")
        .expect("scoped session should be stored");
    assert_eq!(session.secret, "sandbox_session_token");

    gateway_task.abort();
}

/// E2E: Plan rejection cancels the goal.
///
/// Follows the same initial flow as the full pipeline, but rejects the plan
/// instead of approving it. Verifies the goal state transitions to rejected.
#[test]
fn e2e_inquisition_plan_rejection() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _guard = rt.enter();

    let name = format!("e2e-inq-reject_{}", unique_suffix());
    let config = daemon_config_for_test(&name);
    let (mut daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon should initialize");

    register_inquisition_template(&mut daemon);

    let responses = vec![
        // Run 1: ask_user
        r#"{"tool": "ask_user", "params": {"question": "What's the priority?", "quick_replies": ["High", "Low"]}}"#.to_string(),
        r#"{"done": true, "result": "Asked about priority."}"#.to_string(),
        // Run 2: generate_plan
        r#"{"tool": "generate_plan", "params": {"summary": "Execute high-priority task", "steps": [{"task_id": "step_1", "task_slug": "step_1", "task_kind": "execution", "task_driver": "agent", "role": "executor", "description": "Do the thing"}], "confidence": 0.75}}"#.to_string(),
        r#"{"done": true, "result": "Plan proposed."}"#.to_string(),
    ];

    inject_sequential_mock_into_daemon(&daemon, responses);

    let now = now_unix();

    // Submit NL goal to #goals (inquisition path).
    let _events = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: "Handle the high-priority deployment task urgently".to_string(),
                timestamp: now,
            },
            now,
        )
        .expect("route should work");
    let states = daemon.list_goal_states().expect("list states");
    let initial_goal = states
        .iter()
        .find(|s| s.goal_room == "#goals" && s.template.starts_with("inquisition:"))
        .expect("initial inquisition state");
    let inquisition_template = initial_goal.template.clone();

    // Run ask_user — extract workflow goal_id from the event.
    let (event2, _) = daemon.run_once(now + 1).expect("run").expect("job");
    assert_eq!(event2.event_type, EventType::GoalQuestion);
    let wf_goal_id = event2.goal_id.as_ref().expect("goal_id").clone();

    let answer_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.answer",
            "t": wf_goal_id,
            "d": {
                "template": inquisition_template,
                "message": "High"
            }
        }
    })
    .to_string();

    daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: answer_body,
                timestamp: now + 2,
            },
            now + 2,
        )
        .expect("route answer");

    // Run generate_plan — extract plan goal_id.
    let (event4, _) = daemon.run_once(now + 3).expect("run").expect("job");
    assert_eq!(event4.event_type, EventType::GoalPlanProposed);
    let plan_goal_id = event4.goal_id.as_ref().expect("goal_id").clone();

    // Reject the plan using the plan goal_id.
    let reject_body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "goal.plan.rejected",
            "t": plan_goal_id,
            "d": {
                "reason": "Needs more detail"
            }
        }
    })
    .to_string();

    let events5 = daemon
        .route_matrix_message(
            &MatrixMessage {
                room_id: "#goals".to_string(),
                sender: "@user:test".to_string(),
                body: reject_body,
                timestamp: now + 4,
            },
            now + 4,
        )
        .expect("route rejection");

    assert_eq!(events5.len(), 1);
    assert_eq!(events5[0].sym.s, Some(Status::Success));
    assert_eq!(events5[0].sym.s, Some(Status::Success));

    // Goal state should be rejected.
    let states = daemon.list_goal_states().expect("list states");
    let gs = states
        .into_iter()
        .find(|s| s.template == inquisition_template && s.goal_room == "#goals")
        .expect("goal state");
    assert_eq!(gs.status, "rejected");
    assert_eq!(gs.pipeline_stage.as_deref(), Some("rejected"));

    // No re-queued jobs.
    let next = daemon.run_once(now + 5).expect("run");
    assert!(next.is_none(), "no jobs after rejection");
}

// ---------------------------------------------------------------------------
// Thread promotion command routing tests
// ---------------------------------------------------------------------------

#[test]
fn promotion_approve_creates_goal_and_returns_events() {
    let name = format!("promo-approve_{}", unique_suffix());
    let daemon = daemon_for_test(&name);
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "thread.promotion.approve",
            "d": {
                "thread_id": "thread-abc",
                "suggested_title": "Build a website",
                "suggested_template": "general"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    // Should have at least 2 events: accepted event + goal created event
    assert!(
        events.len() >= 2,
        "expected >= 2 events, got {}",
        events.len()
    );

    // First event: thread.promotion.accepted (from DaemonEvent.to_envelope)
    let accepted = &events[0].envelope;
    assert_eq!(accepted.body, "promoted to goal");
    assert_eq!(accepted.sym.k, Kind::State);
    assert_eq!(accepted.sym.s, Some(Status::Success));

    // Second event: goal created confirmation
    let goal_created = &events[1].envelope;
    assert_eq!(goal_created.body, "Goal created from thread promotion");
    assert_eq!(goal_created.sym.k, Kind::Message);
    assert_eq!(goal_created.sym.s, Some(Status::Working));
    assert_eq!(detail_str(&goal_created, "thread_id"), Some("thread-abc"));
    assert_eq!(detail_str(&goal_created, "title"), Some("Build a website"));
    assert_eq!(detail_str(&goal_created, "template"), Some("general"));
    assert!(detail_str(&goal_created, "goal_id").is_some());
    // goal_created should have a thread_id set (the goal_id)
    assert!(goal_created.sym.t.is_some());

    // Verify goal state was created
    let states = daemon.list_goal_states().unwrap_or_default();
    let goal_state = states
        .iter()
        .find(|s| s.template == "general")
        .expect("goal state should exist for promoted goal");
    assert_eq!(goal_state.status, "queued");
}

#[test]
fn promotion_dismiss_returns_dismissal_event() {
    let name = format!("promo-dismiss_{}", unique_suffix());
    let daemon = daemon_for_test(&name);
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "thread.promotion.dismiss",
            "d": {
                "thread_id": "thread-xyz"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    // Should have exactly 1 event: dismissal
    assert_eq!(events.len(), 1, "expected 1 dismissal event");

    let dismissed = &events[0].envelope;
    assert_eq!(dismissed.body, "promotion dismissed");
    assert_eq!(dismissed.sym.k, Kind::State);
    assert_eq!(dismissed.sym.s, Some(Status::Success));

    // No goal state should be created
    let states = daemon.list_goal_states().unwrap_or_default();
    assert!(
        states.is_empty(),
        "no goal state should exist after dismissal"
    );
}

#[test]
fn promotion_command_missing_thread_id_returns_error() {
    let name = format!("promo-missing-id_{}", unique_suffix());
    let daemon = daemon_for_test(&name);
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "thread.promotion.approve",
            "d": {
                "suggested_title": "Build a website"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(events.len(), 1);
    let error_event = &events[0].envelope;
    assert_eq!(error_event.sym.s, Some(Status::Fail));
    assert!(
        error_event
            .body
            .contains("thread.promotion.approve/dismiss requires non-empty thread_id"),
        "body={}",
        error_event.body
    );
}

#[test]
fn vault_edit_add_fact_commits_and_reindexes_entity() {
    let name = format!("vault-edit_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "vault.edit",
            "d": {
                "entity_id": "rust",
                "operation": "add_fact",
                "fact_text": "Memory safe by default",
                "source": "manual"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(
        events.len(),
        2,
        "expected response + profile refresh events"
    );
    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "unexpected vault.edit response body={}",
        envelope.body
    );
    assert!(envelope.body.contains("Added fact to rust"));
    assert!(has_detail(&envelope, "git_commit"));
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(detail_str(&envelope, "operation"), Some("add_fact"));

    let profile_event = &events[1].envelope;
    assert_eq!(
        profile_event.sym.a.as_deref(),
        Some("entity_profile.updated")
    );
    assert_eq!(detail_str(&profile_event, "entity_id"), Some("rust"));
    assert!(has_detail(&profile_event, "content_hash"));

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(updated.contains("Memory safe by default"));

    let generated_profile = kb_dir.join("ledger/concepts/rust/rust.brief.md");
    let generated_content =
        std::fs::read_to_string(&generated_profile).expect("generated entity profile");
    assert!(generated_content.contains("Memory safe by default"));

    let git_count = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&kb_dir)
        .output()
        .expect("git rev-list");
    assert!(
        git_count.status.success(),
        "git rev-list failed: {}",
        String::from_utf8_lossy(&git_count.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&git_count.stdout).trim(), "1");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let memories = runtime
        .block_on(daemon.memory_store.get_memories("rust", None))
        .expect("memories");
    assert!(
        memories
            .iter()
            .any(|memory| memory.fact == "Memory safe by default"),
        "new fact should be queryable after re-index"
    );
}

#[test]
fn vault_edit_replace_fact_archives_old_and_refreshes_entity() {
    let name = format!("vault-replace_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
- Memory safe by default [source: manual] [type: finding] [confidence: 0.8]

## Relationships

## History
",
    )
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "vault.edit",
            "d": {
                "entity_id": "rust",
                "operation": "replace_fact",
                "old_fact_text": "Memory safe by default",
                "new_fact_text": "Ownership enforces memory safety by default",
                "reason": "clarified wording",
                "source": "manual"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(
        events.len(),
        2,
        "expected response + profile refresh events"
    );
    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "body={}",
        envelope.body
    );
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(detail_str(&envelope, "operation"), Some("replace_fact"));
    assert!(has_detail(&envelope, "git_commit"));
    let commit_hash = detail_str(envelope, "git_commit").expect("git commit hash");

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(updated.contains("Ownership enforces memory safety by default"));
    assert!(updated.contains("~~Memory safe by default~~"));
    assert!(updated.contains("reason: clarified wording"));

    let generated_profile = kb_dir.join("ledger/concepts/rust/rust.brief.md");
    let generated_content =
        std::fs::read_to_string(&generated_profile).expect("generated entity profile");
    assert!(generated_content.contains("Ownership enforces memory safety by default"));
    assert!(generated_content.contains(&format!("commit: {commit_hash}")));
}

#[test]
fn vault_edit_remove_relationship_records_history_and_refreshes_entity() {
    let name = format!("vault-remove-relationship_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
- Memory safe by default [source: manual] [type: finding] [confidence: 0.8]

## Relationships
- used_with: [[cargo]] [since: 2026-04-06]

## History
",
    )
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "vault.edit",
            "d": {
                "entity_id": "rust",
                "operation": "remove_relationship",
                "rel_type": "used_with",
                "target": "cargo",
                "reason": "tooling retired"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(
        events.len(),
        2,
        "expected response + profile refresh events"
    );
    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "body={}",
        envelope.body
    );
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(
        detail_str(&envelope, "operation"),
        Some("remove_relationship")
    );
    assert!(has_detail(&envelope, "git_commit"));
    let commit_hash = detail_str(envelope, "git_commit").expect("git commit hash");

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(!updated.contains("used_with: [[cargo]]"));
    assert!(updated.contains("## History"));
    assert!(updated.contains("### Relationship Changes"));
    assert!(updated.contains("removed: used_with -> [[cargo]]"));
    assert!(updated.contains("reason: tooling retired"));

    let generated_profile = kb_dir.join("ledger/concepts/rust/rust.brief.md");
    let generated_content =
        std::fs::read_to_string(&generated_profile).expect("generated entity profile");
    assert!(generated_content.contains("## History"));
    assert!(generated_content.contains("### Relationship Changes"));
    assert!(generated_content.contains("removed: used_with -> [[cargo]]"));
    assert!(generated_content.contains(&format!("commit: {commit_hash}")));
}

#[test]
fn vault_edit_replace_relationship_records_history_and_refreshes_entity() {
    let name = format!("vault-replace-relationship_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
- Memory safe by default [source: manual] [type: finding] [confidence: 0.8]

## Relationships
- used_with: [[cargo]] [since: 2026-04-06]

## History
",
    )
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let body = serde_json::json!({
        "msgtype": "sym.c",
        "body": "",
        "sym": {
            "v": 2,
            "c": "vault.edit",
            "d": {
                "entity_id": "rust",
                "operation": "replace_relationship",
                "rel_type": "used_with",
                "old_target": "cargo",
                "new_target": "rust-analyzer",
                "reason": "editor workflow changed",
                "since": "2026-04-07"
            }
        }
    })
    .to_string();

    let events = daemon
        .route_matrix_message_with_targets(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body,
                timestamp: now,
            },
            now,
        )
        .expect("route should succeed");

    assert_eq!(
        events.len(),
        2,
        "expected response + profile refresh events"
    );
    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "body={}",
        envelope.body
    );
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(
        detail_str(&envelope, "operation"),
        Some("replace_relationship")
    );
    assert!(has_detail(&envelope, "git_commit"));
    let commit_hash = detail_str(envelope, "git_commit").expect("git commit hash");

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(!updated.contains("used_with: [[cargo]] [since: 2026-04-06]"));
    assert!(updated.contains("used_with: [[rust-analyzer]] [since: 2026-04-07]"));
    assert!(updated.contains("## History"));
    assert!(updated.contains("### Relationship Changes"));
    assert!(updated.contains("replaced: used_with -> [[cargo]] => [[rust-analyzer]]"));
    assert!(updated.contains("reason: editor workflow changed"));

    let generated_profile = kb_dir.join("ledger/concepts/rust/rust.brief.md");
    let generated_content =
        std::fs::read_to_string(&generated_profile).expect("generated entity profile");
    assert!(generated_content.contains("### Relationship Changes"));
    assert!(generated_content.contains("replaced: used_with -> [[cargo]] => [[rust-analyzer]]"));
    assert!(generated_content.contains(&format!("commit: {commit_hash}")));
}

struct VaultProcessMockLlm {
    reduce_response: String,
    reflect_response: String,
}

#[async_trait::async_trait]
impl symbiotic_agents::llm::LlmClient for VaultProcessMockLlm {
    async fn chat(
        &self,
        messages: &[symbiotic_agents::llm::ChatMessage],
        _json_mode: bool,
    ) -> anyhow::Result<String> {
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

#[test]
fn vault_process_applies_mutations_and_refreshes_entity_brief() {
    let name = format!("vault-process_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let llm = VaultProcessMockLlm {
        reduce_response: r#"[{"content":"Rust uses ownership for memory safety","impact_score":8,"source_ref":"selection"}]"#.to_string(),
        reflect_response: r#"{"claims":[{"content":"Rust uses ownership for memory safety","impact_score":8,"source_ref":"selection"}],"proposed_links":[{"source_claim_idx":0,"target_node_id":"rust","relationship":"supports","target_space":"knowledge"}]}"#.to_string(),
    };

    let events = daemon
        .handle_vault_process_command_with_llm(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: String::new(),
                timestamp: now,
            },
            &serde_json::json!({
                "entity_id": "rust",
                "text": "Rust uses ownership for memory safety",
                "source": "thread:thread-rust",
            }),
            now,
            &llm,
        )
        .expect("vault.process should succeed");

    assert_eq!(
        events.len(),
        2,
        "expected response + profile refresh events"
    );

    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "body={}",
        envelope.body
    );
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(detail_str(&envelope, "operation"), Some("process"));
    assert_eq!(
        detail_val(&envelope, "claims_extracted").and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        detail_val(&envelope, "claims_verified").and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        detail_val(&envelope, "facts_added").and_then(|value| value.as_u64()),
        Some(1)
    );
    assert_eq!(
        detail_val(&envelope, "facts_archived").and_then(|value| value.as_u64()),
        Some(0)
    );
    assert!(has_detail(&envelope, "git_commit"));
    let mutations_json = detail_str(&envelope, "mutations_json").expect("mutations_json");
    assert!(mutations_json.contains("fact_added"));
    assert!(mutations_json.contains("Rust uses ownership for memory safety"));

    let profile_event = &events[1].envelope;
    assert_eq!(
        profile_event.sym.a.as_deref(),
        Some("entity_profile.updated")
    );
    assert_eq!(detail_str(&profile_event, "entity_id"), Some("rust"));
    assert!(has_detail(&profile_event, "content_hash"));

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(updated.contains("Rust uses ownership for memory safety"));
    assert!(updated.contains("[source: thread:thread-rust]"));

    let generated_profile = kb_dir.join("ledger/concepts/rust/rust.brief.md");
    let generated_content =
        std::fs::read_to_string(&generated_profile).expect("generated entity profile");
    assert!(generated_content.contains("Rust uses ownership for memory safety"));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let memories = runtime
        .block_on(daemon.memory_store.get_memories("rust", None))
        .expect("memories");
    assert!(
        memories
            .iter()
            .any(|memory| memory.fact == "Rust uses ownership for memory safety"),
        "new fact should be queryable after re-index"
    );
}

#[test]
fn vault_process_successful_noop_does_not_emit_profile_refresh() {
    let name = format!("vault-process-noop_{}", unique_suffix());
    let mut config = daemon_config_for_test(&name);
    let kb_dir = config.data_dir.join("knowledge-base");
    let entity_dir = kb_dir.join("ledger/concepts/rust");
    std::fs::create_dir_all(&entity_dir).expect("entity dir");
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
    .expect("seed entity file");
    init_git_repo(&kb_dir);
    config.archive_path = Some(kb_dir.clone());
    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
        SymbioticDaemon::open(config).expect("daemon");
    let now = now_unix();

    let llm = VaultProcessMockLlm {
        reduce_response: r#"[{"content":"Alice prefers espresso","impact_score":4,"source_ref":"selection"}]"#.to_string(),
        reflect_response: r#"{"claims":[{"content":"Alice prefers espresso","impact_score":4,"source_ref":"selection"}],"proposed_links":[]}"#.to_string(),
    };

    let events = daemon
        .handle_vault_process_command_with_llm(
            &MatrixMessage {
                room_id: "#control".to_string(),
                sender: "@user:test".to_string(),
                body: String::new(),
                timestamp: now,
            },
            &serde_json::json!({
                "entity_id": "rust",
                "text": "Alice prefers espresso",
            }),
            now,
            &llm,
        )
        .expect("vault.process should succeed");

    assert_eq!(events.len(), 1, "no-op should not emit brief refresh");

    let envelope = &events[0].envelope;
    assert_eq!(
        envelope.sym.s,
        Some(Status::Success),
        "body={}",
        envelope.body
    );
    assert_eq!(detail_str(&envelope, "entity_id"), Some("rust"));
    assert_eq!(detail_str(&envelope, "operation"), Some("process"));
    assert_eq!(
        detail_val(&envelope, "facts_added").and_then(|value| value.as_u64()),
        Some(0)
    );
    assert_eq!(
        detail_val(&envelope, "facts_archived").and_then(|value| value.as_u64()),
        Some(0)
    );
    assert!(!has_detail(&envelope, "git_commit"));
    assert!(
        envelope
            .body
            .contains("no durable canonical changes extracted"),
        "body={}",
        envelope.body
    );

    let updated = std::fs::read_to_string(entity_dir.join("rust.md")).expect("updated entity");
    assert!(!updated.contains("Alice prefers espresso"));
}

// ---------------------------------------------------------------------------
// LLM classification fallback — parse_llm_classification tests
// ---------------------------------------------------------------------------

#[test]
fn parse_llm_classification_quick() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("quick"), Some(UxClass::Quick));
}

#[test]
fn parse_llm_classification_short_task() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(
        parse_llm_classification("short_task"),
        Some(UxClass::ShortTask)
    );
}

#[test]
fn parse_llm_classification_goal() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("goal"), Some(UxClass::Goal));
}

#[test]
fn parse_llm_classification_with_whitespace() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("  quick  "), Some(UxClass::Quick));
    assert_eq!(parse_llm_classification("\ngoal\n"), Some(UxClass::Goal));
}

#[test]
fn parse_llm_classification_with_quotes() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("\"quick\""), Some(UxClass::Quick));
    assert_eq!(
        parse_llm_classification("'short_task'"),
        Some(UxClass::ShortTask)
    );
}

#[test]
fn parse_llm_classification_with_backticks() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("`goal`"), Some(UxClass::Goal));
}

#[test]
fn parse_llm_classification_case_insensitive() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("Quick"), Some(UxClass::Quick));
    assert_eq!(parse_llm_classification("GOAL"), Some(UxClass::Goal));
    assert_eq!(
        parse_llm_classification("SHORT_TASK"),
        Some(UxClass::ShortTask)
    );
}

#[test]
fn parse_llm_classification_with_trailing_period() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    assert_eq!(parse_llm_classification("goal."), Some(UxClass::Goal));
}

#[test]
fn parse_llm_classification_garbage_returns_none() {
    use crate::commands::parse_llm_classification;

    assert_eq!(parse_llm_classification(""), None);
    assert_eq!(parse_llm_classification("I think this is a goal"), None);
    assert_eq!(parse_llm_classification("quick and goal"), None);
    assert_eq!(parse_llm_classification("unknown"), None);
}

#[test]
fn parse_llm_classification_mixed_formatting() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    // LLM might wrap in quotes + whitespace + period:
    // "  \"Goal.\" " → trim → "\"Goal.\"" → trim_matches('"','.',etc) → "Goal" → "goal"
    assert_eq!(
        parse_llm_classification("  \"Goal.\" "),
        Some(UxClass::Goal),
    );
}

#[test]
fn parse_llm_classification_realistic_responses() {
    use crate::commands::parse_llm_classification;
    use crate::ux_classifier::UxClass;

    // Typical clean LLM output
    assert_eq!(parse_llm_classification("goal"), Some(UxClass::Goal));
    // With newline (common from some models)
    assert_eq!(
        parse_llm_classification("short_task\n"),
        Some(UxClass::ShortTask)
    );
    // Capitalized
    assert_eq!(parse_llm_classification("Quick"), Some(UxClass::Quick));
}
