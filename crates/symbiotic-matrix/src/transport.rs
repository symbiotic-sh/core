use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    config::SyncSettings,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    ruma::events::room::{
        encrypted::OriginalSyncRoomEncryptedEvent,
        message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
    },
    AuthSession, Client, Room,
};
use serde::{Deserialize, Serialize};
use url::form_urlencoded::byte_serialize;

use crate::events::MatrixEventEnvelope;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixMessage {
    pub room_id: String,
    pub sender: String,
    pub body: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SentEnvelope {
    pub room_id: String,
    pub envelope: MatrixEventEnvelope,
}

#[async_trait]
pub trait MatrixTransport: Send + Sync {
    async fn pop_incoming(&self) -> Option<MatrixMessage>;
    async fn send_outgoing(&self, room_id: &str, envelope: MatrixEventEnvelope) -> Result<()>;
}

#[derive(Default)]
pub struct InMemoryMatrixTransport {
    incoming: Mutex<VecDeque<MatrixMessage>>,
    outgoing: Mutex<Vec<SentEnvelope>>,
}

impl InMemoryMatrixTransport {
    pub fn push_incoming(&self, message: MatrixMessage) -> Result<()> {
        self.incoming
            .lock()
            .map_err(|_| anyhow!("incoming lock poisoned"))?
            .push_back(message);
        Ok(())
    }

    pub fn drain_outgoing(&self) -> Result<Vec<SentEnvelope>> {
        let mut outgoing = self
            .outgoing
            .lock()
            .map_err(|_| anyhow!("outgoing lock poisoned"))?;
        let drained = outgoing.clone();
        outgoing.clear();
        Ok(drained)
    }
}

#[async_trait]
impl MatrixTransport for InMemoryMatrixTransport {
    async fn pop_incoming(&self) -> Option<MatrixMessage> {
        self.incoming.lock().ok()?.pop_front()
    }

    async fn send_outgoing(&self, room_id: &str, envelope: MatrixEventEnvelope) -> Result<()> {
        self.outgoing
            .lock()
            .map_err(|_| anyhow!("outgoing lock poisoned"))?
            .push(SentEnvelope {
                room_id: room_id.to_string(),
                envelope,
            });
        Ok(())
    }
}

pub struct FileMatrixTransport {
    incoming_file: PathBuf,
    outgoing_file: PathBuf,
    lock: Mutex<()>,
}

impl FileMatrixTransport {
    pub fn open(incoming_file: impl AsRef<Path>, outgoing_file: impl AsRef<Path>) -> Result<Self> {
        let incoming_file = incoming_file.as_ref().to_path_buf();
        let outgoing_file = outgoing_file.as_ref().to_path_buf();
        init_transport_file(&incoming_file)?;
        init_transport_file(&outgoing_file)?;
        Ok(Self {
            incoming_file,
            outgoing_file,
            lock: Mutex::new(()),
        })
    }
}

#[async_trait]
impl MatrixTransport for FileMatrixTransport {
    async fn pop_incoming(&self) -> Option<MatrixMessage> {
        let _guard = self.lock.lock().ok()?;
        let content = fs::read_to_string(&self.incoming_file).ok()?;
        let mut remainder = Vec::new();
        let mut first = None;

        for line in content.lines() {
            if first.is_none() {
                match serde_json::from_str::<MatrixMessage>(line) {
                    Ok(message) => {
                        first = Some(message);
                        continue;
                    }
                    Err(_) => {
                        // Drop malformed input lines to avoid an infinite parse loop.
                        continue;
                    }
                }
            }
            remainder.push(line);
        }

        if first.is_some() {
            let mut rewritten = remainder.join("\n");
            if !rewritten.is_empty() {
                rewritten.push('\n');
            }
            // Best-effort rewrite; if this fails we still return the popped message.
            let _ = fs::write(&self.incoming_file, rewritten);
        }
        first
    }

    async fn send_outgoing(&self, room_id: &str, envelope: MatrixEventEnvelope) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("file transport lock poisoned"))?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.outgoing_file)
            .with_context(|| {
                format!(
                    "failed to open outgoing matrix transport {}",
                    self.outgoing_file.display()
                )
            })?;
        let payload = serde_json::to_string(&SentEnvelope {
            room_id: room_id.to_string(),
            envelope,
        })?;
        writeln!(file, "{payload}").with_context(|| {
            format!(
                "failed writing outgoing matrix transport {}",
                self.outgoing_file.display()
            )
        })?;
        Ok(())
    }
}

