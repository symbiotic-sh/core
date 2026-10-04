use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use symbiotic_core::intake::{
    normalize_url, IntakeKind, IntakeRequest, IntakeSource, IntakeStatus,
};
use symbiotic_daemon::{AgentBackend, DaemonConfig, FetchMode, SymbioticDaemon};
use symbiotic_queue::now_unix;
use tempfile::tempdir;

#[derive(Debug, Deserialize)]
struct Fixture {
    cases: Vec<Case>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Case {
    Url {
        id: String,
        url: String,
        checks: Vec<String>,
    },
    Note {
        id: String,
        note: String,
        checks: Vec<String>,
    },
    Bookmarks {
        id: String,
        source: String,
        limit: u32,
        urls: Vec<String>,
        checks: Vec<String>,
    },
}

#[derive(Debug)]
struct CaseReport {
    id: String,
    failures: Vec<String>,
}

#[test]
fn intake_integration_suite() -> Result<()> {
    let fixture = load_fixture()?;
    let mut reports = Vec::new();

    for case in fixture.cases {
        let report = run_case(&case).with_context(|| format!("case {} failed", case_id(&case)))?;
        reports.push(report);
    }

    let failures: Vec<&CaseReport> = reports
        .iter()
        .filter(|report| !report.failures.is_empty())
        .collect();
    if !failures.is_empty() {
        let report = render_report(&reports);
        return Err(anyhow!(report));
    }

    Ok(())
}

fn run_case(case: &Case) -> Result<CaseReport> {
    let temp = tempdir()?;
    let config = daemon_config(temp.path());
    let x_thread_fallback_file = config.x_thread_fallback_file.clone();
    let bookmarks_api_file = config.bookmarks_api_file.clone();
    let bookmarks_browser_file = config.bookmarks_browser_file.clone();

    let mut x_urls = Vec::new();
    match case {
        Case::Url { url, .. } => x_urls.push(url.clone()),
        Case::Bookmarks { urls, .. } => x_urls.extend(urls.iter().cloned()),
        Case::Note { .. } => {}
    }
    seed_x_fallback_fixture(&x_thread_fallback_file, &x_urls)?;

    let (daemon, _matrix_outbound_rx, _conflict_goal_rx) = SymbioticDaemon::open(config)?;

    let mut failures = Vec::new();

    match case {
        Case::Url { url, checks, .. } => {
            let normalized = normalize_url(url)?;
            let result = daemon.submit_intake_request(IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Url,
                urls: vec![normalized.clone()],
                note: None,
                tags: vec!["integration".to_string()],
                file_path: None,
                title: None,
            })?;
            drain_queue(&daemon, 50)?;

            apply_checks(
                &mut failures,
                checks,
                &CaseContext {
                    url: Some(normalized.as_str().to_string()),
                    intake_status: result.items.first().map(|item| item.status.clone()),
                    record_id: None,
                    daemon: &daemon,
                },
            )?;
        }
        Case::Note { note, checks, .. } => {
            let result = daemon.submit_intake_request(IntakeRequest {
                source: IntakeSource::Cli,
                kind: IntakeKind::Note,
                urls: Vec::new(),
                note: Some(note.to_string()),
                tags: vec!["integration".to_string()],
                file_path: None,
                title: None,
            })?;

            let record_id = result.items.first().and_then(|item| item.record_id.clone());
            apply_checks(
                &mut failures,
                checks,
                &CaseContext {
                    url: None,
                    intake_status: result.items.first().map(|item| item.status.clone()),
                    record_id,
                    daemon: &daemon,
                },
            )?;
        }
        Case::Bookmarks {
            source,
            limit,
            urls,
            checks,
            ..
        } => {
            let path = match source.as_str() {
                "api" => &bookmarks_api_file,
                "browser" => &bookmarks_browser_file,
                other => return Err(anyhow!("unsupported bookmarks source {other}")),
            };
            write_bookmarks_fixture(path, urls)?;
            daemon.queue_bookmarks_sync(source, *limit)?;
            drain_queue(&daemon, 200)?;

            for url in urls {
                apply_checks(
                    &mut failures,
                    checks,
                    &CaseContext {
                        url: Some(url.to_string()),
                        intake_status: None,
                        record_id: None,
                        daemon: &daemon,
                    },
                )?;
            }
        }
    }

