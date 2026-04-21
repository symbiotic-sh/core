use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use credential_gateway::FileCredentialVault;
use log::{debug, info, warn};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_daemon::bootstrap::{self, BootstrapConfig};
use symbiotic_daemon::{DaemonConfig, DaemonEvent, EventType, RoomRole, SymbioticDaemon};
use symbiotic_matrix::events::MatrixEventEnvelope;
use symbiotic_matrix::transport::{
    CurlMatrixHttpClient, FileMatrixTransport, LiveMatrixConfig, LiveMatrixTransport,
    MatrixSdkConfig, MatrixSdkTransport, MatrixTransport,
};
use symbiotic_queue::now_unix;

#[cfg(unix)]
static SHOULD_STOP: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Parser)]
#[command(name = "symbiotic-daemon")]
#[command(about = "Nucleus daemon runner for Symbiotic")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

/// Format a step event body into human-readable text for the app UI.
///
/// Transforms raw detail strings like "step=execute type=agent.execute index=1 total=2"
/// into clean messages like "Running step: execute (1/2)".
fn parse_event_detail_map(raw: &str) -> Vec<(String, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return Vec::new();
    };
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .map(|(key, value)| {
            let rendered = match value {
                serde_json::Value::Null => String::new(),
                serde_json::Value::String(text) => text.clone(),
                _ => value.to_string(),
            };
            (key.clone(), rendered)
        })
        .collect()
}

fn should_emit_step_event(event: &DaemonEvent) -> bool {
    event.goal_room.is_some() || event.thread_id.is_some()
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    Intake {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
        #[arg(long)]
        message: String,
    },
    RunOnce {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
    },
    Serve {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
        #[arg(long)]
        matrix_incoming_file: Option<String>,
        #[arg(long)]
        matrix_outgoing_file: Option<String>,
        #[arg(long)]
        matrix_homeserver: Option<String>,
        #[arg(long)]
        matrix_user: Option<String>,
        #[arg(long)]
        matrix_sdk_data_dir: Option<String>,
        #[arg(long, default_value_t = false)]
        matrix_sdk: bool,
        #[arg(long)]
        matrix_user_id: Option<String>,
        #[arg(long)]
        matrix_since_file: Option<String>,
        #[arg(long)]
        matrix_session_file: Option<String>,
        #[arg(long, default_value_t = 30_000)]
        matrix_timeout_ms: u64,
        #[arg(long)]
        push_gateway_url: Option<String>,
        #[arg(long)]
        push_apns_gateway_url: Option<String>,
        #[arg(long)]
        push_fcm_gateway_url: Option<String>,
        #[arg(long)]
        push_telemetry_file: Option<String>,
        #[arg(long, default_value_t = 250)]
        sleep_ms: u64,
        #[arg(long)]
        max_iterations: Option<u64>,
        #[arg(long)]
        shutdown_file: Option<String>,
    },
    Status {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
    },
    Goals {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
        #[arg(long)]
        room: Option<String>,
    },
    Agents {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
        #[arg(long)]
        id: Option<String>,
    },
    IngestBatch {
        #[arg(long, default_value = "data/queue/jobs.state")]
        queue_file: String,
        /// Directory containing markdown articles to ingest
        #[arg(long)]
        dir: String,
        /// Optional meta.json for enrichment (relevance, key_insight, status)
        #[arg(long)]
        meta: Option<String>,
        /// Batch size for processing
        #[arg(long, default_value_t = 50)]
        batch_size: usize,
        /// Skip low-relevance unread articles
        #[arg(long, default_value_t = false)]
        skip_low: bool,
        /// State file for resumable progress
        #[arg(long, default_value = "data/batch-ingest-state.json")]
        state_file: String,
    },
}

#[cfg(unix)]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGTERM: i32 = 15;

#[cfg(unix)]
type SignalHandler = extern "C" fn(i32);

#[cfg(unix)]
extern "C" {
    fn signal(sig: i32, handler: SignalHandler) -> SignalHandler;
}