fn init_transport_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }
    if !path.exists() {
        fs::File::create(path)
            .with_context(|| format!("failed to create transport file {}", path.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms).with_context(|| {
            format!(
                "failed to set permissions on transport file {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

#[derive(Clone)]
pub struct LiveMatrixConfig {
    pub homeserver_url: String,
    pub access_token: String,
    pub self_user_id: Option<String>,
    pub sync_timeout_ms: u64,
    pub since_file: Option<PathBuf>,
}

impl std::fmt::Debug for LiveMatrixConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveMatrixConfig")
            .field("homeserver_url", &self.homeserver_url)
            .field("access_token", &"[REDACTED]")
            .field("self_user_id", &self.self_user_id)
            .field("sync_timeout_ms", &self.sync_timeout_ms)
            .field("since_file", &self.since_file)
            .finish()
    }
}

impl LiveMatrixConfig {
    fn normalized_homeserver(&self) -> String {
        self.homeserver_url.trim_end_matches('/').to_string()
    }
}

#[derive(Clone)]
pub struct MatrixSdkConfig {
    pub homeserver_url: String,
    pub user_id: String,
    pub password: String,
    pub sync_timeout_ms: u64,
    pub data_dir: PathBuf,
    pub session_file: Option<PathBuf>,
    pub self_user_id: Option<String>,
    /// Require Matrix rooms to be encrypted before processing/sending events.
    pub require_e2ee: bool,
    /// Delete stale test devices older than this many days.
    /// 0 disables cleanup entirely. Default: 7.
    pub stale_device_cleanup_days: u64,
    /// Device display name for Matrix login. Production uses "Symbiotic Nucleus",
    /// test environments should use "Symbiotic Nucleus (test)" so stale test
    /// devices can be identified and cleaned up without touching production devices.
    pub device_display_name: Option<String>,
}

impl std::fmt::Debug for MatrixSdkConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MatrixSdkConfig")
            .field("homeserver_url", &self.homeserver_url)
            .field("user_id", &self.user_id)
            .field("password", &"[REDACTED]")
            .field("sync_timeout_ms", &self.sync_timeout_ms)
            .field("data_dir", &self.data_dir)
            .field("session_file", &self.session_file)
            .field("self_user_id", &self.self_user_id)
            .field("require_e2ee", &self.require_e2ee)
            .finish()
    }
}

impl MatrixSdkConfig {
    fn normalized_homeserver(&self) -> String {
        self.homeserver_url.trim_end_matches('/').to_string()
    }

    fn resolved_session_file(&self) -> PathBuf {
        self.session_file
            .clone()
            .unwrap_or_else(|| self.data_dir.join("matrix-session.json"))
    }
}

pub struct MatrixSdkTransport {
    config: MatrixSdkConfig,
    client: Client,
    state: Arc<Mutex<MatrixSdkState>>,
}

#[derive(Default)]
struct MatrixSdkState {
    pending: VecDeque<MatrixMessage>,
    alias_to_room_id: HashMap<String, String>,
}

impl MatrixSdkTransport {
    pub async fn open(config: MatrixSdkConfig) -> Result<Self> {
        if config.homeserver_url.trim().is_empty() {
            return Err(anyhow!("matrix homeserver URL cannot be empty"));
        }
        if config.user_id.trim().is_empty() {
            return Err(anyhow!("matrix user id cannot be empty"));
        }
        if config.password.trim().is_empty() {
            return Err(anyhow!("matrix password cannot be empty"));
        }
        fs::create_dir_all(&config.data_dir).with_context(|| {
            format!(
                "failed to create matrix sdk data directory {}",
                config.data_dir.display()
            )
        })?;
        let state = Arc::new(Mutex::new(MatrixSdkState::default()));
        let db_path = config.data_dir.join("matrix-sdk-state");
        let client = Client::builder()
            .homeserver_url(config.normalized_homeserver())
            .sqlite_store(&db_path, None)
            .with_encryption_settings(EncryptionSettings {
                // Don't auto-create backups — the Flutter app bootstraps SSSS + backup.
                // The daemon joins the existing backup via setup_key_backup().
                auto_enable_backups: false,
                backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
                ..Default::default()
            })
            .build()
            .await
            .context("failed to build matrix sdk client")?;
        restore_or_login_session(&client, &config).await?;

        // Warmup syncs: upload device keys and allow initial key exchange.
        // When running as a fresh device (e.g., after reinstall or in E2E tests),
        // the SDK needs multiple sync rounds to:
        //   1. Upload our device keys to the homeserver
        //   2. Process key query/claim responses from other devices
        //   3. Establish Olm sessions for E2EE message decryption
        // Without this, messages from other devices may arrive as UTD (Unable To
        // Decrypt) because the sender didn't know our device when encrypting.
        for i in 1..=3 {
            eprintln!("matrix transport: warmup sync {i}/3");
            if let Err(e) = client
                .sync_once(SyncSettings::default().timeout(Duration::from_secs(3)))
                .await
            {
                eprintln!("matrix transport: warmup sync {i} failed: {e}");
            }
        }

        // Purge stale test devices to prevent E2EE key-share fan-out to
        // ghost devices. Only deletes devices whose display name starts with
        // the test prefix ("Symbiotic Nucleus (test)" or "Symbiotic E2E Test").
        // Never touches the current device or production/app devices.
        if config.stale_device_cleanup_days > 0 {
            cleanup_stale_test_devices(&client, config.stale_device_cleanup_days).await;
        }

        // Enable key backup so Megolm session keys are uploaded to the
        // server-side backup. This is critical for same-user operation:
        // the app can then restore these keys after reinstall.
        setup_key_backup(&client, &config.password).await;

        install_sdk_event_handler(&client, state.clone(), config.require_e2ee);
        install_utd_event_handler(&client);
        Ok(Self {
            config,
            client,
            state,
        })
    }

    async fn sync_once(&self) -> Result<()> {
        let timeout = self.config.sync_timeout_ms.max(1);
        self.client
            .sync_once(SyncSettings::default().timeout(Duration::from_millis(timeout)))
            .await
            .context("matrix sdk sync failed")?;
        Ok(())
    }

    fn resolve_room_id_for_send(&self, room_id: &str) -> Result<String> {
        if room_id.starts_with('!') {
            return Ok(room_id.to_string());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow!("matrix sdk state lock poisoned"))?;
        Ok(state
            .alias_to_room_id
            .get(room_id)
            .cloned()
            .unwrap_or_else(|| room_id.to_string()))
    }
}

#[async_trait]
impl MatrixTransport for MatrixSdkTransport {
    async fn pop_incoming(&self) -> Option<MatrixMessage> {
        {
            let mut state = self.state.lock().ok()?;
            if let Some(message) = state.pending.pop_front() {
                return Some(message);
            }
        }
        if self.sync_once().await.is_err() {
            return None;
        }
        self.state.lock().ok()?.pending.pop_front()
    }

    async fn send_outgoing(&self, room_id: &str, envelope: MatrixEventEnvelope) -> Result<()> {
        let resolved_room_id = self.resolve_room_id_for_send(room_id)?;
        let Some(room) = self
            .client
            .joined_rooms()
            .into_iter()
            .find(|room| room.room_id().as_str() == resolved_room_id)
        else {
            return Err(anyhow!(
                "matrix sdk transport could not resolve joined room `{room_id}`"
            ));
        };
        if self.config.require_e2ee {
            let encrypted = room
                .latest_encryption_state()
                .await
                .map(|s| s.is_encrypted())
                .unwrap_or(false);
            if !encrypted {
                return Err(anyhow!(
                    "matrix sdk transport requires E2EE room for send `{room_id}`"
                ));
            }
        }
        // Build custom msgtype so the Flutter parser sees "sym.e"
        // directly in the content, with `sym` as a nested object.
        let mut data = serde_json::Map::new();
        data.insert("sym".to_string(), serde_json::to_value(&envelope.sym)?);
        let msg_type = MessageType::new(&envelope.msgtype, envelope.body.clone(), data)
            .map_err(|e| anyhow!("failed to build custom message type: {e}"))?;
        room.send(RoomMessageEventContent::new(msg_type))
            .await
            .context("matrix sdk send failed")?;
        Ok(())
    }
}

async fn restore_or_login_session(client: &Client, config: &MatrixSdkConfig) -> Result<()> {
    let session_file = config.resolved_session_file();
    if session_file.exists() {
        let raw = tokio::fs::read_to_string(&session_file)
            .await
            .with_context(|| format!("failed to read matrix session {}", session_file.display()))?;
        let session: MatrixSession =
            serde_json::from_str(&raw).context("failed to parse matrix session JSON")?;
        if client.restore_session(session).await.is_ok() {
            // Verify the restored token is still valid by making a lightweight API call.
            match client.whoami().await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    eprintln!("warn: restored session token is stale, re-logging in: {e}");
                }
            }
        }
    }

    let display_name = config
        .device_display_name
        .as_deref()
        .unwrap_or("Symbiotic Nucleus");
    client
        .matrix_auth()
        .login_username(&config.user_id, &config.password)
        .initial_device_display_name(display_name)
        .await
        .context("matrix sdk password login failed")?;
    persist_matrix_session(client, &session_file).await?;
    Ok(())
}

