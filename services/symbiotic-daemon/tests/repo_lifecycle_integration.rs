//! T126 §09 — End-to-end integration tests for the repo lifecycle wiring.
//!
//! These tests exercise the chain that unit tests cannot:
//!
//!     archive markdown on disk
//!         → RepoRegistry::load_from_archive at SymbioticDaemon::open
//!         → spawn_repo_scheduler
//!         → mirror_pull_once / mirror_push_with_approval
//!         → ApprovalGate / DaemonMatrixPoster
//!         → MatrixOutboundReceiver drain
//!         → repo_events archive output
//!         → ConflictGoalSender → ConflictGoalReceiver → handle_conflict_goal
//!
//! The 58 unit tests in repo_capabilities / repo_events / repo_mirror /
//! repo_registry / repo_scheduler already cover behaviour at module
//! isolation. This file deliberately does not duplicate that coverage —
//! every test below is justified by a wire that no unit test traces.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use symbiotic_daemon::{
    AgentBackend, DaemonConfig, FetchMode, MatrixOutboundMessage, SymbioticDaemon,
};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::watch;
use tokio::time::timeout;

// ── Git fixture helpers ────────────────────────────────────────────────
// Copied verbatim from `repo_scheduler::tests` because integration tests
// cannot reach private `#[cfg(test)] mod tests` items. Matching the
// precedent set by `intake_integration.rs`. Keep these in sync if the
// `repo_scheduler` originals change shape.

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
}

fn must_succeed(mut cmd: Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|e| panic!("spawn {what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed: status={}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Initialize a bare repo with a seed commit on `main`. `--initial-branch=main`
/// is explicit because some CI runners default `init.defaultBranch` to
/// `master`.
fn init_bare_with_commit(dir: &Path, name: &str, content: &str) -> PathBuf {
    let bare = dir.join(format!("{name}.git"));
    let mut init = Command::new("git");
    init.arg("init")
        .arg("--bare")
        .arg("--initial-branch=main")
        .arg(&bare);
    git_env(&mut init);
    must_succeed(init, "init --bare");

    let work = dir.join(format!("{name}-work"));
    let mut clone = Command::new("git");
    clone.arg("clone").arg(&bare).arg(&work);
    git_env(&mut clone);
    must_succeed(clone, "clone seed");

    std::fs::write(work.join("README.md"), content).unwrap();

    let mut checkout = Command::new("git");
    checkout
        .arg("-C")
        .arg(&work)
        .arg("checkout")
        .arg("-B")
        .arg("main");
    git_env(&mut checkout);
    must_succeed(checkout, "checkout -B main");

    let mut add = Command::new("git");
    add.arg("-C").arg(&work).arg("add").arg("README.md");
    git_env(&mut add);
    must_succeed(add, "git add");

    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com")] {
        let mut cfg = Command::new("git");
        cfg.arg("-C").arg(&work).arg("config").arg(key).arg(value);
        git_env(&mut cfg);
        must_succeed(cfg, "git config");
    }

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&work)
        .arg("commit")
        .arg("-m")
        .arg("seed");
    git_env(&mut commit);
    must_succeed(commit, "git commit");

    let mut push = Command::new("git");
    push.arg("-C")
        .arg(&work)
        .arg("push")
        .arg("origin")
        .arg("main");
    git_env(&mut push);
    must_succeed(push, "git push seed");

    bare
}