    Ok(CaseReport {
        id: case_id(case),
        failures,
    })
}

struct CaseContext<'a> {
    url: Option<String>,
    intake_status: Option<IntakeStatus>,
    record_id: Option<String>,
    daemon: &'a SymbioticDaemon,
}

fn apply_checks(
    failures: &mut Vec<String>,
    checks: &[String],
    ctx: &CaseContext<'_>,
) -> Result<()> {
    for check in checks {
        if check == "archive_record" {
            let Some(url) = ctx.url.as_ref() else {
                failures.push("archive_record requires url".to_string());
                continue;
            };
            let record = ctx.daemon.archive_record_by_url(url)?;
            if record.is_none() {
                failures.push(format!("archive_record missing for {url}"));
            }
            continue;
        }
        if check == "review_record" {
            let Some(url) = ctx.url.as_ref() else {
                failures.push("review_record requires url".to_string());
                continue;
            };
            let record = ctx.daemon.archive_record_by_url(url)?;
            let Some(record) = record else {
                failures.push(format!("review_record missing archive record for {url}"));
                continue;
            };
            let review = ctx.daemon.review_record(&record.record_id)?;
            if review.is_none() {
                failures.push(format!("review_record missing for {}", record.record_id));
            }
            continue;
        }
        if check == "vault_record" {
            let Some(record_id) = ctx.record_id.as_ref() else {
                failures.push("vault_record requires record_id".to_string());
                continue;
            };
            let records = ctx.daemon.vault_records()?;
            if !records.iter().any(|record| &record.record_id == record_id) {
                failures.push(format!("vault_record missing for {record_id}"));
            }
            continue;
        }
        if check == "secure_routed" {
            if ctx.intake_status != Some(IntakeStatus::SecureRouted) {
                failures.push(format!(
                    "expected secure_routed, got {:?}",
                    ctx.intake_status
                ));
            }
            continue;
        }
        if let Some(value) = check.strip_prefix("content_contains:") {
            let Some(url) = ctx.url.as_ref() else {
                failures.push("content_contains requires url".to_string());
                continue;
            };
            let record = ctx.daemon.archive_record_by_url(url)?;
            let Some(record) = record else {
                failures.push(format!("content_contains missing archive record for {url}"));
                continue;
            };
            if !record.content.contains(value) {
                failures.push(format!("content missing '{value}' for {url}"));
            }
            continue;
        }

        failures.push(format!("unknown check: {check}"));
    }
    Ok(())
}

fn drain_queue(daemon: &SymbioticDaemon, max_iterations: usize) -> Result<()> {
    for now in (now_unix()..).take(max_iterations) {
        if daemon.run_once(now)?.is_none() {
            return Ok(());
        }
    }
    Err(anyhow!(
        "queue did not drain after {max_iterations} iterations"
    ))
}

fn write_bookmarks_fixture(path: &Path, urls: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create bookmarks fixture dir {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, urls.join("\n"))
        .with_context(|| format!("failed to write bookmarks fixture {}", path.display()))?;
    Ok(())
}