#[cfg(unix)]
extern "C" fn handle_shutdown_signal(_: i32) {
    SHOULD_STOP.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_shutdown_signal_handlers() {
    // SAFETY: Registering process signal handlers with a static C ABI function.
    unsafe {
        signal(SIGINT, handle_shutdown_signal);
        signal(SIGTERM, handle_shutdown_signal);
    }
}

#[cfg(not(unix))]
fn install_shutdown_signal_handlers() {}

/// Prints the daemon startup feature summary at INFO level.
/// Combines transport-level lines (only known in main.rs) with
/// daemon-internal lines from `SymbioticDaemon::feature_summary()`.
fn print_feature_summary(
    daemon: &symbiotic_daemon::SymbioticDaemon,
    matrix_connected: bool,
    matrix_homeserver: Option<&str>,
    matrix_allow_unencrypted: bool,
    reconciler_active: bool,
    tunnel_active: bool,
) {
    use symbiotic_daemon::FeatureLine;

    let mut lines: Vec<FeatureLine> = vec![
        FeatureLine {
            category: "Matrix",
            enabled: matrix_connected,
            detail: if matrix_connected {
                format!("connected ({})", matrix_homeserver.unwrap_or("unknown"))
            } else {
                "disabled (no transport)".into()
            },
        },
        FeatureLine {
            category: "E2EE",
            enabled: !matrix_allow_unencrypted,
            detail: if matrix_allow_unencrypted {
                "unencrypted allowed".into()
            } else {
                "required".into()
            },
        },
        FeatureLine {
            category: "Control Plane",
            enabled: reconciler_active,
            detail: if reconciler_active {
                "reconciler running".into()
            } else {
                "disabled (no archive path)".into()
            },
        },
        FeatureLine {
            category: "Tunnel",
            enabled: tunnel_active,
            detail: if tunnel_active {
                "relay tunnel active".into()
            } else {
                "disabled".into()
            },
        },
    ];

    lines.extend(daemon.feature_summary());

    info!("┌─────────────────────────────────────────────────────┐");
    info!("│             Symbiotic Nucleus — Features            │");
    info!("├─────────────────────────────────────────────────────┤");
    for line in &lines {
        info!("│{line}│");
    }
    info!("└─────────────────────────────────────────────────────┘");
}

#[cfg(unix)]
fn shutdown_requested() -> bool {
    SHOULD_STOP.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn shutdown_requested() -> bool {
    false
}

#[tokio::main]
async fn main() -> Result<()> {
    // Map SYMBIOTIC_LOG_LEVEL to RUST_LOG if RUST_LOG is not already set.
    if std::env::var("RUST_LOG").is_err() {
        if let Ok(level) = std::env::var("SYMBIOTIC_LOG_LEVEL") {
            std::env::set_var("RUST_LOG", &level);
        }
    }
    // Use tracing-subscriber to capture both `log` and `tracing` output.
    // matrix-sdk uses `tracing` internally, so this is needed to see SDK logs.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Intake {
            queue_file,
            message,
        } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;
            let reply = daemon.handle_intake_message(&message)?;
            println!("{}", reply.body);
        }
        Command::RunOnce { queue_file } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;
            let policy_events = daemon.process_declared_task_policy_tick(now_unix())?;
            if !policy_events.is_empty() {
                println!("policy_events={}", policy_events.len());
            }
            match daemon.run_once(now_unix())? {
                Some((event, step_events)) => {
                    for se in &step_events {
                        println!(
                            "step_event={} status={} detail={}",
                            se.event_type, se.status, se.detail
                        );
                    }
                    println!(
                        "event={} status={} job_id={} detail={}",
                        event.event_type,
                        event.status,
                        event.job_id.unwrap_or_default(),
                        event.detail
                    );
                }
                None => println!("no_jobs_available"),
            }
        }
        Command::Serve {
            queue_file,
            matrix_incoming_file,
            matrix_outgoing_file,
            matrix_homeserver,
            matrix_user,
            matrix_sdk_data_dir,
            matrix_sdk,
            matrix_user_id,
            matrix_since_file,
            matrix_session_file,
            matrix_timeout_ms,
            push_gateway_url,
            push_apns_gateway_url,
            push_fcm_gateway_url,
            push_telemetry_file,
            sleep_ms,
            max_iterations,
            shutdown_file,
        } => {
            install_shutdown_signal_handlers();
            let matrix_homeserver =
                matrix_homeserver.or_else(|| std::env::var("SYMBIOTIC_MATRIX_HOMESERVER").ok());
            let matrix_user = matrix_user.or_else(|| std::env::var("SYMBIOTIC_MATRIX_USER").ok());
            let env_room_roles = symbiotic_daemon::RoomRoleMap {
                control: std::env::var("SYMBIOTIC_MATRIX_ROOM_CONTROL").ok(),
                intake: std::env::var("SYMBIOTIC_MATRIX_ROOM_INTAKE").ok(),
                alerts: std::env::var("SYMBIOTIC_MATRIX_ROOM_ALERTS").ok(),
                status: std::env::var("SYMBIOTIC_MATRIX_ROOM_STATUS").ok(),
                credentials: std::env::var("SYMBIOTIC_MATRIX_ROOM_CREDENTIALS").ok(),
                goals: std::env::var("SYMBIOTIC_MATRIX_ROOM_GOALS").ok(),
                stream: std::env::var("SYMBIOTIC_MATRIX_ROOM_STREAM").ok(),
            };
            let matrix_allow_unencrypted = std::env::var("SYMBIOTIC_MATRIX_ALLOW_UNENCRYPTED")
                .ok()
                .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false);

            let push_gateway_url =
                push_gateway_url.or_else(|| std::env::var("SYMBIOTIC_PUSH_GATEWAY_URL").ok());
            let push_gateway_api_key = std::env::var("SYMBIOTIC_PUSH_GATEWAY_API_KEY").ok();
            let push_apns_gateway_url = push_apns_gateway_url
                .or_else(|| std::env::var("SYMBIOTIC_PUSH_APNS_GATEWAY_URL").ok());
            let push_apns_gateway_api_key =
                std::env::var("SYMBIOTIC_PUSH_APNS_GATEWAY_API_KEY").ok();
            let push_fcm_gateway_url = push_fcm_gateway_url
                .or_else(|| std::env::var("SYMBIOTIC_PUSH_FCM_GATEWAY_URL").ok());
            let push_fcm_gateway_api_key = std::env::var("SYMBIOTIC_PUSH_FCM_GATEWAY_API_KEY").ok();
            let push_telemetry_file =
                push_telemetry_file.or_else(|| std::env::var("SYMBIOTIC_PUSH_TELEMETRY_FILE").ok());

            // Real APNs gateway credentials (secrets — env-only, never CLI args).
            let push_apns_team_id = std::env::var("SYMBIOTIC_PUSH_APNS_TEAM_ID")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let push_apns_key_id = std::env::var("SYMBIOTIC_PUSH_APNS_KEY_ID")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let push_apns_private_key_pem = std::env::var("SYMBIOTIC_PUSH_APNS_PRIVATE_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let push_apns_sandbox = std::env::var("SYMBIOTIC_PUSH_APNS_SANDBOX")
                .ok()
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false);

            // Real FCM gateway credentials (secrets — env-only, never CLI args).
            let push_fcm_project_id = std::env::var("SYMBIOTIC_PUSH_FCM_PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty());
            let push_fcm_service_account_email =
                std::env::var("SYMBIOTIC_PUSH_FCM_SERVICE_ACCOUNT_EMAIL")
                    .ok()
                    .filter(|v| !v.trim().is_empty());
            let push_fcm_private_key_pem = std::env::var("SYMBIOTIC_PUSH_FCM_PRIVATE_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            let vps_provision_endpoint = std::env::var("SYMBIOTIC_CONTROL_PLANE_URL")
                .ok()
                .or_else(|| std::env::var("SYMBIOTIC_VPS_PROVISION_ENDPOINT").ok());
            let vps_provision_token = std::env::var("SYMBIOTIC_CONTROL_PLANE_TOKEN")
                .ok()
                .or_else(|| std::env::var("SYMBIOTIC_VPS_PROVISION_TOKEN").ok());
            let hcloud_token = std::env::var("SYMBIOTIC_HCLOUD_TOKEN").ok();
            let vps_region = std::env::var("SYMBIOTIC_VPS_REGION")
                .ok()
                .filter(|value| !value.trim().is_empty());
            let vps_size = std::env::var("SYMBIOTIC_VPS_SIZE")
                .ok()
                .filter(|value| !value.trim().is_empty());
            let vps_image = std::env::var("SYMBIOTIC_VPS_IMAGE")
                .ok()
                .filter(|value| !value.trim().is_empty());
            let vps_ssh_public_key = std::env::var("SYMBIOTIC_VPS_SSH_PUBLIC_KEY")
                .ok()
                .filter(|value| !value.trim().is_empty());

            // X API credentials (secrets — env-only, never CLI args).
            let x_client_id = std::env::var("SYMBIOTIC_X_CLIENT_ID").ok();
            let x_client_secret = std::env::var("SYMBIOTIC_X_CLIENT_SECRET").ok();

            // Bootstrap config.
            let allow_self_registration = std::env::var("SYMBIOTIC_BOOTSTRAP_SELF_REGISTER")
                .ok()
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false);
            let matrix_server_name = std::env::var("SYMBIOTIC_MATRIX_SERVER_NAME")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "symbiotic.local".to_string());
            // Keep a copy for DaemonConfig (bootstrap moves the original).
            let matrix_server_name_copy = matrix_server_name.clone();

            // Clear secret environment variables to prevent propagation to child processes.
            for key in [
                "SYMBIOTIC_PUSH_GATEWAY_API_KEY",
                "SYMBIOTIC_PUSH_APNS_GATEWAY_API_KEY",
                "SYMBIOTIC_PUSH_FCM_GATEWAY_API_KEY",
                "SYMBIOTIC_PUSH_APNS_PRIVATE_KEY",
                "SYMBIOTIC_PUSH_FCM_PRIVATE_KEY",
                "SYMBIOTIC_X_CLIENT_ID",
                "SYMBIOTIC_X_CLIENT_SECRET",
                "SYMBIOTIC_CONTROL_PLANE_TOKEN",
                "SYMBIOTIC_VPS_PROVISION_TOKEN",
                "SYMBIOTIC_HCLOUD_TOKEN",
                "SYMBIOTIC_VPS_SSH_PUBLIC_KEY",
                "ANTHROPIC_API_KEY",
                "OPENAI_API_KEY",
            ] {
                std::env::remove_var(key);
            }

            // --- Bootstrap: resolve credentials and rooms ---
            // Bootstrap is the sole credential and room resolution path.
            // Credentials come from Docker secrets, encrypted vault, or self-registration.
            // Plaintext env var overrides are not supported (insecure).
            let mut matrix_password: Option<String> = None;
            let mut matrix_access_token: Option<String> = None;
            let mut bootstrap_room_roles = env_room_roles;

            if matrix_incoming_file.is_none()
                && matrix_outgoing_file.is_none()
                && matrix_homeserver.is_some()
            {
                let vault_path = DaemonConfig::default().credential_vault_file;
                let vault: Arc<dyn credential_gateway::CredentialVault> =
                    Arc::new(FileCredentialVault::open(&vault_path)?);
                let boot_config = BootstrapConfig {
                    homeserver: matrix_homeserver.clone().unwrap(),
                    username: matrix_user
                        .clone()
                        .unwrap_or_else(|| "symbiotic-daemon".to_string()),
                    server_name: matrix_server_name,
                    allow_self_registration,
                };
                let result = bootstrap::bootstrap(&boot_config, &vault)
                    .await
                    .context("bootstrap failed")?;

                matrix_password = result.password;
                // Capture the bootstrap access token for dynamic room creation
                // (thread rooms). MatrixSdkTransport handles its own login
                // independently for E2EE sync.
                matrix_access_token = result.access_token;
                bootstrap_room_roles.control = result.rooms.get("control").cloned();
                bootstrap_room_roles.intake = result.rooms.get("intake").cloned();
                bootstrap_room_roles.alerts = result.rooms.get("alerts").cloned();
                bootstrap_room_roles.status = result.rooms.get("status").cloned();
                bootstrap_room_roles.credentials = result.rooms.get("credentials").cloned();
                bootstrap_room_roles.goals = result.rooms.get("goals").cloned();
                bootstrap_room_roles.stream = result.rooms.get("stream").cloned();

                if result.self_registered {
                    info!("bootstrap: self-registered, zero-config mode active");
                }
            }

            // Determine SDK mode after bootstrap resolved credentials.
            // When homeserver + user + password are available, always prefer SDK
            // mode (handles E2EE). Bootstrap may also return an access_token for
            // HTTP API calls (room creation) — that's separate from transport.
            let matrix_sdk_mode = matrix_sdk
                || (matrix_homeserver.is_some()
                    && matrix_user.is_some()
                    && matrix_password.is_some());

            let mut daemon_config = DaemonConfig {
                queue_file: queue_file.into(),
                ..DaemonConfig::default()
            };
            if let Some(value) = push_gateway_url {
                daemon_config.push_gateway_url = Some(value);
            }
            if let Some(value) = push_gateway_api_key {
                daemon_config.push_gateway_api_key = Some(value);
            }
            if let Some(value) = push_apns_gateway_url {
                daemon_config.push_apns_gateway_url = Some(value);
            }
            if let Some(value) = push_apns_gateway_api_key {
                daemon_config.push_apns_gateway_api_key = Some(value);
            }
            if let Some(value) = push_fcm_gateway_url {
                daemon_config.push_fcm_gateway_url = Some(value);
            }
            if let Some(value) = push_fcm_gateway_api_key {
                daemon_config.push_fcm_gateway_api_key = Some(value);
            }
            // Real APNs/FCM gateway credentials.
            daemon_config.push_apns_team_id = push_apns_team_id;
            daemon_config.push_apns_key_id = push_apns_key_id;
            daemon_config.push_apns_private_key_pem = push_apns_private_key_pem;
            daemon_config.push_apns_sandbox = push_apns_sandbox;
            daemon_config.push_fcm_project_id = push_fcm_project_id;
            daemon_config.push_fcm_service_account_email = push_fcm_service_account_email;
            daemon_config.push_fcm_private_key_pem = push_fcm_private_key_pem;
            if let Some(value) = push_telemetry_file {
                daemon_config.push_telemetry_file = value.into();
            }
            if x_client_id.is_some() {
                daemon_config.x_client_id = x_client_id;
            }
            if x_client_secret.is_some() {
                daemon_config.x_client_secret = x_client_secret;
            }
            if let Some(value) = vps_provision_endpoint {
                daemon_config.vps_provision_endpoint = Some(value);
            }
            if let Some(value) = vps_provision_token {
                daemon_config.vps_provision_token = Some(value);
            }
            if let Some(value) = hcloud_token {
                daemon_config.hcloud_token = Some(value);
            }
            if let Some(value) = vps_region {
                daemon_config.vps_region = value;
            }
            if let Some(value) = vps_size {
                daemon_config.vps_size = value;
            }
            if let Some(value) = vps_image {
                daemon_config.vps_image = value;
            }
            if let Some(value) = vps_ssh_public_key {
                daemon_config.vps_ssh_public_key = Some(value);
            }

            // Archive path for the declarative control-plane reconciler.
            daemon_config.archive_path = std::env::var("SYMBIOTIC_ARCHIVE_PATH")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from);

            // Explicit SOUL.md path (overrides archive-derived and home-directory defaults).
            daemon_config.soul_file = std::env::var("SYMBIOTIC_SOUL_FILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from);

            // Embedding retry interval (seconds). 0 disables periodic retry.
            if let Ok(val) = std::env::var("SYMBIOTIC_EMBEDDING_RETRY_INTERVAL_SECS") {
                if let Ok(secs) = val.trim().parse::<u64>() {
                    daemon_config.embedding_retry_interval_secs = secs;
                }
            }
            if let Ok(val) = std::env::var("SYMBIOTIC_RECALL_PROBE_INTERVAL_SECS") {
                if let Ok(secs) = val.trim().parse::<u64>() {
                    daemon_config.recall_probe_interval_secs = secs;
                }
            }
            if let Ok(val) = std::env::var("SYMBIOTIC_RECALL_PROBE_TOP_K") {
                if let Ok(top_k) = val.trim().parse::<usize>() {
                    daemon_config.recall_probe_top_k = top_k;
                }
            }
            if let Ok(val) = std::env::var("SYMBIOTIC_RECALL_PROBE_MAX_SUBJECTS") {
                if let Ok(max_subjects) = val.trim().parse::<usize>() {
                    daemon_config.recall_probe_max_subjects_per_run = max_subjects;
                }
            }
            if let Ok(val) = std::env::var("SYMBIOTIC_RECALL_PROBE_MAX_QUERIES_PER_SUBJECT") {
                if let Ok(max_queries) = val.trim().parse::<usize>() {
                    daemon_config.recall_probe_max_queries_per_subject = max_queries;
                }
            }

            // Snapshot interval (seconds). 0 disables periodic snapshots.
            let snapshot_interval_secs: u64 = std::env::var("SYMBIOTIC_SNAPSHOT_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(300);

            daemon_config.room_roles = bootstrap_room_roles;
            info!(
                "room_roles: control={:?} intake={:?} alerts={:?} status={:?}",
                daemon_config.room_roles.control,
                daemon_config.room_roles.intake,
                daemon_config.room_roles.alerts,
                daemon_config.room_roles.status,
            );

            // Pass Matrix credentials so ThreadManager can create thread rooms.
            daemon_config.matrix_homeserver = matrix_homeserver.clone();
            daemon_config.matrix_server_name = Some(matrix_server_name_copy);
            daemon_config.matrix_access_token = matrix_access_token.clone();

            // F2: Read allowed senders from env (comma-separated list of Matrix user IDs)
            if let Ok(senders) = std::env::var("SYMBIOTIC_ALLOWED_SENDERS") {
                daemon_config.allowed_senders = senders
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
            daemon_config.allow_open_access = std::env::var("SYMBIOTIC_ALLOW_OPEN_ACCESS")
                .ok()
                .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
                .unwrap_or(false);
            info!(
                "allowed_senders: {:?} (count={}, allow_open_access={})",
                daemon_config.allowed_senders,
                daemon_config.allowed_senders.len(),
                daemon_config.allow_open_access,
            );
            if daemon_config.allowed_senders.is_empty() && daemon_config.allow_open_access {
                warn!(
                    "sender authorization open-access mode enabled; this is intended for local development only"
                );
            }

            // Ollama chat model for agent completions (default: qwen3.5).
            daemon_config.ollama_chat_model = std::env::var("SYMBIOTIC_OLLAMA_CHAT_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty());
            daemon_config.model_manifest_file = std::env::var("SYMBIOTIC_MODEL_MANIFEST_FILE")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from("config/model-manifest.toml"));

            // Anthropic API key for cloud completions via Claude models.
            daemon_config.anthropic_api_key = std::env::var("ANTHROPIC_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Claude Code CLI binary for subscription-based completions.
            // When set, uses user's Pro/Max plan instead of separate API key.
            daemon_config.claude_code_binary = std::env::var("SYMBIOTIC_CLAUDE_CODE_BINARY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Claude Code model override (e.g. "sonnet", "opus").
            daemon_config.claude_code_model = std::env::var("SYMBIOTIC_CLAUDE_CODE_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Codex CLI binary for OpenAI subscription-based completions.
            // Apache 2.0 licensed — safe for daemon/server use.
            daemon_config.codex_binary = std::env::var("SYMBIOTIC_CODEX_BINARY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Codex CLI model override (e.g. "o3", "gpt-4.1").
            daemon_config.codex_model = std::env::var("SYMBIOTIC_CODEX_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Gemini API key for Google AI completions (free tier or paid).
            daemon_config.gemini_api_key = std::env::var("GEMINI_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Gemini model override (default: gemini-2.5-flash).
            daemon_config.gemini_model = std::env::var("SYMBIOTIC_GEMINI_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // LLM Gateway Unix socket path (decoupled agent bridge).
            daemon_config.llm_gateway_socket = std::env::var("SYMBIOTIC_LLM_GATEWAY_SOCKET")
                .ok()
                .filter(|v| !v.trim().is_empty());
            daemon_config.llm_gateway_world_accessible =
                std::env::var("SYMBIOTIC_LLM_GATEWAY_WORLD_ACCESSIBLE")
                    .ok()
                    .map(|value| {
                        matches!(
                            value.trim().to_ascii_lowercase().as_str(),
                            "1" | "true" | "yes" | "on"
                        )
                    })
                    .unwrap_or(false);

            // OpenRouter API key for aggregated model access.
            daemon_config.openrouter_api_key = std::env::var("OPENROUTER_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // OpenRouter model override (default: anthropic/claude-sonnet-4).
            daemon_config.openrouter_model = std::env::var("SYMBIOTIC_OPENROUTER_MODEL")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Explicit default provider override (bypasses auto-detection priority).
            daemon_config.default_provider = std::env::var("SYMBIOTIC_DEFAULT_PROVIDER")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Agent execution backend: react (default), cli, or runner.
            if let Ok(val) = std::env::var("SYMBIOTIC_AGENT_BACKEND") {
                match val.trim().to_lowercase().as_str() {
                    "cli" => daemon_config.agent_backend = symbiotic_daemon::AgentBackend::Cli,
                    "runner" => {
                        daemon_config.agent_backend = symbiotic_daemon::AgentBackend::Runner
                    }
                    "react" | "" => {
                        daemon_config.agent_backend = symbiotic_daemon::AgentBackend::React
                    }
                    other => {
                        warn!("unknown SYMBIOTIC_AGENT_BACKEND={other}, defaulting to react");
                    }
                }
            }

            // Ollama URL for embeddings and completions.
            daemon_config.ollama_url = std::env::var("SYMBIOTIC_OLLAMA_URL")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // OpenAI API key for cloud embeddings and completions.
            daemon_config.openai_api_key = std::env::var("OPENAI_API_KEY")
                .ok()
                .filter(|v| !v.trim().is_empty());

            // Auth sandbox: directory containing deterministic login scripts.
            daemon_config.auth_scripts_dir = std::env::var("SYMBIOTIC_AUTH_SCRIPTS_DIR")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from);
            daemon_config.auth_sandbox_bin = std::env::var("SYMBIOTIC_AUTH_SANDBOX_BIN")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from);
            daemon_config.node_bin = std::env::var("SYMBIOTIC_NODE_BIN")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .map(std::path::PathBuf::from);
            if let Ok(val) = std::env::var("SYMBIOTIC_AUTH_APPROVAL_TTL_SECS") {
                if let Ok(secs) = val.trim().parse::<u64>() {
                    daemon_config.auth_approval_ttl_secs = secs;
                }
            }
            if let Ok(val) = std::env::var("SYMBIOTIC_AUTH_INPUT_TTL_SECS") {
                if let Ok(secs) = val.trim().parse::<u64>() {
                    daemon_config.auth_input_ttl_secs = secs;
                }
            }

            // Capture values before config is moved into daemon.
            let archive_path = daemon_config.archive_path.clone();
            let embedding_retry_interval_secs = daemon_config.embedding_retry_interval_secs;

            let (mut daemon, mut matrix_outbound_rx, mut conflict_goal_rx) =
                SymbioticDaemon::open(daemon_config)?;

            // T128 §16a — second matrix outbound channel for posters that
            // need the server-assigned event_id (archeology threaded
            // notifications in §16b, future T42/T126 reply chains).
            // Distinct from the legacy `matrix_outbound_rx` tuple channel
            // because the response oneshot is what makes capture possible
            // — adding it to the existing tuple would have forced a
            // breaking change on every fire-and-forget call site.
            //
            // The sender is wired into `DaemonMatrixPoster::with_awaited`
            // by callers that need it (no §16a-internal call sites yet —
            // the §16b archeology notifier will clone `_awaited_matrix_tx`
            // when it lands). The pump loop below drains the receiver and
            // ships back `Result<EventId>` through each item's `response`
            // oneshot.
            let (_awaited_matrix_tx, mut awaited_matrix_rx) = tokio::sync::mpsc::unbounded_channel::<
                symbiotic_daemon::matrix_poster::OutboundMatrixMessage,
            >();

            let swarm_server = match symbiotic_daemon::swarm_server::SwarmServer::new(
                daemon.capability_broker(),
                daemon.management_store(),
                daemon.sandbox_manager(),
                daemon.config().llm_gateway_socket.clone(),
                daemon.config().archive_root.clone(),
                daemon.config().data_dir.clone(),
                Some(daemon.repo_registry()),
            ) {
                Ok(server) => {
                    let server = Arc::new(server);
                    match server.start().await {
                        Ok(()) => Some(server),
                        Err(e) => {
                            warn!("swarm: failed to start git server container: {e}");
                            None
                        }
                    }
                }
                Err(e) => {
                    warn!("swarm: failed to initialize: {e}");
                    None
                }
            };

            // --- LLM Gateway (agent-runner bridge) ---
            if let Some(socket_path) = daemon.config().llm_gateway_socket.clone() {
                let llm_audit_path = daemon.config().data_dir.join("runtime/llm-audit.db");
                let tool_memory_path = daemon.config().data_dir.join("runtime/tool-memory.db");
                let audit_log = std::sync::Arc::new(std::sync::Mutex::new(
                    match symbiotic_daemon::llm_audit::LlmAuditLog::open(
                        &llm_audit_path,
                        symbiotic_daemon::llm_audit::LlmAuditLevel::MetadataOnly,
                        30,
                    ) {
                        Ok(log) => log,
                        Err(error) => {
                            warn!(
                                "llm_audit: failed to open durable store at {}: {}; falling back to in-memory log",
                                llm_audit_path.display(),
                                error
                            );
                            symbiotic_daemon::llm_audit::LlmAuditLog::new(
                                symbiotic_daemon::llm_audit::LlmAuditLevel::MetadataOnly,
                                30,
                            )
                        }
                    },
                ));
                let tool_memory = std::sync::Arc::new(std::sync::Mutex::new(
                    match symbiotic_memory::tool_memory::ToolMemoryStore::open(&tool_memory_path) {
                        Ok(store) => store,
                        Err(error) => {
                            warn!(
                                "tool_memory: failed to open durable store at {}: {}; falling back to in-memory log",
                                tool_memory_path.display(),
                                error
                            );
                            symbiotic_memory::tool_memory::ToolMemoryStore::new()
                        }
                    },
                ));
                if let Some(ollama_url) = daemon.config().ollama_url.as_deref() {
                    let models = symbiotic_daemon::model_integrity::requested_ollama_models(
                        daemon.config().ollama_chat_model.as_deref(),
                    );
                    let verifications = symbiotic_daemon::model_integrity::verify_ollama_models(
                        ollama_url,
                        &models,
                        &daemon.config().model_manifest_file,
                    )
                    .await;
                    let now = symbiotic_daemon::llm_audit::now_epoch_secs();
                    if let Ok(mut log) = audit_log.lock() {
                        for verification in verifications {
                            log.record_model_verification(verification, now);
                        }
                    } else {
                        warn!("llm_audit: failed to acquire audit log lock for model verification");
                    }
                }
                let gateway = symbiotic_daemon::llm_gateway::LlmGateway::new(
                    daemon.provider_router().clone(),
                    daemon.archive_store(),
                    daemon.queue(),
                    daemon.recall_gateway(),
                    swarm_server.clone(),
                    daemon.capability_broker(),
                    daemon.credential_gateway(),
                    daemon.credential_vault(),
                    daemon.auth_engine(),
                    daemon.auth_job_store(),
                    daemon.auth_approval_policy_store(),
                    daemon.bridge_session_store(),
                    daemon.bridge_interaction_log_store(),
                    daemon.agent_runtime_log_store(),
                    daemon.agent_runtime_status_store(),
                    daemon.bridge_checkpoint_store(),
                    audit_log,
                    tool_memory,
                    daemon
                        .config()
                        .room_roles
                        .resolve(symbiotic_daemon::RoomRole::Credentials),
                    symbiotic_daemon::auth_jobs::AuthJobConfig {
                        approval_ttl_secs: daemon.config().auth_approval_ttl_secs,
                        input_ttl_secs: daemon.config().auth_input_ttl_secs,
                    },
                    &socket_path,
                    daemon.config().llm_gateway_world_accessible,
                );
                tokio::spawn(async move {
                    if let Err(e) = gateway.run().await {
                        tracing::error!(error = %e, "LLM Gateway failed");
                    }
                });
            }

            // --- Control-plane reconciler with ActionDispatcher (optional, behind archive_path) ---
            // Build the trait-based ActionDispatcher with concrete daemon ops
            // (DaemonGoalOps, DaemonAgentOps, DaemonSkillOps, DaemonIdentityOps).
            // The dispatcher is Send+Sync and routes reconciliation actions to
            // the appropriate daemon subsystems via the ops traits.
            let _reconciler_handle: Option<symbiotic_daemon::control_plane::ReconcilerHandle> =
                if let Some(ref kb_path) = archive_path {
                    info!(
                        "control_plane: starting reconciler with ActionDispatcher, archive_path={}",
                        kb_path.display()
                    );

                    // Create a DaemonStateQuery to get the ActualState handle for
                    // ops types that need to update the reconciler's world view.
                    let state_query = symbiotic_daemon::control_plane::DaemonStateQuery::new();
                    let actual_state = state_query.state_handle();

                    // Build the ActionDispatcher from the daemon's internal state.
                    let dispatcher = daemon.build_action_dispatcher(Arc::clone(&actual_state));

                    let handle = symbiotic_daemon::control_plane::spawn_reconciler_with_dispatcher(
                        kb_path.clone(),
                        daemon.identity_content(),
                        dispatcher,
                    );

                    Some(handle)
                } else {
                    info!("control_plane: reconciler disabled (SYMBIOTIC_ARCHIVE_PATH not set)");
                    None
                };

            // --- Relay tunnel client (optional, managed mode) ---
            // Spawned as an independent background task. The tunnel client
            // uses only Send+Sync types (reqwest + tokio-tungstenite), so
            // tokio::spawn is safe here even though SymbioticDaemon is !Send.
            let _tunnel_handle =
                if let Some(tunnel_config) = symbiotic_daemon::tunnel::TunnelConfig::from_env() {
                    info!(
                        "tunnel: starting relay tunnel client (url={})",
                        tunnel_config.tunnel_url
                    );
                    let client = symbiotic_daemon::tunnel::TunnelClient::new(tunnel_config);
                    Some(tokio::spawn(async move { client.run().await }))
                } else {
                    info!("tunnel: relay tunnel disabled (SYMBIOTIC_RELAY_TUNNEL_URL not set)");
                    None
                };

            // Clear relay/tunnel secrets now that configs have been read.
            std::env::remove_var("SYMBIOTIC_RELAY_DAEMON_TOKEN");

            let matrix_transport: Option<Box<dyn MatrixTransport>> = match (
                matrix_incoming_file.as_deref(),
                matrix_outgoing_file.as_deref(),
                matrix_homeserver.as_deref(),
                matrix_access_token.as_deref(),
                matrix_user.as_deref(),
                matrix_password.as_deref(),
                matrix_sdk_mode,
            ) {
                (Some(incoming), Some(outgoing), None, None, None, None, false) => {
                    Some(Box::new(FileMatrixTransport::open(incoming, outgoing)?))
                }
                (None, None, Some(homeserver), Some(access_token), None, None, false) => {
                    if !CurlMatrixHttpClient::is_available() {
                        anyhow::bail!("live matrix transport requires curl to be available");
                    }
                    let live = LiveMatrixTransport::open(LiveMatrixConfig {
                        homeserver_url: homeserver.to_string(),
                        access_token: access_token.to_string(),
                        self_user_id: matrix_user_id.clone(),
                        sync_timeout_ms: matrix_timeout_ms,
                        since_file: matrix_since_file.as_ref().map(Into::into),
                    })?;
                    Some(Box::new(live))
                }
                (None, None, Some(homeserver), _, Some(user), Some(password), true) => {
                    let data_dir = matrix_sdk_data_dir
                        .as_ref()
                        .map(Into::into)
                        .unwrap_or_else(|| "data/matrix-sdk".into());
                    let device_display_name = std::env::var("SYMBIOTIC_DEVICE_DISPLAY_NAME").ok();
                    let stale_device_cleanup_days: u64 =
                        std::env::var("SYMBIOTIC_STALE_DEVICE_CLEANUP_DAYS")
                            .ok()
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(7);
                    let transport = MatrixSdkTransport::open(MatrixSdkConfig {
                        homeserver_url: homeserver.to_string(),
                        user_id: user.to_string(),
                        password: password.to_string(),
                        sync_timeout_ms: matrix_timeout_ms,
                        data_dir,
                        session_file: matrix_session_file.as_ref().map(Into::into),
                        self_user_id: matrix_user_id.clone(),
                        require_e2ee: !matrix_allow_unencrypted,
                        stale_device_cleanup_days,
                        device_display_name,
                    })
                    .await?;
                    Some(Box::new(transport))
                }
                (None, None, None, None, None, None, false) => None,
                _ => {
                    anyhow::bail!(
                        "matrix transport must be either file mode (--matrix-incoming-file + --matrix-outgoing-file) \
or live HTTP mode (--matrix-homeserver + --matrix-access-token) \
or matrix-sdk mode (--matrix-sdk --matrix-homeserver --matrix-user --matrix-password)"
                    );
                }
            };

            // Print startup feature summary
            print_feature_summary(
                &daemon,
                matrix_transport.is_some(),
                matrix_homeserver.as_deref(),
                matrix_allow_unencrypted,
                _reconciler_handle.is_some(),
                _tunnel_handle.is_some(),
            );

            // --- HTTP API server (archive sync endpoint) ---
            // Spawned as an independent background task. The HTTP server uses only
            // Send+Sync types (Arc<FileArchiveStore> + reqwest), so tokio::spawn
            // is safe here even though SymbioticDaemon is !Send.
            let http_port: u16 = std::env::var("SYMBIOTIC_HTTP_PORT")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(8090);
            let http_homeserver = matrix_homeserver
                .clone()
                .unwrap_or_else(|| "http://conduwuit:8008".to_string());
            let http_state = symbiotic_daemon::http_api::HttpApiState {
                archive_store: daemon.archive_store(),
                memory_store: daemon.memory_store(),
                probe_store_path: daemon.config().data_dir.join("runtime/recall-probes.db"),
                homeserver_url: http_homeserver,
                kb_root: archive_path.clone(),
                swarm: swarm_server.clone(),
            };
            let _http_handle = tokio::spawn(async move {
                if let Err(e) = symbiotic_daemon::http_api::serve(http_state, http_port).await {
                    warn!("http_api: server exited with error: {e}");
                }
            });

            // --- Brain Bootstrap greeting (sent once, first run only) ---
            if let Some(transport) = matrix_transport.as_deref() {
                let now = now_unix();
                if let Some(greeting) = daemon.maybe_build_greeting(now) {
                    if let Err(e) = daemon
                        .send_matrix_event(transport, &greeting.room_id, greeting.envelope, now)
                        .await
                    {
                        warn!("brain_bootstrap: greeting send failed: {e}");
                    }
                }
            }

            // --- Repo mirror scheduler (T126 §08.c) ---
            // One tokio task per active repo drives the mirror loop (pull /
            // push-with-approval / conflict-goal) on the manifest's
            // `mirror.sync_interval_secs` cadence. Cancellation flows via a
            // `tokio::sync::watch<bool>` that we flip to `true` on shutdown.
            let (scheduler_cancel_tx, scheduler_cancel_rx) =
                tokio::sync::watch::channel::<bool>(false);
            let scheduler_handles = daemon.spawn_repo_scheduler(scheduler_cancel_rx).await;
            info!(
                "repo_scheduler: spawned {} per-repo task(s)",
                scheduler_handles.len()
            );

            let mut iterations = 0u64;
            let mut draining = false;
            let start_time = Instant::now();
            let snapshot_interval = Duration::from_secs(snapshot_interval_secs);
            let snapshot_enabled = snapshot_interval > Duration::ZERO;
            let mut last_snapshot = Instant::now();
            let embedding_retry_interval = Duration::from_secs(embedding_retry_interval_secs);
            let embedding_retry_enabled = embedding_retry_interval > Duration::ZERO;
            let mut last_embedding_retry = Instant::now();
            let recall_probe_interval =
                Duration::from_secs(daemon.config().recall_probe_interval_secs);
            let recall_probe_enabled = recall_probe_interval > Duration::ZERO;
            let mut last_recall_probe = Instant::now();
            // Thread auto-archive: archive threads idle for >30 days (~1 hour cadence).
            let auto_archive_interval = Duration::from_secs(3600);
            let mut last_auto_archive = Instant::now();
            // Entity dedup: merge duplicate entities (~24 hour cadence, idempotent).
            let entity_dedup_interval = Duration::from_secs(24 * 3600);
            let mut last_entity_dedup = Instant::now();
            // Friction detection: scan for structural issues (~24 hour cadence).
            let friction_detection_interval = Duration::from_secs(24 * 3600);
            let mut last_friction_detection = Instant::now();
            loop {
                if shutdown_requested() && !draining {
                    draining = true;
                    info!("shutdown_signal_received draining=true");
                }
                if let Some(path) = shutdown_file.as_deref() {
                    if Path::new(path).exists() && !draining {
                        draining = true;
                        info!("shutdown_file_detected path={path} draining=true");
                    }
                }
                let now = now_unix();
                let pumped = if let Some(transport) = matrix_transport.as_deref() {
                    daemon.pump_transport_once(transport, now).await?
                } else {
                    0
                };
                if pumped > 0 {
                    debug!("matrix_messages_processed={pumped}");
                }

                // Drain the matrix outbound channel (T126 §08.b). Background daemon
                // tasks (scheduler, future workers) enqueue (room_id, envelope)
                // items via `DaemonMatrixPoster`; we forward them to the transport
                // that this loop owns. `try_recv` is non-blocking so we never stall
                // the pump loop waiting for outbound work. Individual send failures
                // are logged but do not break the drain — a missing message should
                // not poison future deliveries.
                if let Some(transport) = matrix_transport.as_deref() {
                    loop {
                        match matrix_outbound_rx.try_recv() {
                            Ok((room_id, envelope)) => {
                                if let Err(e) = transport.send_outgoing(&room_id, envelope).await {
                                    warn!("matrix outbound drain: send_outgoing failed: {e}");
                                }
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                // All senders dropped — daemon is tearing down.
                                // Nothing more will arrive; exit the drain loop.
                                break;
                            }
                        }
                    }
                }

                // T128 §16a — drain the awaited matrix outbound channel.
                // Same shape as the legacy tuple drain above, but each
                // item also carries an optional `oneshot::Sender` that
                // wants the resulting `Result<EventId>` shipped back so
                // the awaiting caller (e.g. archeology threaded
                // notifications, §16b) can chain further replies under
                // the captured event_id.
                //
                // `MatrixTransport::send_outgoing` currently returns
                // `Result<()>` — it does not surface the server-assigned
                // event_id. Until that changes, the pump synthesizes a
                // best-effort placeholder id from `(room_id, ts)` so the
                // round-trip plumbing exists and tests can verify it.
                // §16b's threaded reply chain treats this as opaque.
                if let Some(transport) = matrix_transport.as_deref() {
                    loop {
                        match awaited_matrix_rx.try_recv() {
                            Ok(item) => {
                                let symbiotic_daemon::matrix_poster::OutboundMatrixMessage {
                                    room_id,
                                    envelope,
                                    response,
                                } = item;
                                let ts = envelope.sym.ts;
                                let send_result = transport.send_outgoing(&room_id, envelope).await;
                                let outcome: Result<symbiotic_daemon::matrix_poster::EventId> =
                                    match send_result {
                                        Ok(()) => Ok(format!("$pending-{room_id}-{ts}")),
                                        Err(e) => {
                                            warn!(
                                                "awaited matrix drain: send_outgoing failed: {e}"
                                            );
                                            Err(e)
                                        }
                                    };
                                if let Some(tx) = response {
                                    // Ignore drop — caller may have given up.
                                    let _ = tx.send(outcome);
                                }
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                // All senders dropped — daemon is tearing down.
                                break;
                            }
                        }
                    }
                }

                // Drain the conflict-goal channel (T126 §08.c). The repo scheduler
                // enqueues `ConflictGoalRequest`s when it detects mirror divergence;
                // the daemon is `!Send` so the scheduler can't call
                // `process_goal_through_pipeline` directly. Dispatch happens here
                // on the main pump-loop thread.
                loop {
                    match conflict_goal_rx.try_recv() {
                        Ok(req) => {
                            if let Err(e) = daemon.handle_conflict_goal(&req, now) {
                                warn!(
                                    "repo_scheduler: conflict_goal dispatch failed for {}: {e}",
                                    req.description
                                );
                            }
                        }
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
                    }
                }

                // Process any pending thread room creation requests.
                if let Some(transport) = matrix_transport.as_deref() {
                    match daemon.process_pending_room_creations(transport, now).await {
                        Ok(events) => {
                            for routed in events {
                                if let Err(e) = daemon
                                    .send_matrix_event(
                                        transport,
                                        &routed.room_id,
                                        routed.envelope,
                                        now,
                                    )
                                    .await
                                {
                                    warn!("thread_room_event_failed: {e}");
                                }
                            }
                        }
                        Err(e) => warn!("process_pending_room_creations_failed: {e}"),
                    }
                }

                if let Some(transport) = matrix_transport.as_deref() {
                    match daemon.process_declared_task_policy_tick(now) {
                        Ok(events) => {
                            for routed in events {
                                if let Err(e) = daemon
                                    .send_matrix_event(
                                        transport,
                                        &routed.room_id,
                                        routed.envelope,
                                        now,
                                    )
                                    .await
                                {
                                    warn!("declared_task_policy_event_failed: {e}");
                                }
                            }
                        }
                        Err(e) => warn!("declared_task_policy_tick_failed: {e}"),
                    }
                }

                if let Some((event, step_events)) = daemon.run_once(now)? {
                    // Emit step-level progress events to the goal room first.
                    // Skip internal events (no goal_room) — e.g. Process Engineer.
                    if let Some(transport) = matrix_transport.as_deref() {
                        for se in step_events.iter().filter(|se| should_emit_step_event(se)) {
                            let se_envelope = se.to_envelope(now);
                            // Route step events to the goal's thread room when available,
                            // fall back to goal_room, then #goals for system-level events.
                            let target_room = se
                                .thread_id
                                .as_deref()
                                .and_then(|tid| daemon.resolve_thread_room(tid))
                                .or_else(|| se.goal_room.as_deref().map(|s| s.to_string()))
                                .unwrap_or_else(|| {
                                    daemon.resolve_room(RoomRole::Goals).to_string()
                                });
                            if let Err(e) = daemon
                                .send_matrix_event(transport, &target_room, se_envelope, now)
                                .await
                            {
                                warn!("step_event_emission_failed: {e}");
                            }
                        }
                    }
                    let job_id_for_print = event.job_id.clone().unwrap_or_default();
                    info!(
                        "event={} status={} job_id={} detail={}",
                        event.event_type, event.status, job_id_for_print, event.detail
                    );
                    if let Some(transport) = matrix_transport.as_deref() {
                        // Emit pipeline progress events to #intake when an
                        // intake_run_id is present (correlates with intake.started).
                        if let Some(ref _run_id) = event.intake_run_id {
                            if matches!(
                                event.event_type,
                                EventType::IngestFetch
                                    | EventType::ArchiveReviewEnqueue
                                    | EventType::ArchiveReview
                            ) {
                                let intake_status = if event.status == "completed" {
                                    Status::Success
                                } else {
                                    Status::Fail
                                };
                                let body = format!("{} {}", event.event_type, event.status);
                                let mut envelope = MatrixEventEnvelope::new(
                                    Kind::Message,
                                    intake_status,
                                    now,
                                    &body,
                                )
                                .with_detail_field("job_id", &*job_id_for_print)
                                .with_detail_field("detail", &*event.detail);
                                if let Some(ref url) = event.url {
                                    envelope = envelope.with_detail_field("url", url.as_str());
                                }
                                if let Some(ref title) = event.title {
                                    envelope = envelope.with_detail_field("title", title.as_str());
                                }
                                if let Some(ref sensitivity) = event.sensitivity {
                                    envelope = envelope.with_sensitivity(sensitivity);
                                }
                                if let Err(e) = daemon
                                    .send_matrix_event(
                                        transport,
                                        daemon.resolve_room(RoomRole::Intake),
                                        envelope,
                                        now,
                                    )
                                    .await
                                {
                                    warn!("intake_event_emission_failed: {e}");
                                }
                            }
                        }
                        if matches!(
                            event.event_type,
                            EventType::ArchiveReviewEnqueue | EventType::ArchiveReview
                        ) {
                            let (review_action, review_status_val) = match event.event_type {
                                EventType::ArchiveReviewEnqueue => {
                                    if event.status == "completed" {
                                        ("review.queued", Status::Working)
                                    } else {
                                        ("review.failed", Status::Fail)
                                    }
                                }
                                EventType::ArchiveReview => {
                                    if event.status == "completed" {
                                        ("review.completed", Status::Success)
                                    } else {
                                        ("review.failed", Status::Fail)
                                    }
                                }
                                _ => ("review.failed", Status::Fail),
                            };
                            let body = match review_action {
                                "review.completed" => "Archive review completed".to_string(),
                                "review.queued" => "Archive review queued".to_string(),
                                _ => "Archive review failed".to_string(),
                            };
                            let mut envelope = MatrixEventEnvelope::new(
                                Kind::Message,
                                review_status_val,
                                now,
                                &body,
                            )
                            .with_detail_field("worker_event", event.event_type.as_str())
                            .with_detail_field("worker_status", &*event.status)
                            .with_detail_field("job_id", &*job_id_for_print)
                            .with_detail_field("detail", &*event.detail);
                            if let Some(ref url) = event.url {
                                envelope = envelope.with_detail_field("url", url.as_str());
                            }
                            if let Some(ref title) = event.title {
                                envelope = envelope.with_detail_field("title", title.as_str());
                            }
                            daemon
                                .send_matrix_event(
                                    transport,
                                    daemon.resolve_room(RoomRole::Status),
                                    envelope,
                                    now,
                                )
                                .await?;
                        }
                        if event.event_type.is_install() {
                            let _install_status = if event.status == "completed" {
                                Status::Success
                            } else if event.status == "failed" {
                                Status::Fail
                            } else {
                                Status::Working
                            };
                            let mut envelope = MatrixEventEnvelope::state(
                                event.event_type.as_str(),
                                now,
                                "Install status update",
                            );
                            // State events don't use status, but we still add detail.
                            // Actually install events are state-like — use the action pattern.
                            envelope = envelope
                                .with_detail_field("worker_event", event.event_type.as_str())
                                .with_detail_field("worker_status", &*event.status)
                                .with_detail_field("job_id", &*job_id_for_print)
                                .with_detail_field("detail", &*event.detail);
                            for (key, value) in parse_event_detail_map(&event.detail) {
                                envelope =
                                    envelope.with_detail_field(&key, serde_json::json!(value));
                            }
                            daemon
                                .send_matrix_event(
                                    transport,
                                    daemon.resolve_room(RoomRole::Status),
                                    envelope,
                                    now,
                                )
                                .await?;
                        }
                        // NOTE: alert.generated events removed — worker failures
                        // are now emitted directly to #intake (via intake_run_id)
                        // and the Flutter app derives notifications client-side
                        // via EventRouter._shouldNotify().
                    }
                    if let (Some(transport), Some(_goal_template)) =
                        (matrix_transport.as_deref(), event.goal_template.as_deref())
                    {
                        // Single source of truth: DaemonEvent::to_envelope()
                        // in events.rs. No formatting logic lives here.
                        let envelope = event.to_envelope(now);
                        // Route goal events to the goal's thread room when available,
                        // fall back to goal_room, then #goals for system-level events.
                        let goal_target_room = event
                            .thread_id
                            .as_deref()
                            .and_then(|tid| daemon.resolve_thread_room(tid))
                            .or_else(|| event.goal_room.clone())
                            .unwrap_or_else(|| daemon.resolve_room(RoomRole::Goals).to_string());
                        daemon
                            .send_matrix_event(transport, &goal_target_room, envelope, now)
                            .await?;
                    }
                } else if draining {
                    info!("drain_complete");
                    break;
                } else if pumped == 0 {
                    debug!("idle");
                }
                // Periodic status.snapshot emission (configurable, default 5min)
                if !draining && snapshot_enabled && last_snapshot.elapsed() >= snapshot_interval {
                    if let Some(transport) = matrix_transport.as_deref() {
                        let uptime = start_time.elapsed().as_secs();
                        match daemon.build_snapshot_envelope(now, Some(uptime)) {
                            Ok(envelope) => {
                                if let Err(e) = daemon
                                    .send_matrix_event(
                                        transport,
                                        daemon.resolve_room(RoomRole::Status),
                                        envelope,
                                        now,
                                    )
                                    .await
                                {
                                    warn!("periodic_snapshot_failed: {e}");
                                }
                            }
                            Err(e) => {
                                warn!("periodic_snapshot_build_failed: {e}");
                            }
                        }
                    }
                    last_snapshot = Instant::now();
                }
                // Periodic retry of pending (failed) embeddings.
                if !draining
                    && embedding_retry_enabled
                    && last_embedding_retry.elapsed() >= embedding_retry_interval
                {
                    let embedded = daemon.retry_pending_embeddings();
                    if embedded > 0 {
                        info!("embedding_retry: embedded {embedded} pending chunks");
                    }
                    last_embedding_retry = Instant::now();
                }
                // Periodic active recall probes (~24 hour cadence by default).
                if !draining
                    && recall_probe_enabled
                    && last_recall_probe.elapsed() >= recall_probe_interval
                {
                    let recall_events = daemon.run_periodic_recall_probe_cycle(now)?;
                    if let Some(transport) = matrix_transport.as_deref() {
                        for routed in recall_events {
                            if let Err(e) = daemon
                                .send_matrix_event(transport, &routed.room_id, routed.envelope, now)
                                .await
                            {
                                warn!("periodic_recall_probe: event emission failed: {e}");
                            }
                        }
                    }
                    last_recall_probe = Instant::now();
                }
                // Periodic thread auto-archive (~1 hour cadence).
                if !draining && last_auto_archive.elapsed() >= auto_archive_interval {
                    let now = now_unix();
                    let archive_events = daemon.run_periodic_auto_archive(now);
                    if let Some(transport) = matrix_transport.as_deref() {
                        for event in &archive_events {
                            let envelope = event.to_envelope(now);
                            if let Err(e) = daemon
                                .send_matrix_event(
                                    transport,
                                    daemon.resolve_room(RoomRole::Status),
                                    envelope,
                                    now,
                                )
                                .await
                            {
                                warn!("periodic_auto_archive: event emission failed: {e}");
                            }
                        }
                    }
                    last_auto_archive = Instant::now();
                }
                // Periodic entity dedup (~24 hour cadence).
                if !draining && last_entity_dedup.elapsed() >= entity_dedup_interval {
                    daemon.run_periodic_entity_dedup();
                    last_entity_dedup = Instant::now();
                }
                // Periodic friction detection (~24 hour cadence).
                if !draining && last_friction_detection.elapsed() >= friction_detection_interval {
                    let proposal_events = daemon.run_periodic_friction_detection().await;
                    for event in &proposal_events {
                        if let Some(transport) = matrix_transport.as_deref() {
                            let ts = now_unix();
                            let envelope = event.to_envelope(ts);
                            // Send to thread's room if available, otherwise stream room.
                            let room_id = event
                                .thread_id
                                .as_deref()
                                .and_then(|tid| daemon.resolve_thread_room(tid))
                                .unwrap_or_else(|| {
                                    daemon
                                        .resolve_room(symbiotic_daemon::RoomRole::Stream)
                                        .to_string()
                                });
                            if let Err(e) = daemon
                                .send_matrix_event(transport, &room_id, envelope, now)
                                .await
                            {
                                warn!("periodic_friction_detection: event emission failed: {e}");
                            }
                        }
                    }
                    last_friction_detection = Instant::now();
                }

                iterations += 1;
                if let Some(limit) = max_iterations {
                    if iterations >= limit {
                        break;
                    }
                }
                if !draining {
                    thread::sleep(Duration::from_millis(sleep_ms.max(1)));
                }
            }

            // --- Shutdown: cancel repo scheduler and await its tasks ---
            // Task bodies park on `cancel_rx.changed()`; the broadcast below
            // wakes each one, which checks the new value and returns. Bound
            // the wait so a misbehaving task can't block the daemon from
            // exiting.
            let _ = scheduler_cancel_tx.send(true);
            for handle in scheduler_handles {
                let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
            }

            if let Some(swarm) = swarm_server.as_ref() {
                if let Err(e) = swarm.stop().await {
                    warn!("swarm: failed to stop git server container: {e}");
                }
            }
        }
        Command::Status { queue_file } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;
            let status = daemon.status_snapshot(now_unix())?;
            println!(
                "queued={} running={} failed={} done={} dlq={} ts={}",
                status.queued,
                status.running,
                status.failed,
                status.done,
                status.dlq,
                status.timestamp
            );
        }
        Command::Goals { queue_file, room } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;
            if let Some(room) = room {
                match daemon.goal_state(&room)? {
                    Some(state) => println!(
                        "room={} template={} status={} job_id={} run_id={} owner={} updated_at={}",
                        state.goal_room,
                        state.template,
                        state.status,
                        state.last_job_id,
                        state.last_run_id.unwrap_or_default(),
                        state.owner.unwrap_or_default(),
                        state.updated_at
                    ),
                    None => println!("goal_state_not_found room={room}"),
                }
            } else {
                let states = daemon.list_goal_states()?;
                for state in states {
                    println!(
                        "room={} template={} status={} job_id={} run_id={} owner={} updated_at={}",
                        state.goal_room,
                        state.template,
                        state.status,
                        state.last_job_id,
                        state.last_run_id.unwrap_or_default(),
                        state.owner.unwrap_or_default(),
                        state.updated_at
                    );
                }
            }
        }
        Command::Agents { queue_file, id } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;
            if let Some(id) = id {
                match daemon.agent_state(&id)? {
                    Some(state) => println!(
                        "agent_id={} parent={} llm_type={} trust_level={} last_status={} last_scope={} updated_at={}",
                        state.agent_id,
                        state.parent,
                        state.llm_type,
                        state.trust_level,
                        state.last_status,
                        state.last_scope.unwrap_or_default(),
                        state.updated_at
                    ),
                    None => println!("agent_state_not_found agent_id={id}"),
                }
            } else {
                let states = daemon.list_agent_states()?;
                for state in states {
                    println!(
                        "agent_id={} parent={} llm_type={} trust_level={} last_status={} last_scope={} updated_at={}",
                        state.agent_id,
                        state.parent,
                        state.llm_type,
                        state.trust_level,
                        state.last_status,
                        state.last_scope.unwrap_or_default(),
                        state.updated_at
                    );
                }
            }
        }
        Command::IngestBatch {
            queue_file,
            dir,
            meta,
            batch_size,
            skip_low,
            state_file,
        } => {
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) =
                SymbioticDaemon::open(DaemonConfig {
                    queue_file: queue_file.into(),
                    ..DaemonConfig::default()
                })?;

            // Load meta.json for enrichment if provided
            let meta_map: std::collections::HashMap<String, serde_json::Value> =
                if let Some(meta_path) = &meta {
                    let raw = std::fs::read_to_string(meta_path)
                        .with_context(|| format!("reading meta.json: {meta_path}"))?;
                    serde_json::from_str(&raw)
                        .with_context(|| format!("parsing meta.json: {meta_path}"))?
                } else {
                    std::collections::HashMap::new()
                };

            // Load resumable state
            let state_path = std::path::PathBuf::from(&state_file);
            let mut completed: std::collections::HashSet<String> = if state_path.exists() {
                let raw = std::fs::read_to_string(&state_path)?;
                serde_json::from_str(&raw).unwrap_or_default()
            } else {
                std::collections::HashSet::new()
            };

            // Walk the articles directory
            let dir_path = std::path::PathBuf::from(&dir);
            let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir_path)
                .with_context(|| format!("reading directory: {}", dir_path.display()))?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().map(|ext| ext == "md").unwrap_or(false))
                .collect();
            files.sort();

            info!(
                "ingest-batch: found {} markdown files in {}",
                files.len(),
                dir_path.display()
            );

            let mut processed = 0usize;
            let mut skipped = 0usize;
            let mut duplicates = 0usize;
            let mut errors = 0usize;

            for batch in files.chunks(batch_size) {
                for file_path in batch {
                    let file_name = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

                    // Skip already completed files
                    if completed.contains(file_name) {
                        skipped += 1;
                        continue;
                    }

                    // Check meta.json for relevance filtering
                    // Meta keys are the first 8 chars of the UUID filename
                    let meta_key = &file_name[..file_name.len().min(8)];
                    if skip_low {
                        if let Some(entry) = meta_map.get(meta_key) {
                            let relevance = entry
                                .get("relevance")
                                .and_then(|v| v.as_str())
                                .unwrap_or("medium");
                            let status = entry
                                .get("status")
                                .and_then(|v| v.as_str())
                                .unwrap_or("unread");
                            if relevance == "low" && status == "unread" {
                                skipped += 1;
                                completed.insert(file_name.to_string());
                                continue;
                            }
                        }
                    }

                    let path_str = file_path.to_string_lossy().to_string();

                    // Build tags from meta.json enrichment
                    let mut tags = vec!["source/legacy-kb".to_string(), "batch-import".to_string()];
                    if let Some(entry) = meta_map.get(meta_key) {
                        if let Some(relevance) = entry.get("relevance").and_then(|v| v.as_str()) {
                            tags.push(format!("relevance/{relevance}"));
                        }
                    }

                    let request = symbiotic_core::intake::IntakeRequest {
                        source: symbiotic_core::intake::IntakeSource::Cli,
                        kind: symbiotic_core::intake::IntakeKind::LocalFile,
                        urls: vec![],
                        note: None,
                        tags,
                        file_path: Some(path_str.clone()),
                        title: None,
                    };

                    match daemon.submit_intake_request(request) {
                        Ok(result) => {
                            let status = result
                                .items
                                .first()
                                .map(|i| format!("{:?}", i.status))
                                .unwrap_or_else(|| "unknown".to_string());
                            if status.contains("Duplicate") {
                                duplicates += 1;
                            } else {
                                processed += 1;
                            }
                            info!("ingest-batch: {} -> {}", file_name, status);
                        }
                        Err(e) => {
                            warn!("ingest-batch: {} -> error: {}", file_name, e);
                            errors += 1;
                        }
                    }

                    completed.insert(file_name.to_string());
                }

                // Save progress after each batch
                if let Some(parent) = state_path.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                let state_json = serde_json::to_string(&completed)?;
                std::fs::write(&state_path, state_json)?;

                info!(
                    "ingest-batch: progress — processed={processed} skipped={skipped} duplicates={duplicates} errors={errors} / total={}",
                    files.len()
                );
            }

            println!(
                "ingest-batch complete: processed={processed} skipped={skipped} duplicates={duplicates} errors={errors} total={}",
                files.len()
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_event() -> DaemonEvent {
        DaemonEvent {
            event_type: EventType::ThreadDistillery,
            status: "completed".to_string(),
            job_id: None,
            detail: "test".to_string(),
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
        }
    }

    #[test]
    fn should_emit_step_event_for_thread_scoped_events() {
        let mut event = test_event();
        event.thread_id = Some("thread-abc".to_string());
        assert!(should_emit_step_event(&event));
    }

    #[test]
    fn should_not_emit_internal_step_event_without_goal_or_thread_scope() {
        let event = test_event();
        assert!(!should_emit_step_event(&event));
    }
}