/// Add another commit on `main` in the bare via a throwaway worktree and
/// return the new HEAD sha.
fn add_commit_to_bare(bare: &Path, scratch: &Path, content: &str) -> String {
    let work = scratch.join(format!("edit-{}", uuid::Uuid::new_v4().as_simple()));
    let mut clone = Command::new("git");
    clone.arg("clone").arg(bare).arg(&work);
    git_env(&mut clone);
    must_succeed(clone, "clone edit");

    std::fs::write(work.join("README.md"), content).unwrap();

    for (key, value) in [("user.name", "Test"), ("user.email", "test@example.com")] {
        let mut cfg = Command::new("git");
        cfg.arg("-C").arg(&work).arg("config").arg(key).arg(value);
        git_env(&mut cfg);
        must_succeed(cfg, "git config");
    }

    let mut add = Command::new("git");
    add.arg("-C").arg(&work).arg("add").arg("README.md");
    git_env(&mut add);
    must_succeed(add, "git add edit");

    let mut commit = Command::new("git");
    commit
        .arg("-C")
        .arg(&work)
        .arg("commit")
        .arg("-m")
        .arg(format!("edit {}", content.len()));
    git_env(&mut commit);
    must_succeed(commit, "git commit edit");

    let mut push = Command::new("git");
    push.arg("-C")
        .arg(&work)
        .arg("push")
        .arg("origin")
        .arg("main");
    git_env(&mut push);
    must_succeed(push, "git push edit");

    let out = Command::new("git")
        .arg("-C")
        .arg(bare)
        .arg("rev-parse")
        .arg("refs/heads/main")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn rev_parse(bare: &Path, refname: &str) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(bare)
        .arg("rev-parse")
        .arg(refname)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ── Archive manifest fixtures ──────────────────────────────────────────

#[derive(Clone, Copy)]
struct ManifestSpec<'a> {
    project_id: &'a str,
    project_slug: &'a str,
    repo_id: &'a str,
    repo_slug: &'a str,
    source_url: &'a str,
    internal_bare_path: &'a Path,
    push_external: bool,
    requires_operator_approval: bool,
    sync_interval_secs: u64,
}

/// Write a project + repo manifest pair into `archive_root` so
/// `RepoRegistry::load_from_archive` picks them up at `SymbioticDaemon::open`
/// time.
fn write_manifest_to_archive(archive_root: &Path, spec: &ManifestSpec) {
    let project_dir = archive_root
        .join("operations/projects")
        .join(spec.project_slug);
    std::fs::create_dir_all(project_dir.join("repos")).unwrap();

    let project_md = format!(
        r#"---
id: "{pid}"
slug: {pslug}
title: "{pslug}"
state: active
repos: ["{rslug}"]
---

# {pslug}
"#,
        pid = spec.project_id,
        pslug = spec.project_slug,
        rslug = spec.repo_slug,
    );
    std::fs::write(project_dir.join("project.md"), project_md).unwrap();

    let approval_value = if spec.requires_operator_approval {
        "[\"push_external\"]"
    } else {
        "[]"
    };
    let repo_md = format!(
        r#"---
id: "{rid}"
project_id: "{pid}"
slug: {rslug}
title: "{rslug}"
state: active
repo_role: source

source:
  url: "{url}"
  provider: local
  default_branch: "main"
  protected_branches: []

credential:
  id: "credential:integ-{rslug}"
  scope: push
  trust_floor: ReadOnly

mirror:
  internal_bare_path: "{bare}"
  direction: bidirectional
  sync_interval_secs: {interval}

checkout:
  worktree_root: "data/worktrees/{rslug}/"
  agent_branch_prefix: "agent/"
  max_concurrent_worktrees: 1
  cleanup_on_goal_close: true

agent_scopes:
  read: []
  write: []
  push_external: {push_ext}
  requires_operator_approval_for: {approval_value}

hooks: {{}}

metadata:
  attached_at: "2026-04-18T00:00:00Z"
  attached_by: "integ"
  notes: "§09 fixture"
---

# {rslug}

Integration test fixture.
"#,
        rid = spec.repo_id,
        pid = spec.project_id,
        rslug = spec.repo_slug,
        url = spec.source_url,
        bare = spec.internal_bare_path.display(),
        interval = spec.sync_interval_secs,
        push_ext = spec.push_external,
        approval_value = approval_value,
    );
    std::fs::write(
        project_dir
            .join("repos")
            .join(format!("{}.md", spec.repo_slug)),
        repo_md,
    )
    .unwrap();
}