fn seed_x_fallback_fixture(path: &Path, urls: &[String]) -> Result<()> {
    let mut lines = Vec::new();
    for url in urls {
        if let Ok(normalized) = normalize_url(url) {
            if let Some(host) = normalized.host_str() {
                if (host == "x.com" || host.ends_with(".x.com") || host == "twitter.com")
                    && normalized.path().contains("/status/")
                {
                    lines.push(format!(
                        "{}\tIntegration fallback content for {}",
                        normalized.as_str(),
                        normalized.as_str()
                    ));
                }
            }
        }
    }

    if lines.is_empty() {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create x fallback fixture dir {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("failed to write x fallback fixture {}", path.display()))
}

fn load_fixture() -> Result<Fixture> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("intake_cases.json");
    let payload = fs::read_to_string(&path)
        .with_context(|| format!("failed to read fixture {}", path.display()))?;
    let fixture = serde_json::from_str(&payload)
        .with_context(|| format!("invalid fixture JSON {}", path.display()))?;
    Ok(fixture)
}

fn case_id(case: &Case) -> String {
    match case {
        Case::Url { id, .. } => id.clone(),
        Case::Note { id, .. } => id.clone(),
        Case::Bookmarks { id, .. } => id.clone(),
    }
}

fn daemon_config(root: &Path) -> DaemonConfig {
    let data_dir = root.join("data");
    let queue_file = data_dir.join("queue/jobs.state");
    let archive_root = data_dir.join("archive");
    let vault_root = data_dir.join("vault");
    let review_root = data_dir.join("review");
    let domain_root = root.join("domains");
    let audit_root = data_dir.join("audit");
    let intake_root = data_dir.join("intake");
    let goals_root = data_dir.join("goals");
    let agents_root = data_dir.join("agents");
    let push_root = data_dir.join("push");

    DaemonConfig {
        queue_file,
        archive_root,
        vault_root: vault_root.clone(),
        review_root,
        domain_root,
        credential_vault_file: vault_root.join("credentials.tsv"),
        audit_log_file: audit_root.join("context.log"),
        capability_tokens_file: audit_root.join("capability-tokens.json"),
        bookmarks_api_file: intake_root.join("bookmarks-api.txt"),
        bookmarks_browser_file: intake_root.join("bookmarks-browser.txt"),
        x_thread_fallback_file: intake_root.join("twitter-threads.txt"),
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
        goal_log_file: goals_root.join("runs.log"),
        goal_state_file: goals_root.join("state.tsv"),
        agent_log_file: agents_root.join("runs.log"),
        agent_state_file: agents_root.join("state.tsv"),
        push_registry_file: push_root.join("tokens.tsv"),
        push_token_key_file: push_root.join("push-token.key"),
        push_outbox_file: push_root.join("outbox.ndjson"),
        push_ack_file: push_root.join("acks.ndjson"),
        push_telemetry_file: push_root.join("delivery.log"),
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
        retry_backoff_seconds: 30,
        max_matrix_message_bytes: 32 * 1024,
        blocked_hosts: Default::default(),
        room_roles: Default::default(),
        allowed_senders: Default::default(),
        allow_open_access: false,
        data_dir,
        ollama_url: None,
        ollama_chat_model: None,
        model_manifest_file: root.join("config").join("model-manifest.toml"),
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

        role_dir: std::path::PathBuf::from("config/agents"),
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
        push_preferences_file: push_root.join("preferences.toml"),
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

fn render_report(reports: &[CaseReport]) -> String {
    let mut summary = HashMap::new();
    for report in reports {
        summary.insert(&report.id, report.failures.len());
    }

    let mut out = String::new();
    out.push_str("INTAKE INTEGRATION TEST AUDIT\n");
    out.push_str(&format!("Run: {}\n", now_unix()));
    out.push_str(&format!("Commit: {}\n\n", resolve_commit()));

    for report in reports {
        if report.failures.is_empty() {
            continue;
        }
        out.push_str(&format!("Case: {}\n", report.id));
        for failure in &report.failures {
            out.push_str(&format!("  - {failure}\n"));
        }
        out.push('\n');
    }

    out
}

fn resolve_commit() -> String {
    std::env::var("SYMBIOTIC_COMMIT")
        .or_else(|_| std::env::var("GIT_COMMIT"))
        .or_else(|_| std::env::var("GITHUB_SHA"))
        .unwrap_or_else(|_| "unknown".to_string())
}