fn install_sdk_event_handler(
    client: &Client,
    state: Arc<Mutex<MatrixSdkState>>,
    require_e2ee: bool,
) {
    client.add_event_handler(move |event: OriginalSyncRoomMessageEvent, room: Room| {
        let state = state.clone();
        async move {
            if require_e2ee {
                let encrypted = room
                    .latest_encryption_state()
                    .await
                    .map(|s| s.is_encrypted())
                    .unwrap_or(false);
                if !encrypted {
                    return;
                }
            }
            let sender = event.sender.to_string();

            // Filter by msgtype: only process m.text messages (user commands).
            // Daemon events use msgtype "sym.e" which hits the _ arm.
            // This replaces the old user-ID self-filter, allowing same-user operation
            // (daemon and app as the same Matrix user for shared key backup).
            let body = match event.content.msgtype {
                MessageType::Text(text) => text.body.to_string(),
                _ => return,
            };
            let room_id = room.room_id().to_string();
            let room_alias = room
                .canonical_alias()
                .map(|alias| alias.to_string())
                .or_else(|| room.alt_aliases().pop().map(|alias| alias.to_string()));
            let Ok(mut lock) = state.lock() else {
                return;
            };
            if let Some(alias) = room_alias {
                lock.alias_to_room_id.insert(alias, room_id.clone());
            }
            lock.pending.push_back(MatrixMessage {
                room_id,
                sender,
                body,
                timestamp: normalize_ts_secs(event.origin_server_ts.get().into()),
            });
        }
    });
}