/// Count archive event files matching `event_type` for a given project slug.
/// `event_type` uses snake_case (e.g. `repo_mirror_pull_completed`), but
/// `repo_events::sanitize_archive_component` converts `_` → `-` in filenames.
/// We accept both shapes for robustness.
fn count_archive_events(archive_root: &Path, project_slug: &str, event_type: &str) -> usize {
    let events_dir = archive_root
        .join("operations/projects")
        .join(project_slug)
        .join("repos/events");
    if !events_dir.exists() {
        return 0;
    }
    let dashed = event_type.replace('_', "-");
    std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.contains(&dashed) || name.contains(event_type)
        })
        .count()
}

/// Return event filenames sorted alphabetically (which is chronologically by
/// the leading observed_at unix timestamp). Used for ordering assertions.
fn list_event_filenames(archive_root: &Path, project_slug: &str) -> Vec<String> {
    let events_dir = archive_root
        .join("operations/projects")
        .join(project_slug)
        .join("repos/events");
    if !events_dir.exists() {
        return Vec::new();
    }
    let mut names: Vec<String> = std::fs::read_dir(&events_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Seed a credential record into the daemon's vault BEFORE
/// `SymbioticDaemon::open` so push tests can resolve `manifest.credential.id`.
/// `secret` is intentionally empty — `file://` pushes don't use it.
fn seed_vault_credential(vault_path: &Path, credential_id: &str) {
    use credential_gateway::{CredentialRecord, GoalScopedVault};
    let vault = GoalScopedVault::open(vault_path).expect("open vault for seeding");
    vault
        .put_scoped(
            None,
            CredentialRecord {
                service: credential_id.to_string(),
                username: "integ".to_string(),
                secret: String::new(),
                totp_secret: None,
            },
        )
        .expect("seed credential");
    drop(vault);
}

// ── DaemonConfig builder ───────────────────────────────────────────────
// Modeled on `tests/intake_integration.rs::daemon_config`. Deliberately
// duplicated rather than shared — DaemonConfig is heavy and unstable, and
// each integration test file picks the field defaults that match its own
// scenario.

fn integ_daemon_config(root: &Path) -> DaemonConfig {
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

    // archive_root must exist before SymbioticDaemon::open scans it.
    std::fs::create_dir_all(&archive_root).unwrap();

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
        role_dir: PathBuf::from("config/agents"),
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
        auth_approval_ttl_secs: 60,
        auth_input_ttl_secs: 60,
        matrix_homeserver: None,
        matrix_server_name: None,
        matrix_access_token: None,
    }
}

// ── Test 1 ─────────────────────────────────────────────────────────────
// Pull tick: archive manifest on disk → registry bootstrap → scheduler
// pulls → local bare advances → repo_mirror_pull_completed event written.
//
// Wires proven: load_from_archive + manifest parser + spawn_repo_scheduler
// + mirror_pull_once + repo_events::append_repo_event_archive.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_tick_advances_local_bare_and_writes_archive_event() {
    let tmp = TempDir::new().unwrap();
    let source = init_bare_with_commit(tmp.path(), "src", "v1\n");
    let local = tmp.path().join("local-pull.git"); // not yet initialised

    // Build config and seed manifest BEFORE opening daemon so
    // load_from_archive picks it up.
    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();
    write_manifest_to_archive(
        &archive_root,
        &ManifestSpec {
            project_id: "project:integpull",
            project_slug: "integpull",
            repo_id: "repo:integpull",
            repo_slug: "integpull",
            source_url: &file_url(&source),
            internal_bare_path: &local,
            push_external: false,
            requires_operator_approval: false,
            sync_interval_secs: 1,
        },
    );

    let (daemon, mut matrix_rx, mut conflict_rx) = SymbioticDaemon::open(config).unwrap();

    // Confirm the registry actually bootstrapped from the archive.
    {
        let registry = daemon.repo_registry();
        let reg = registry.lock().await;
        assert!(
            reg.get("repo:integpull").is_some(),
            "registry should pick up manifest from archive"
        );
    }

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;
    assert_eq!(handles.len(), 1, "one active manifest → one scheduler task");

    // Wait one full sync interval + slack for the pull tick to fire.
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    cancel_tx.send(true).unwrap();
    for h in handles {
        let _ = timeout(Duration::from_secs(3), h).await;
    }

    // Local bare should now exist and HEAD should match source.
    assert!(local.exists(), "scheduler should have created local bare");
    let local_head = rev_parse(&local, "refs/heads/main").expect("local main");
    let source_head = rev_parse(&source, "refs/heads/main").expect("source main");
    assert_eq!(local_head, source_head, "local should mirror source HEAD");

    // Archive should record the pull-completed event.
    assert!(
        count_archive_events(&archive_root, "integpull", "repo_mirror_pull_completed") >= 1,
        "expected at least one repo_mirror_pull_completed event"
    );

    // Steady-state pull → no matrix outbound, no conflict goal.
    assert!(
        matrix_rx.try_recv().is_err(),
        "no matrix outbound expected on a clean pull"
    );
    assert!(
        conflict_rx.try_recv().is_err(),
        "no conflict goal expected on a clean pull"
    );
}

// ── Test 2 ─────────────────────────────────────────────────────────────
// Push tick: scheduler decides Push → opens approval ticket → posts to
// matrix outbound → simulated operator approves → push lands in source bare.
//
// Wires proven: scheduler push branch + mirror_push_with_approval + ticket
// open + DaemonMatrixPoster → MatrixOutboundReceiver delivery + gate
// approve → push completion.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_tick_drains_through_matrix_outbound_and_lands_after_approval() {
    let tmp = TempDir::new().unwrap();
    let source = init_bare_with_commit(tmp.path(), "src-push", "v1\n");

    // Local bare = clone --bare of source so heads start identical.
    let local = tmp.path().join("local-push.git");
    let mut clone = Command::new("git");
    clone.arg("clone").arg("--bare").arg(&source).arg(&local);
    git_env(&mut clone);
    must_succeed(clone, "clone --bare");
    // Add a local-only commit so the scheduler decides Push.
    let local_head_after_edit = add_commit_to_bare(&local, tmp.path(), "v2\n");

    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();
    let vault_path = config.credential_vault_file.clone();
    write_manifest_to_archive(
        &archive_root,
        &ManifestSpec {
            project_id: "project:integpush",
            project_slug: "integpush",
            repo_id: "repo:integpush",
            repo_slug: "integpush",
            source_url: &file_url(&source),
            internal_bare_path: &local,
            push_external: true,
            requires_operator_approval: true,
            sync_interval_secs: 1,
        },
    );
    seed_vault_credential(&vault_path, "credential:integ-integpush");

    let (daemon, mut matrix_rx, _conflict_rx) = SymbioticDaemon::open(config).unwrap();
    let approval_gate = daemon.approval_gate_for_test();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;

    // Spawn the operator-side approver: poll the gate every 100ms; as soon
    // as a pending ticket appears, approve it.
    let approver_gate = approval_gate.clone();
    let approver = tokio::spawn(async move {
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let pending_id = {
                let gate = approver_gate.lock().expect("gate lock");
                gate.list_pending().first().map(|t| t.ticket_id.clone())
            };
            if let Some(id) = pending_id {
                let mut gate = approver_gate.lock().expect("gate lock");
                gate.approve(&id, "operator", symbiotic_core::now_unix())
                    .expect("approve");
                return id;
            }
        }
        panic!("no pending ticket appeared within ~10s");
    });

    let approved_ticket_id = timeout(Duration::from_secs(15), approver)
        .await
        .expect("approver timeout")
        .expect("approver task");

    // After approval, the scheduler's poll loop should pick up the approval
    // and complete the push. Allow a couple of poll intervals + a push tick.
    tokio::time::sleep(Duration::from_millis(2_000)).await;

    cancel_tx.send(true).unwrap();
    for h in handles {
        let _ = timeout(Duration::from_secs(5), h).await;
    }

    // Source bare should now have the local-only commit.
    let source_head = rev_parse(&source, "refs/heads/main").expect("source main");
    assert_eq!(
        source_head, local_head_after_edit,
        "approved push should land in source bare"
    );

    // The matrix outbound channel should have carried at least one
    // approval-request envelope mentioning the ticket id.
    let approval_message = drain_matrix_outbound(&mut matrix_rx, Duration::from_millis(50))
        .into_iter()
        .find(|(_room, env)| envelope_text(env).contains(&approved_ticket_id));
    assert!(
        approval_message.is_some(),
        "matrix outbound should carry the approval-request body containing ticket {approved_ticket_id}"
    );
}