/// Registers an event handler for encrypted events that the SDK could not
/// decrypt (UTD — Unable To Decrypt). This provides visibility into E2EE
/// key exchange failures that would otherwise be silently swallowed.
fn install_utd_event_handler(client: &Client) {
    client.add_event_handler(
        |event: OriginalSyncRoomEncryptedEvent, room: Room| async move {
            eprintln!(
                "matrix transport: UTD — encrypted event in room {} from {} \
                 (event_id={}). Key exchange may still be in progress.",
                room.room_id(),
                event.sender,
                event.event_id,
            );
        },
    );
}

/// Test device display name prefixes that are safe to clean up.
/// Production devices ("Symbiotic Nucleus" without suffix) are NEVER deleted.
const TEST_DEVICE_PREFIXES: &[&str] = &["Symbiotic Nucleus (test)", "Symbiotic E2E Test"];

/// Returns true if a device display name matches a test device pattern.
fn is_test_device(display_name: &str) -> bool {
    TEST_DEVICE_PREFIXES
        .iter()
        .any(|prefix| display_name.starts_with(prefix))
}

/// Delete stale test devices to reduce E2EE key-share overhead.
///
/// Safety rules:
/// - NEVER deletes the current device
/// - NEVER deletes devices without a display name (unknown origin)
/// - NEVER deletes production devices ("Symbiotic Nucleus" without test suffix)
/// - Only deletes devices whose display name matches TEST_DEVICE_PREFIXES
/// - Only deletes devices not seen within `max_age_days`
/// - Logs every deletion for auditability
async fn cleanup_stale_test_devices(client: &Client, max_age_days: u64) {
    let current_device_id = client.device_id().map(|id| id.to_string());

    let devices = match client.devices().await {
        Ok(resp) => resp.devices,
        Err(e) => {
            eprintln!("device cleanup: failed to list devices: {e}");
            return;
        }
    };

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let max_age_ms = max_age_days * 24 * 60 * 60 * 1000;

    let mut deleted = 0u32;
    let total = devices.len();

    for device in &devices {
        let device_id = device.device_id.to_string();

        // Never delete current device
        if current_device_id.as_deref() == Some(&device_id) {
            continue;
        }

        // Only delete devices with a known test display name
        let display_name = match device.display_name.as_deref() {
            Some(name) => name,
            None => continue,
        };

        if !is_test_device(display_name) {
            continue;
        }

        // Only delete if last_seen is older than max_age_days
        // If last_seen is missing, use 0 (treat as ancient)
        let last_seen_ms = device
            .last_seen_ts
            .map(|ts| u64::from(ts.get()))
            .unwrap_or(0);

        if now_ms.saturating_sub(last_seen_ms) < max_age_ms {
            continue;
        }

        let age_days = now_ms.saturating_sub(last_seen_ms) / (24 * 60 * 60 * 1000);
        eprintln!(
            "device cleanup: deleting stale test device {device_id} \
             (name={display_name:?}, last_seen={age_days}d ago)"
        );

        // Delete without interactive auth — Conduit/Conduwuit typically
        // doesn't require UIA for device deletion by the owning user.
        if let Err(e) = client
            .delete_devices(std::slice::from_ref(&device.device_id), None)
            .await
        {
            eprintln!("device cleanup: failed to delete {device_id}: {e}");
        } else {
            deleted += 1;
        }
    }

    if deleted > 0 {
        eprintln!(
            "device cleanup: removed {deleted} stale test device(s) \
             ({} remaining out of {total})",
            total - deleted as usize
        );
    }
}

async fn persist_matrix_session(client: &Client, session_file: &Path) -> Result<()> {
    let session = client
        .session()
        .ok_or_else(|| anyhow!("matrix sdk login did not produce session"))?;
    let matrix_session = match session {
        AuthSession::Matrix(session) => session,
        _ => return Err(anyhow!("unsupported matrix auth session type")),
    };
    let encoded = serde_json::to_string_pretty(&matrix_session)?;
    if let Some(parent) = session_file.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "failed to create matrix session parent directory {}",
                parent.display()
            )
        })?;
    }
    tokio::fs::write(session_file, encoded)
        .await
        .with_context(|| {
            format!(
                "failed to persist matrix session {}",
                session_file.display()
            )
        })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(session_file, perms).with_context(|| {
            format!(
                "failed to set permissions on matrix session {}",
                session_file.display()
            )
        })?;
    }
    Ok(())
}