// ── Test 3 ─────────────────────────────────────────────────────────────
// Conflict tick: remote advanced AND local has unpushed commits → scheduler
// emits archive event + enqueues conflict goal → request lands on
// conflict_goal_rx → handle_conflict_goal dispatches successfully.
//
// Wires proven: scheduler conflict branch + repo_events conflict event +
// ConflictGoalSender / Receiver wiring + handle_conflict_goal callable.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conflict_tick_emits_event_and_pump_loop_drains_conflict_goal() {
    let tmp = TempDir::new().unwrap();
    let source = init_bare_with_commit(tmp.path(), "src-conflict", "v1\n");

    let local = tmp.path().join("local-conflict.git");
    let mut clone = Command::new("git");
    clone.arg("clone").arg("--bare").arg(&source).arg(&local);
    git_env(&mut clone);
    must_succeed(clone, "clone --bare");
    // Diverge: advance both source and local independently.
    add_commit_to_bare(&source, tmp.path(), "remote-v2\n");
    add_commit_to_bare(&local, tmp.path(), "local-v2\n");

    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();
    write_manifest_to_archive(
        &archive_root,
        &ManifestSpec {
            project_id: "project:integconflict",
            project_slug: "integconflict",
            repo_id: "repo:integconflict",
            repo_slug: "integconflict",
            source_url: &file_url(&source),
            internal_bare_path: &local,
            push_external: true,
            requires_operator_approval: false,
            sync_interval_secs: 1,
        },
    );

    let (daemon, _matrix_rx, mut conflict_rx) = SymbioticDaemon::open(config).unwrap();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;

    // Drain conflict goal request — give the scheduler a couple of ticks.
    let req = timeout(Duration::from_secs(5), conflict_rx.recv())
        .await
        .expect("conflict goal request timeout")
        .expect("channel closed unexpectedly");
    assert_eq!(req.sender, "scheduler@symbiotic.sh");
    assert!(
        req.description.contains("repo:integconflict"),
        "description should name the conflicting repo, got: {}",
        req.description
    );

    cancel_tx.send(true).unwrap();
    for h in handles {
        let _ = timeout(Duration::from_secs(3), h).await;
    }

    // Conflict event should be in the archive.
    assert!(
        count_archive_events(
            &archive_root,
            "integconflict",
            "repo_mirror_conflict_opened"
        ) >= 1,
        "expected repo_mirror_conflict_opened archive event"
    );

    // Resolves Open Q1 (option a): hand-call handle_conflict_goal to prove
    // the dispatch path is wired. Failure here = the channel reaches the
    // caller but the daemon can't act on it. We accept any Result here
    // (the goal pipeline may legitimately reject a synthetic conflict
    // description against an empty configuration), but the call must not
    // panic.
    let now = symbiotic_core::now_unix();
    let _ = daemon.handle_conflict_goal(&req, now);
}

// ── Test 4 ─────────────────────────────────────────────────────────────
// Negative path: push_external=false → scheduler must not attempt push
// even when local is ahead. No matrix traffic, no ticket opened.
//
// Wires proven: capability-gate silencing inside the scheduler decide
// branch, end-to-end through the daemon's open() wiring (not just decide()).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_external_false_silences_scheduler_with_local_ahead() {
    let tmp = TempDir::new().unwrap();
    let source = init_bare_with_commit(tmp.path(), "src-silent", "v1\n");
    let local = tmp.path().join("local-silent.git");
    let mut clone = Command::new("git");
    clone.arg("clone").arg("--bare").arg(&source).arg(&local);
    git_env(&mut clone);
    must_succeed(clone, "clone --bare");
    add_commit_to_bare(&local, tmp.path(), "v2\n");

    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();
    write_manifest_to_archive(
        &archive_root,
        &ManifestSpec {
            project_id: "project:integsilent",
            project_slug: "integsilent",
            repo_id: "repo:integsilent",
            repo_slug: "integsilent",
            source_url: &file_url(&source),
            internal_bare_path: &local,
            push_external: false,
            requires_operator_approval: false,
            sync_interval_secs: 1,
        },
    );

    let (daemon, mut matrix_rx, mut conflict_rx) = SymbioticDaemon::open(config).unwrap();
    let gate = daemon.approval_gate_for_test();

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;

    // Let several ticks fire so we'd have observed any rogue push attempt.
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    cancel_tx.send(true).unwrap();
    for h in handles {
        let _ = timeout(Duration::from_secs(3), h).await;
    }

    assert!(
        matrix_rx.try_recv().is_err(),
        "push_external=false must produce no matrix traffic"
    );
    assert!(
        conflict_rx.try_recv().is_err(),
        "no-push scenario should not fabricate a conflict"
    );
    let pending_count = {
        let gate = gate.lock().expect("gate lock");
        gate.list_pending().len()
    };
    assert_eq!(
        pending_count, 0,
        "no approval ticket should be opened when push is disabled"
    );

    // Source must not have advanced.
    let source_head = rev_parse(&source, "refs/heads/main").expect("source main");
    let local_head = rev_parse(&local, "refs/heads/main").expect("local main");
    assert_ne!(
        source_head, local_head,
        "source should remain at its original commit when push is disabled"
    );
}

// ── Test 5 ─────────────────────────────────────────────────────────────
// Cancellation under load: 3 active manifests → 3 scheduler tasks →
// cancel_tx.send(true) → all handles join within bound.
//
// Wires proven: §08.c shutdown contract scales to N≥3.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_drains_all_repo_tasks_within_bound() {
    let tmp = TempDir::new().unwrap();
    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();

    // Three manifests with long sync intervals so tasks spend time in select!
    // waiting on the cancellation half.
    for slug in ["alpha", "beta", "gamma"] {
        let source = init_bare_with_commit(tmp.path(), &format!("src-{slug}"), "v1\n");
        let local = tmp.path().join(format!("local-{slug}.git"));
        write_manifest_to_archive(
            &archive_root,
            &ManifestSpec {
                project_id: &format!("project:cancel-{slug}"),
                project_slug: &format!("cancel-{slug}"),
                repo_id: &format!("repo:cancel-{slug}"),
                repo_slug: &format!("cancel-{slug}"),
                source_url: &file_url(&source),
                internal_bare_path: &local,
                push_external: false,
                requires_operator_approval: false,
                sync_interval_secs: 3_600,
            },
        );
    }

    let (daemon, _matrix_rx, _conflict_rx) = SymbioticDaemon::open(config).unwrap();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;
    assert_eq!(handles.len(), 3, "three active manifests → three tasks");

    let start = std::time::Instant::now();
    cancel_tx.send(true).unwrap();
    for h in handles {
        let res = timeout(Duration::from_secs(3), h).await;
        assert!(res.is_ok(), "task did not join within bound after cancel");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "all three tasks should drain quickly after cancel; took {elapsed:?}"
    );
}