/// Enable key backup so Megolm session keys are uploaded to the server-side
/// backup. Opens SSSS (4S) with the user's password, imports the backup
/// decryption key, and lets the SDK auto-upload outbound group session keys.
///
/// Best-effort: logs warnings on failure but never prevents daemon startup.
async fn setup_key_backup(client: &Client, password: &str) {
    // Initial sync to pull account data (SSSS configuration lives there).
    if let Err(e) = client
        .sync_once(SyncSettings::default().timeout(Duration::from_secs(10)))
        .await
    {
        eprintln!("key backup: initial sync failed: {e}");
        return;
    }

    let encryption = client.encryption();

    // Check if a backup exists on the server (set up by the Flutter app).
    let backup_exists = match encryption.backups().exists_on_server().await {
        Ok(exists) => exists,
        Err(e) => {
            eprintln!("key backup: could not check server backup: {e}");
            false
        }
    };
    if !backup_exists {
        eprintln!("key backup: no backup on server yet (app will set it up)");
        return;
    }

    // Open SSSS with the user's password and import secrets.
    // import_secrets() internally calls maybe_enable_backups() which verifies
    // the recovery key in SSSS matches the server's backup and enables upload.
    let store = match encryption
        .secret_storage()
        .open_secret_store(password)
        .await
    {
        Ok(store) => store,
        Err(e) => {
            eprintln!("key backup: could not open SSSS: {e}");
            return;
        }
    };

    if let Err(e) = store.import_secrets().await {
        eprintln!("key backup: import_secrets failed: {e}");
        return;
    }

    if encryption.backups().are_enabled().await {
        eprintln!("key backup: enabled — Megolm keys will be auto-uploaded");
    } else {
        eprintln!(
            "key backup: secrets imported but backup not enabled (state={:?})",
            encryption.backups().state()
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Put,
}

impl HttpMethod {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatrixHttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub bearer_token: String,
    pub body: Option<String>,
}

pub trait MatrixHttpClient: Send + Sync {
    fn execute(&self, request: MatrixHttpRequest) -> Result<String>;
}

pub struct CurlMatrixHttpClient;

impl CurlMatrixHttpClient {
    pub fn is_available() -> bool {
        let mut cmd = Command::new("curl");
        // Clear inherited environment to prevent secret leakage to child processes.
        cmd.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            cmd.env("PATH", path);
        }
        cmd.arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }
}

impl MatrixHttpClient for CurlMatrixHttpClient {
    fn execute(&self, request: MatrixHttpRequest) -> Result<String> {
        use std::process::Stdio;

        let mut command = Command::new("curl");
        // Clear inherited environment to prevent secret leakage to child processes.
        command.env_clear();
        if let Ok(path) = std::env::var("PATH") {
            command.env("PATH", path);
        }
        if let Ok(home) = std::env::var("HOME") {
            command.env("HOME", home);
        }
        command
            .arg("-sS")
            .arg("--fail")
            .arg("-X")
            .arg(request.method.as_str())
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-K")
            .arg("-"); // read bearer header from stdin to avoid argv exposure
        if let Some(body) = request.body {
            command.arg("--data").arg(body);
        }
        command.arg(&request.url);
        command.stdin(Stdio::piped());

        // Validate bearer token BEFORE spawning the process.
        // Curl -K config uses `"` as value delimiters, so reject tokens
        // containing `"`, `\`, `\n`, or `\r` to prevent config injection.
        if request
            .bearer_token
            .chars()
            .any(|c| c == '\n' || c == '\r' || c == '"' || c == '\\')
        {
            return Err(anyhow!(
                "bearer token contains characters unsafe for curl config"
            ));
        }
        let mut child = command
            .spawn()
            .context("failed to spawn curl for matrix request")?;
        if let Some(mut stdin) = child.stdin.take() {
            let config_line = format!(
                "header = \"Authorization: Bearer {}\"",
                request.bearer_token
            );
            stdin
                .write_all(config_line.as_bytes())
                .context("failed to write bearer config to curl stdin")?;
        }
        let output = child
            .wait_with_output()
            .context("failed to execute curl for matrix request")?;
        if !output.status.success() {
            return Err(anyhow!(
                "matrix curl request failed with status {}",
                output.status.code().unwrap_or(-1)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

pub struct LiveMatrixTransport<C: MatrixHttpClient = CurlMatrixHttpClient> {
    config: LiveMatrixConfig,
    client: C,
    state: Mutex<LiveMatrixState>,
}

#[derive(Default)]
struct LiveMatrixState {
    pending: VecDeque<MatrixMessage>,
    since: Option<String>,
    txn_counter: u64,
}

impl LiveMatrixTransport<CurlMatrixHttpClient> {
    pub fn open(config: LiveMatrixConfig) -> Result<Self> {
        Self::with_client(config, CurlMatrixHttpClient)
    }
}

impl<C: MatrixHttpClient> LiveMatrixTransport<C> {
    pub fn with_client(config: LiveMatrixConfig, client: C) -> Result<Self> {
        if config.homeserver_url.trim().is_empty() {
            return Err(anyhow!("matrix homeserver URL cannot be empty"));
        }
        if config.access_token.trim().is_empty() {
            return Err(anyhow!("matrix access token cannot be empty"));
        }
        let since = load_since_token(config.since_file.as_deref())?;
        Ok(Self {
            config,
            client,
            state: Mutex::new(LiveMatrixState {
                since,
                ..LiveMatrixState::default()
            }),
        })
    }

    fn sync_url(&self, since: Option<&str>) -> String {
        let mut url = format!(
            "{}/_matrix/client/v3/sync?timeout={}",
            self.config.normalized_homeserver(),
            self.config.sync_timeout_ms.max(1)
        );
        if let Some(token) = since {
            url.push_str("&since=");
            url.push_str(&encode_component(token));
        }
        url
    }

    fn send_url(&self, room_id: &str, txn_id: &str) -> String {
        format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            self.config.normalized_homeserver(),
            encode_component(room_id),
            encode_component(txn_id)
        )
    }

    fn fetch_sync_messages(&self) -> Result<()> {
        let since = self
            .state
            .lock()
            .map_err(|_| anyhow!("live matrix state lock poisoned"))?
            .since
            .clone();
        let response = self.client.execute(MatrixHttpRequest {
            method: HttpMethod::Get,
            url: self.sync_url(since.as_deref()),
            bearer_token: self.config.access_token.clone(),
            body: None,
        })?;
        let parsed: SyncResponse =
            serde_json::from_str(&response).context("failed to parse matrix sync response JSON")?;

        let mut messages = Vec::new();
        if let Some(rooms) = parsed.rooms {
            if let Some(joined) = rooms.join {
                for (room_id, room) in joined {
                    if let Some(timeline) = room.timeline {
                        for event in timeline.events {
                            if event.event_type != "m.room.message" {
                                continue;
                            }
                            let Some(body) = event.content.body else {
                                continue;
                            };
                            if body.trim().is_empty() {
                                continue;
                            }
                            // Filter by msgtype: only process m.text messages.
                            // Daemon events use "sym.e" msgtype, so skip anything
                            // that isn't "m.text" (user commands).
                            // This replaces the old user-ID self-filter, allowing
                            // same-user operation for shared key backup.
                            if event
                                .content
                                .msgtype
                                .as_deref()
                                .map(|mt| mt != "m.text")
                                .unwrap_or(true)
                            {
                                continue;
                            }
                            messages.push(MatrixMessage {
                                room_id: room_id.clone(),
                                sender: event.sender,
                                body,
                                timestamp: normalize_ts_secs(event.origin_server_ts),
                            });
                        }
                    }
                }
            }
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("live matrix state lock poisoned"))?;
        for message in messages {
            state.pending.push_back(message);
        }
        if let Some(next_batch) = parsed.next_batch {
            state.since = Some(next_batch.clone());
            persist_since_token(self.config.since_file.as_deref(), &next_batch)?;
        }
        Ok(())
    }
}

#[async_trait]
impl<C: MatrixHttpClient> MatrixTransport for LiveMatrixTransport<C> {
    async fn pop_incoming(&self) -> Option<MatrixMessage> {
        {
            let mut state = self.state.lock().ok()?;
            if let Some(message) = state.pending.pop_front() {
                return Some(message);
            }
        }
        if self.fetch_sync_messages().is_err() {
            return None;
        }
        self.state.lock().ok()?.pending.pop_front()
    }

    async fn send_outgoing(&self, room_id: &str, envelope: MatrixEventEnvelope) -> Result<()> {
        let txn_id = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow!("live matrix state lock poisoned"))?;
            state.txn_counter = state.txn_counter.saturating_add(1);
            format!("symbiotic-{}", state.txn_counter)
        };
        let payload = serde_json::to_string(&envelope)?;
        let request = MatrixHttpRequest {
            method: HttpMethod::Put,
            url: self.send_url(room_id, &txn_id),
            bearer_token: self.config.access_token.clone(),
            body: Some(payload),
        };
        self.client.execute(request)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncResponse {
    next_batch: Option<String>,
    rooms: Option<SyncRooms>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncRooms {
    join: Option<std::collections::HashMap<String, SyncRoom>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncRoom {
    timeline: Option<SyncTimeline>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncTimeline {
    events: Vec<SyncEvent>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncEvent {
    #[serde(rename = "type")]
    event_type: String,
    sender: String,
    origin_server_ts: u64,
    content: SyncContent,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SyncContent {
    body: Option<String>,
    msgtype: Option<String>,
}

fn normalize_ts_secs(raw: u64) -> u64 {
    if raw > 10_000_000_000 {
        raw / 1000
    } else {
        raw
    }
}

fn encode_component(value: &str) -> String {
    byte_serialize(value.as_bytes()).collect()
}

fn load_since_token(path: Option<&Path>) -> Result<Option<String>> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let token = fs::read_to_string(path)
        .with_context(|| format!("failed to read matrix since token {}", path.display()))?;
    let trimmed = token.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

fn persist_since_token(path: Option<&Path>, token: &str) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create matrix since token parent dir {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, token)
        .with_context(|| format!("failed to write matrix since token {}", path.display()))?;
    symbiotic_core::harden_file_permissions(path, 0o600).with_context(|| {
        format!(
            "failed to harden matrix since token permissions {}",
            path.display()
        )
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::MatrixEventEnvelope;
    use std::sync::Arc;
    use symbiotic_core::protocol::{Kind, Status};

    #[tokio::test]
    async fn in_memory_transport_roundtrip() {
        let transport = InMemoryMatrixTransport::default();
        transport
            .push_incoming(MatrixMessage {
                room_id: "#intake".to_string(),
                sender: "@user:test".to_string(),
                body: "https://example.com".to_string(),
                timestamp: 1,
            })
            .expect("push_incoming");

        let incoming = transport
            .pop_incoming()
            .await
            .expect("message should exist");
        assert_eq!(incoming.room_id, "#intake");

        transport
            .send_outgoing(
                "#intake",
                MatrixEventEnvelope::new(Kind::Message, Status::Success, 1, "done"),
            )
            .await
            .expect("send");

        let out = transport.drain_outgoing().expect("drain_outgoing");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].room_id, "#intake");
        assert_eq!(out[0].envelope.sym.k, Kind::Message);
    }

    #[tokio::test]
    async fn file_transport_roundtrip() {
        let root = std::env::temp_dir().join(format!(
            "symbiotic_matrix_transport_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let incoming = root.join("incoming.ndjson");
        let outgoing = root.join("outgoing.ndjson");
        fs::create_dir_all(&root).expect("root should be created");

        let input_payload = serde_json::to_string(&MatrixMessage {
            room_id: "#intake".to_string(),
            sender: "@user:test".to_string(),
            body: "https://example.com".to_string(),
            timestamp: 1,
        })
        .expect("input serialization should work");
        fs::write(&incoming, format!("{input_payload}\n"))
            .expect("incoming file should be written");

        let transport = FileMatrixTransport::open(&incoming, &outgoing).expect("transport open");
        let popped = transport.pop_incoming().await.expect("message should pop");
        assert_eq!(popped.room_id, "#intake");
        assert!(transport.pop_incoming().await.is_none());

        transport
            .send_outgoing(
                "#intake",
                MatrixEventEnvelope::new(Kind::Message, Status::Success, 1, "done"),
            )
            .await
            .expect("send should work");
        let written = fs::read_to_string(outgoing).expect("outgoing file should be readable");
        assert!(written.contains("sym.e"));
    }

    #[derive(Clone, Default)]
    struct StubHttpClient {
        responses: Arc<Mutex<VecDeque<Result<String>>>>,
        requests: Arc<Mutex<Vec<MatrixHttpRequest>>>,
    }

    impl StubHttpClient {
        fn queue_response(&self, response: Result<String>) {
            self.responses
                .lock()
                .expect("stub responses lock")
                .push_back(response);
        }

        fn requests(&self) -> Vec<MatrixHttpRequest> {
            self.requests.lock().expect("stub requests lock").clone()
        }
    }

    impl MatrixHttpClient for StubHttpClient {
        fn execute(&self, request: MatrixHttpRequest) -> Result<String> {
            self.requests
                .lock()
                .expect("stub requests lock")
                .push(request);
            self.responses
                .lock()
                .expect("stub responses lock")
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("no stubbed response")))
        }
    }

    fn live_config(name: &str) -> LiveMatrixConfig {
        let root = std::env::temp_dir().join(format!(
            "symbiotic_matrix_live_{name}_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        LiveMatrixConfig {
            homeserver_url: "https://matrix.example.test".to_string(),
            access_token: "token123".to_string(),
            self_user_id: None,
            sync_timeout_ms: 100,
            since_file: Some(root.join("since.token")),
        }
    }

    #[tokio::test]
    async fn live_transport_pop_reads_sync_and_persists_since() {
        let client = StubHttpClient::default();
        client.queue_response(Ok(r#"{
              "next_batch":"s123",
              "rooms":{
                "join":{
                  "!room:example.test":{
                    "timeline":{
                      "events":[
                        {
                          "type":"m.room.message",
                          "sender":"@user:example.test",
                          "origin_server_ts":1710000000123,
                          "content":{"body":"workflow intake-url","msgtype":"m.text"}
                        }
                      ]
                    }
                  }
                }
              }
            }"#
        .to_string()));
        client.queue_response(Ok(r#"{
              "next_batch":"s124",
              "rooms":{"join":{}}
            }"#
        .to_string()));
        let config = live_config("sync");
        let since_file = config.since_file.clone().expect("since file");
        let transport = LiveMatrixTransport::with_client(config, client.clone()).expect("open");

        let message = transport
            .pop_incoming()
            .await
            .expect("message should exist");
        assert_eq!(message.room_id, "!room:example.test");
        assert_eq!(message.sender, "@user:example.test");
        assert_eq!(message.body, "workflow intake-url");
        assert_eq!(message.timestamp, 1_710_000_000);
        assert!(transport.pop_incoming().await.is_none());

        let stored_since = fs::read_to_string(since_file).expect("since token should persist");
        assert_eq!(stored_since, "s124");

        let requests = client.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].method, HttpMethod::Get);
        assert!(requests[0].url.contains("/_matrix/client/v3/sync"));
    }

    #[tokio::test]
    async fn live_transport_pop_filters_by_msgtype() {
        let client = StubHttpClient::default();
        // Daemon events use msgtype "sym.e" — should be skipped.
        // Only "m.text" messages (user commands) should be kept.
        client.queue_response(Ok(r#"{
              "next_batch":"s-self",
              "rooms":{
                "join":{
                  "!room:example.test":{
                    "timeline":{
                      "events":[
                        {
                          "type":"m.room.message",
                          "sender":"@testuser:example.test",
                          "origin_server_ts":1710000000123,
                          "content":{"body":"daemon event payload","msgtype":"sym.e"}
                        },
                        {
                          "type":"m.room.message",
                          "sender":"@testuser:example.test",
                          "origin_server_ts":1710000001123,
                          "content":{"body":"user command","msgtype":"m.text"}
                        },
                        {
                          "type":"m.room.message",
                          "sender":"@testuser:example.test",
                          "origin_server_ts":1710000002123,
                          "content":{"body":"no msgtype message"}
                        }
                      ]
                    }
                  }
                }
              }
            }"#
        .to_string()));
        let config = live_config("msgtype-filter");
        let transport = LiveMatrixTransport::with_client(config, client).expect("open");

        let first = transport
            .pop_incoming()
            .await
            .expect("message should exist");
        assert_eq!(first.body, "user command");
        // The daemon event and no-msgtype event should both be filtered out
        assert!(transport.pop_incoming().await.is_none());
    }

    #[tokio::test]
    async fn live_transport_send_issues_put_request() {
        let client = StubHttpClient::default();
        client.queue_response(Ok("{}".to_string()));
        let config = live_config("send");
        let transport = LiveMatrixTransport::with_client(config, client.clone()).expect("open");

        transport
            .send_outgoing(
                "!room:example.test",
                MatrixEventEnvelope::new(Kind::Message, Status::Success, 1, "done"),
            )
            .await
            .expect("send should work");

        let requests = client.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, HttpMethod::Put);
        assert!(requests[0]
            .url
            .contains("/rooms/%21room%3Aexample.test/send/m.room.message/"));
        let body = requests[0].body.as_deref().expect("body required");
        assert!(body.contains("\"msgtype\":\"sym.e\""));
        assert!(body.contains("\"k\":0"), "should contain kind=Message(0)");
    }

    #[test]
    fn curl_client_rejects_bearer_with_quote() {
        let client = CurlMatrixHttpClient;
        let err = client
            .execute(MatrixHttpRequest {
                method: HttpMethod::Get,
                url: "https://matrix.example.test/_matrix/client/v3/sync".to_string(),
                bearer_token: "token\"injected".to_string(),
                body: None,
            })
            .expect_err("should reject quote in bearer token");
        assert!(err.to_string().contains("unsafe for curl config"));
    }

    #[test]
    fn curl_client_rejects_bearer_with_backslash() {
        let client = CurlMatrixHttpClient;
        let err = client
            .execute(MatrixHttpRequest {
                method: HttpMethod::Get,
                url: "https://matrix.example.test/_matrix/client/v3/sync".to_string(),
                bearer_token: "token\\escaped".to_string(),
                body: None,
            })
            .expect_err("should reject backslash in bearer token");
        assert!(err.to_string().contains("unsafe for curl config"));
    }

    #[test]
    fn curl_client_rejects_bearer_with_newline() {
        let client = CurlMatrixHttpClient;
        let err = client
            .execute(MatrixHttpRequest {
                method: HttpMethod::Get,
                url: "https://matrix.example.test/_matrix/client/v3/sync".to_string(),
                bearer_token: "token\ninjected".to_string(),
                body: None,
            })
            .expect_err("should reject newline in bearer token");
        assert!(err.to_string().contains("unsafe for curl config"));
    }

    #[tokio::test]
    async fn matrix_sdk_transport_rejects_empty_config() {
        let root = std::env::temp_dir().join(format!(
            "symbiotic_matrix_sdk_invalid_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        let config = MatrixSdkConfig {
            homeserver_url: String::new(),
            user_id: String::new(),
            password: String::new(),
            sync_timeout_ms: 10_000,
            data_dir: root,
            session_file: None,
            self_user_id: None,
            require_e2ee: true,
            stale_device_cleanup_days: 0,
            device_display_name: None,
        };
        assert!(MatrixSdkTransport::open(config).await.is_err());
    }
}