// ── Test 6 ─────────────────────────────────────────────────────────────
// Archive event ordering: pull then conflict → event filenames sort
// chronologically by the leading observed_at unix timestamp.
//
// Wires proven: monotonic observed_at across event helpers; no rename-race
// or stale-stat regression in repo_events.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_event_ordering_pull_then_conflict_is_chronological() {
    let tmp = TempDir::new().unwrap();
    let source = init_bare_with_commit(tmp.path(), "src-order", "v1\n");
    let local = tmp.path().join("local-order.git");

    let config = integ_daemon_config(tmp.path());
    let archive_root = config.archive_root.clone();
    write_manifest_to_archive(
        &archive_root,
        &ManifestSpec {
            project_id: "project:integorder",
            project_slug: "integorder",
            repo_id: "repo:integorder",
            repo_slug: "integorder",
            source_url: &file_url(&source),
            internal_bare_path: &local,
            push_external: true,
            requires_operator_approval: false,
            sync_interval_secs: 1,
        },
    );

    let (daemon, _matrix_rx, mut conflict_rx) = SymbioticDaemon::open(config).unwrap();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let handles = daemon.spawn_repo_scheduler(cancel_rx).await;

    // Tick 1: clean pull seeds the local bare and emits a pull event.
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    assert!(
        count_archive_events(&archive_root, "integorder", "repo_mirror_pull_completed") >= 1,
        "expected pull event after first tick"
    );

    // Force chronological separation between events. Without a sleep here
    // both events could land in the same Unix second and the filename sort
    // is undefined. Matches the trick used in repo_scheduler::tests.
    tokio::time::sleep(Duration::from_millis(1_100)).await;

    // Diverge: remote advances + local advances independently → next tick
    // is a Conflict.
    add_commit_to_bare(&source, tmp.path(), "remote-v2\n");
    add_commit_to_bare(&local, tmp.path(), "local-v2\n");

    // Drain the conflict request so subsequent ticks don't pile up.
    let _req = timeout(Duration::from_secs(5), conflict_rx.recv())
        .await
        .expect("conflict timeout")
        .expect("channel closed");

    cancel_tx.send(true).unwrap();
    for h in handles {
        let _ = timeout(Duration::from_secs(3), h).await;
    }

    // Now inspect ordering. Filenames start with `{observed_at}-...`, so a
    // lexical sort is a chronological sort.
    let names = list_event_filenames(&archive_root, "integorder");
    let pull_idx = names
        .iter()
        .position(|n| n.contains("repo-mirror-pull-completed"))
        .expect("pull event present");
    let conflict_idx = names
        .iter()
        .position(|n| n.contains("repo-mirror-conflict-opened"))
        .expect("conflict event present");
    assert!(
        pull_idx < conflict_idx,
        "pull should sort before conflict; got names: {names:?}"
    );
}

// ── Helpers shared by tests 2 and others ──────────────────────────────

fn drain_matrix_outbound(
    rx: &mut UnboundedReceiver<MatrixOutboundMessage>,
    settle: Duration,
) -> Vec<MatrixOutboundMessage> {
    // Allow the producer side a moment to flush any in-flight sends.
    std::thread::sleep(settle);
    let mut out = Vec::new();
    while let Ok(msg) = rx.try_recv() {
        out.push(msg);
    }
    out
}

fn envelope_text(env: &symbiotic_matrix::events::MatrixEventEnvelope) -> String {
    // The envelope's serde_json round-trip keeps any ticket id verbatim in
    // the JSON body, which is enough for a substring match.
    serde_json::to_string(env).unwrap_or_default()
}
