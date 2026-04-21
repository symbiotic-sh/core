use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use credential_gateway::TokenEncryptor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;

use crate::{harden_dir_permissions, harden_file_permissions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushDevice {
    pub device_id: String,
    pub token_hash: String,
    /// Encrypted device token (nonce:ciphertext hex). Used for actual APNs/FCM delivery.
    pub encrypted_token: String,
    pub platform: String,
    pub last_seen: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushPriority {
    #[allow(dead_code)]
    Critical,
    High,
}

impl PushPriority {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::High => "high",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PushNotification {
    pub notification_id: String,
    pub device_id: String,
    pub token_hash: String,
    /// Encrypted device token (nonce:ciphertext hex). Decrypted by the push
    /// gateway at delivery time — never written to disk in plaintext.
    pub encrypted_token: String,
    pub platform: String,
    pub priority: String,
    pub title: String,
    pub body: String,
    pub rid: String,
    pub event_type: String,
    pub event_status: String,
    pub ts: u64,
    /// Thread ID for iOS notification grouping. Related notifications
    /// (same intake run, goal run, etc.) are grouped together.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    /// Badge count for the device. Incremented on each push, reset on ack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub badge: Option<u32>,
}

pub trait PushProvider: Send + Sync {
    fn send(&self, notification: &PushNotification) -> Result<()>;
}

pub(crate) struct FilePushProvider {
    outbox: PathBuf,
    lock: Mutex<()>,
}

impl FilePushProvider {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let outbox = path.as_ref().to_path_buf();
        if let Some(parent) = outbox.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create push outbox dir {}", parent.display())
            })?;
            harden_dir_permissions(parent, 0o700)?;
        }
        if !outbox.exists() {
            fs::File::create(&outbox)
                .with_context(|| format!("failed to create push outbox {}", outbox.display()))?;
        }
        harden_file_permissions(&outbox, 0o600)?;
        Ok(Self {
            outbox,
            lock: Mutex::new(()),
        })
    }
}

impl PushProvider for FilePushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("push outbox lock poisoned"))?;
        let payload = serde_json::to_string(notification)?;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.outbox)
            .with_context(|| format!("failed to open push outbox {}", self.outbox.display()))?;
        writeln!(file, "{payload}")
            .with_context(|| format!("failed to write push outbox {}", self.outbox.display()))?;
        Ok(())
    }
}

pub(crate) trait PushHttpClient: Send + Sync {
    fn post_json(&self, url: &str, body: &str, bearer_token: Option<&str>) -> Result<()>;
}

pub(crate) struct CurlPushHttpClient;

impl CurlPushHttpClient {
    pub(crate) fn is_available() -> bool {
        let mut cmd = std::process::Command::new("curl");
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

impl PushHttpClient for CurlPushHttpClient {
    fn post_json(&self, url: &str, body: &str, bearer_token: Option<&str>) -> Result<()> {
        use std::io::Write;
        use std::process::Stdio;

        let mut command = std::process::Command::new("curl");
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
            .arg("--max-time")
            .arg("20")
            .arg("-X")
            .arg("POST")
            .arg("-H")
            .arg("Content-Type: application/json")
            .arg("-d")
            .arg(body);
        if bearer_token.is_some() {
            command.arg("-K").arg("-"); // read bearer header from stdin
        }
        command.arg(url);

        if let Some(token) = bearer_token {
            if token.contains('\n') || token.contains('\r') {
                return Err(anyhow!("bearer token contains invalid control characters"));
            }
            command.stdin(Stdio::piped());
            let mut child = command
                .spawn()
                .with_context(|| format!("failed to spawn push gateway request to {url}"))?;
            if let Some(mut stdin) = child.stdin.take() {
                let config_line = format!("header = \"Authorization: Bearer {token}\"");
                let _ = stdin.write_all(config_line.as_bytes());
            }
            let output = child
                .wait_with_output()
                .with_context(|| format!("failed to execute push gateway request to {url}"))?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow!(
                "push gateway request failed (status={}): {}",
                output.status,
                stderr.trim()
            ));
        }

        let output = command
            .output()
            .with_context(|| format!("failed to execute push gateway request to {url}"))?;
        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(anyhow!(
            "push gateway request failed (status={}): {}",
            output.status,
            stderr.trim()
        ))
    }
}

pub(crate) struct HttpPushProvider {
    gateway_url: String,
    gateway_api_key: Option<String>,
    http_client: Arc<dyn PushHttpClient>,
    max_attempts: u8,
    base_backoff_ms: u64,
}

impl HttpPushProvider {
    pub(crate) fn new(
        gateway_url: String,
        gateway_api_key: Option<String>,
        http_client: Arc<dyn PushHttpClient>,
    ) -> Self {
        Self {
            gateway_url,
            gateway_api_key,
            http_client,
            max_attempts: 3,
            base_backoff_ms: if cfg!(test) { 0 } else { 200 },
        }
    }
}

impl PushProvider for HttpPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        let payload = serde_json::to_string(notification)?;
        send_push_with_retry(
            &*self.http_client,
            &self.gateway_url,
            self.gateway_api_key.as_deref(),
            &payload,
            self.max_attempts,
            self.base_backoff_ms,
            "push gateway delivery",
        )
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct PushGatewayEnvelope<'a> {
    provider: &'a str,
    notification: &'a PushNotification,
}

pub(crate) struct ApnsGatewayPushProvider {
    gateway_url: String,
    gateway_api_key: Option<String>,
    http_client: Arc<dyn PushHttpClient>,
    max_attempts: u8,
    base_backoff_ms: u64,
}

impl ApnsGatewayPushProvider {
    pub(crate) fn new(
        gateway_url: String,
        gateway_api_key: Option<String>,
        http_client: Arc<dyn PushHttpClient>,
    ) -> Self {
        Self {
            gateway_url,
            gateway_api_key,
            http_client,
            max_attempts: 3,
            base_backoff_ms: if cfg!(test) { 0 } else { 200 },
        }
    }
}

impl PushProvider for ApnsGatewayPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        if !notification.platform.eq_ignore_ascii_case("apns")
            && !notification.platform.eq_ignore_ascii_case("ios")
        {
            return Ok(());
        }
        let payload = serde_json::to_string(&PushGatewayEnvelope {
            provider: "apns",
            notification,
        })?;
        send_push_with_retry(
            &*self.http_client,
            &self.gateway_url,
            self.gateway_api_key.as_deref(),
            &payload,
            self.max_attempts,
            self.base_backoff_ms,
            "apns gateway delivery",
        )
    }
}

pub(crate) struct FcmGatewayPushProvider {
    gateway_url: String,
    gateway_api_key: Option<String>,
    http_client: Arc<dyn PushHttpClient>,
    max_attempts: u8,
    base_backoff_ms: u64,
}

impl FcmGatewayPushProvider {
    pub(crate) fn new(
        gateway_url: String,
        gateway_api_key: Option<String>,
        http_client: Arc<dyn PushHttpClient>,
    ) -> Self {
        Self {
            gateway_url,
            gateway_api_key,
            http_client,
            max_attempts: 3,
            base_backoff_ms: if cfg!(test) { 0 } else { 200 },
        }
    }
}

impl PushProvider for FcmGatewayPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        if !notification.platform.eq_ignore_ascii_case("fcm")
            && !notification.platform.eq_ignore_ascii_case("android")
        {
            return Ok(());
        }
        let payload = serde_json::to_string(&PushGatewayEnvelope {
            provider: "fcm",
            notification,
        })?;
        send_push_with_retry(
            &*self.http_client,
            &self.gateway_url,
            self.gateway_api_key.as_deref(),
            &payload,
            self.max_attempts,
            self.base_backoff_ms,
            "fcm gateway delivery",
        )
    }
}

fn send_push_with_retry(
    client: &dyn PushHttpClient,
    url: &str,
    bearer_token: Option<&str>,
    payload: &str,
    max_attempts: u8,
    base_backoff_ms: u64,
    label: &str,
) -> Result<()> {
    let mut errors = Vec::new();
    for attempt in 1..=max_attempts {
        match client.post_json(url, payload, bearer_token) {
            Ok(()) => return Ok(()),
            Err(err) => {
                errors.push(err.to_string());
                if attempt < max_attempts && base_backoff_ms > 0 {
                    let multiplier = 1u64 << (attempt - 1);
                    std::thread::sleep(std::time::Duration::from_millis(
                        base_backoff_ms.saturating_mul(multiplier),
                    ));
                }
            }
        }
    }
    Err(anyhow!(
        "{label} failed after {} attempts: {}",
        max_attempts,
        errors.join(" | ")
    ))
}

pub(crate) struct CompositePushProvider {
    providers: Vec<Arc<dyn PushProvider>>,
}

impl CompositePushProvider {
    pub(crate) fn new(providers: Vec<Arc<dyn PushProvider>>) -> Self {
        Self { providers }
    }
}

impl PushProvider for CompositePushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        let mut errors = Vec::new();
        for provider in &self.providers {
            if let Err(err) = provider.send(notification) {
                errors.push(err.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(anyhow!("push delivery failed: {}", errors.join(" | ")))
        }
    }
}

pub(crate) struct PushRegistry {
    path: PathBuf,
    devices: Mutex<HashMap<String, PushDevice>>,
    encryptor: TokenEncryptor,
    /// Per-device badge count. Incremented on each push, reset on ack.
    badge_counts: Mutex<HashMap<String, u32>>,
}

impl PushRegistry {
    pub(crate) fn open(path: impl AsRef<Path>, key_path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let encryptor = TokenEncryptor::open(key_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create push registry dir {}", parent.display())
            })?;
            harden_dir_permissions(parent, 0o700)?;
        }
        if !path.exists() {
            fs::File::create(&path)
                .with_context(|| format!("failed to create push registry {}", path.display()))?;
        }
        harden_file_permissions(&path, 0o600)?;
        let devices = load_push_registry(&path)?;
        Ok(Self {
            path,
            devices: Mutex::new(devices),
            encryptor,
            badge_counts: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn register(
        &self,
        device_id: &str,
        token: &str,
        platform: &str,
        now: u64,
    ) -> Result<PushDevice> {
        let mut devices = self
            .devices
            .lock()
            .map_err(|_| anyhow!("push registry lock poisoned"))?;
        let token_hash = hash_token(token);
        let encrypted_token = self.encryptor.encrypt(token)?;
        let record = PushDevice {
            device_id: device_id.to_string(),
            token_hash,
            encrypted_token,
            platform: platform.to_ascii_lowercase(),
            last_seen: now,
        };
        devices.insert(device_id.to_string(), record.clone());
        persist_push_registry(&self.path, devices.values())?;
        Ok(record)
    }

    pub(crate) fn list(&self) -> Result<Vec<PushDevice>> {
        let devices = self
            .devices
            .lock()
            .map_err(|_| anyhow!("push registry lock poisoned"))?;
        Ok(devices.values().cloned().collect())
    }

    /// Remove a device from the push registry.
    ///
    /// Returns `true` if the device was found and removed, `false` if not found.
    pub(crate) fn unregister(&self, device_id: &str) -> Result<bool> {
        let mut devices = self
            .devices
            .lock()
            .map_err(|_| anyhow!("push registry lock poisoned"))?;
        let existed = devices.remove(device_id).is_some();
        if existed {
            persist_push_registry(&self.path, devices.values())?;
            // Also clear badge count for the removed device.
            if let Ok(mut badges) = self.badge_counts.lock() {
                badges.remove(device_id);
            }
        }
        Ok(existed)
    }

    /// Increment the badge count for a device and return the new count.
    pub(crate) fn increment_badge(&self, device_id: &str) -> u32 {
        let mut badges = match self.badge_counts.lock() {
            Ok(b) => b,
            Err(_) => return 1,
        };
        let count = badges.entry(device_id.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    /// Reset the badge count for a device to zero.
    pub(crate) fn reset_badge(&self, device_id: &str) {
        if let Ok(mut badges) = self.badge_counts.lock() {
            badges.insert(device_id.to_string(), 0);
        }
    }

    /// Get the current badge count for a device.
    #[allow(dead_code)]
    pub(crate) fn badge_count(&self, device_id: &str) -> u32 {
        match self.badge_counts.lock() {
            Ok(badges) => badges.get(device_id).copied().unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Decrypt an encrypted device token for delivery.
    /// Used by push gateway providers at send time and in tests.
    #[allow(dead_code)]
    pub(crate) fn decrypt_token(&self, encrypted_token: &str) -> Result<String> {
        self.encryptor.decrypt(encrypted_token)
    }

    /// Remove stale devices whose `last_seen` is older than `max_age_secs`
    /// relative to `now`.
    ///
    /// Returns the number of devices pruned. Typically called periodically
    /// (e.g. daily) to clean up devices that have not re-registered.
    #[allow(dead_code)]
    pub(crate) fn prune_stale(&self, now: u64, max_age_secs: u64) -> Result<usize> {
        let mut devices = self
            .devices
            .lock()
            .map_err(|_| anyhow!("push registry lock poisoned"))?;
        let cutoff = now.saturating_sub(max_age_secs);
        let stale_ids: Vec<String> = devices
            .values()
            .filter(|d| d.last_seen < cutoff)
            .map(|d| d.device_id.clone())
            .collect();
        let count = stale_ids.len();
        if count == 0 {
            return Ok(0);
        }
        for id in &stale_ids {
            devices.remove(id);
        }
        persist_push_registry(&self.path, devices.values())?;
        // Clear badge counts for pruned devices.
        if let Ok(mut badges) = self.badge_counts.lock() {
            for id in &stale_ids {
                badges.remove(id);
            }
        }
        tracing::info!(
            pruned = count,
            cutoff_epoch = cutoff,
            "push_registry: pruned stale devices"
        );
        Ok(count)
    }

    /// Remove a device by its token hash.
    ///
    /// Used when APNs returns a 410 (GONE) response, indicating the device
    /// token is no longer valid. Returns `true` if a matching device was found
    /// and removed.
    pub(crate) fn remove_by_token_hash(&self, token_hash: &str) -> Result<bool> {
        let mut devices = self
            .devices
            .lock()
            .map_err(|_| anyhow!("push registry lock poisoned"))?;
        let maybe_id = devices
            .values()
            .find(|d| d.token_hash == token_hash)
            .map(|d| d.device_id.clone());
        if let Some(id) = maybe_id {
            devices.remove(&id);
            persist_push_registry(&self.path, devices.values())?;
            if let Ok(mut badges) = self.badge_counts.lock() {
                badges.remove(&id);
            }
            tracing::info!(
                device_id = %id,
                "push_registry: removed device with invalid token (APNs 410)"
            );
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Return the number of registered devices.
    #[allow(dead_code)]
    pub(crate) fn device_count(&self) -> usize {
        self.devices.lock().map(|d| d.len()).unwrap_or(0)
    }
}

pub(crate) fn init_push_telemetry_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create push telemetry dir {}", parent.display()))?;
        harden_dir_permissions(parent, 0o700)?;
    }
    if !path.exists() {
        fs::File::create(path)
            .with_context(|| format!("failed to create push telemetry file {}", path.display()))?;
    }
    harden_file_permissions(path, 0o600)?;
    Ok(())
}

pub(crate) fn append_push_telemetry(
    path: &Path,
    notification: &PushNotification,
    status: &str,
    error: Option<&str>,
) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open push telemetry file {}", path.display()))?;
    let payload = serde_json::json!({
        "ts": notification.ts,
        "notification_id": notification.notification_id,
        "device_id": notification.device_id,
        "platform": notification.platform,
        "rid": notification.rid,
        "event_type": notification.event_type,
        "event_status": notification.event_status,
        "status": status,
        "error": error.unwrap_or(""),
    });
    writeln!(file, "{payload}")
        .with_context(|| format!("failed to write push telemetry file {}", path.display()))?;
    Ok(())
}

fn load_push_registry(path: &Path) -> Result<HashMap<String, PushDevice>> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read push registry {}", path.display()))?;
    let mut devices = HashMap::new();
    for line in raw.lines() {
        if let Some(device) = parse_push_registry_line(line) {
            devices.insert(device.device_id.clone(), device);
        }
    }
    Ok(devices)
}

fn parse_push_registry_line(line: &str) -> Option<PushDevice> {
    let parts = line.split('\t').collect::<Vec<_>>();
    // 5-column format: device_id, token_hash, encrypted_token, platform, last_seen
    // Legacy 4-column format (no encrypted_token) is rejected — re-registration required.
    if parts.len() < 5 {
        return None;
    }
    let device_id = parts[0].trim().to_string();
    let token_hash = parts[1].trim().to_string();
    let encrypted_token = parts[2].trim().to_string();
    let platform = parts[3].trim().to_string();
    let last_seen = parts[4].trim().parse::<u64>().ok()?;
    if device_id.is_empty()
        || token_hash.is_empty()
        || encrypted_token.is_empty()
        || platform.is_empty()
    {
        return None;
    }
    Some(PushDevice {
        device_id,
        token_hash,
        encrypted_token,
        platform,
        last_seen,
    })
}

fn persist_push_registry<'a>(
    path: &Path,
    devices: impl Iterator<Item = &'a PushDevice>,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create push registry dir {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("failed to write push registry {}", tmp.display()))?;
    harden_file_permissions(&tmp, 0o600)?;
    for device in devices {
        writeln!(
            file,
            "{}\t{}\t{}\t{}\t{}",
            device.device_id,
            device.token_hash,
            device.encrypted_token,
            device.platform,
            device.last_seen
        )
        .with_context(|| format!("failed to write push registry {}", tmp.display()))?;
    }
    file.flush()
        .with_context(|| format!("failed to flush push registry {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed to replace push registry {}", path.display()))?;
    harden_file_permissions(path, 0o600)?;
    Ok(())
}

pub(crate) fn record_push_ack(
    path: &Path,
    notification_id: &str,
    run_id: Option<&str>,
    now: u64,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create push ack dir {}", parent.display()))?;
        harden_dir_permissions(parent, 0o700)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open push ack file {}", path.display()))?;
    harden_file_permissions(path, 0o600)?;
    let run_id = run_id.unwrap_or("");
    writeln!(file, "{notification_id}\t{run_id}\t{now}")
        .with_context(|| format!("failed to write push ack {}", path.display()))?;
    Ok(())
}

pub(crate) fn push_priority_for_event(envelope: &MatrixEventEnvelope) -> Option<PushPriority> {
    // State events used for push/routing bookkeeping should never trigger push
    if envelope.sym.k == Kind::State {
        let action = envelope.sym.a.as_deref().unwrap_or("");
        if action.starts_with("push.") {
            return None;
        }
        // Alert state events get high priority
        if action.starts_with("alert.") {
            return Some(PushPriority::High);
        }
        return None;
    }
    // Failed events get high priority
    if envelope.sym.s == Some(Status::Fail) {
        return Some(PushPriority::High);
    }
    // Questions/awaiting events get high priority (user action needed)
    if envelope.sym.k == Kind::Question || envelope.sym.s == Some(Status::Awaiting) {
        return Some(PushPriority::High);
    }
    // Notifications always get pushed
    if envelope.sym.k == Kind::Notification {
        return Some(PushPriority::High);
    }
    None
}

pub(crate) fn push_title_for_event(envelope: &MatrixEventEnvelope) -> String {
    match envelope.sym.k {
        Kind::Question => "Action needed".to_string(),
        Kind::Notification => "Notification".to_string(),
        Kind::State => {
            let action = envelope.sym.a.as_deref().unwrap_or("");
            if action.starts_with("alert.") {
                "Alert".to_string()
            } else {
                "Symbiotic update".to_string()
            }
        }
        Kind::Message => {
            if envelope.sym.s == Some(Status::Fail) {
                "Something went wrong".to_string()
            } else {
                "Symbiotic update".to_string()
            }
        }
    }
}

pub(crate) fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// Push notification preferences
// ---------------------------------------------------------------------------

/// User-configurable push notification preferences.
///
/// Stored in TOML at `data/push/preferences.toml` alongside the token registry.
/// `auth_required` and `alert_escalations` are always-on and cannot be disabled
/// by the user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PushPreferences {
    /// Push for auth-required events (always on, NOT user-overridable).
    pub auth_required: bool,
    /// Push for failure/DLQ events.
    pub failures: bool,
    /// Push for goal/workflow completions.
    pub goal_completions: bool,
    /// Push for entry capture confirmations.
    pub capture_confirmations: bool,
    /// Push for install lifecycle events.
    pub install_progress: bool,
    /// Push for alert escalations (always on, NOT user-overridable).
    pub alert_escalations: bool,
}

impl Default for PushPreferences {
    fn default() -> Self {
        Self {
            auth_required: true,
            failures: true,
            goal_completions: true,
            capture_confirmations: true,
            install_progress: true,
            alert_escalations: true,
        }
    }
}

impl PushPreferences {
    /// Load preferences from a TOML file. Creates defaults if the file is
    /// missing or unparseable.
    pub(crate) fn load(path: &Path) -> Self {
        if !path.exists() {
            let prefs = Self::default();
            let _ = prefs.save(path);
            return prefs;
        }
        match fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist preferences to a TOML file.
    pub(crate) fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create push preferences dir {}", parent.display())
            })?;
            harden_dir_permissions(parent, 0o700)?;
        }
        let raw =
            toml::to_string_pretty(self).with_context(|| "failed to serialize push preferences")?;
        fs::write(path, raw)
            .with_context(|| format!("failed to write push preferences {}", path.display()))?;
        harden_file_permissions(path, 0o600)?;
        Ok(())
    }

    /// Enforce non-overridable fields: `auth_required` and
    /// `alert_escalations` are always true regardless of user input.
    pub(crate) fn enforce_invariants(&mut self) {
        self.auth_required = true;
        self.alert_escalations = true;
    }

    /// Check whether a given event type + status combination is enabled
    /// by these preferences.
    pub(crate) fn is_event_enabled(&self, event_type: &str, _status: &str) -> bool {
        // Auth events are always enabled.
        if event_type.starts_with("auth.") {
            return true;
        }
        // Alert escalations are always enabled.
        if event_type == "alert.escalation" || event_type == "alert.received" {
            return true;
        }
        // Failures/DLQ.
        if event_type.ends_with(".failed") || event_type == "job.unknown" || _status == "dlq" {
            return self.failures;
        }
        // Goal/workflow completions.
        if event_type == "workflow.run" && _status == "completed" {
            return self.goal_completions;
        }
        // Capture confirmations.
        if event_type == "ingest.fetch" && _status == "completed" {
            return self.capture_confirmations;
        }
        // Install events.
        if event_type.starts_with("install.") {
            return self.install_progress;
        }
        // Default: allow (don't suppress unrecognised events).
        true
    }
}

// ---------------------------------------------------------------------------
// Real gateway bridge providers (symbiotic-push crate integration)
// ---------------------------------------------------------------------------
//
// The `symbiotic-push` crate provides async `PushGateway` implementations
// for APNs (ES256 JWT auth) and FCM (OAuth2 service account auth).
// The daemon uses a synchronous `PushProvider` trait.  These bridge structs
// wrap the async gateways and convert between the two notification types,
// using `tokio::runtime::Handle::block_on` from a scoped thread to avoid
// blocking the async runtime.

use symbiotic_push::gateway::{ApnsConfig, ApnsGateway, FcmConfig, FcmGateway, PushGateway};

/// Bridge that wraps the async `ApnsGateway` from `symbiotic-push` into
/// the daemon's synchronous `PushProvider` trait.
pub(crate) struct RealApnsPushProvider {
    gateway: ApnsGateway,
    push_registry: Arc<PushRegistry>,
}

impl RealApnsPushProvider {
    pub(crate) fn new(config: ApnsConfig, push_registry: Arc<PushRegistry>) -> Self {
        Self {
            gateway: ApnsGateway::new(config),
            push_registry,
        }
    }
}

impl PushProvider for RealApnsPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        // Only handle APNs (iOS) notifications.
        if !notification.platform.eq_ignore_ascii_case("apns")
            && !notification.platform.eq_ignore_ascii_case("ios")
        {
            return Ok(());
        }

        // Decrypt the device token for delivery.
        let device_token = self
            .push_registry
            .decrypt_token(&notification.encrypted_token)
            .with_context(|| {
                format!(
                    "failed to decrypt push token for device {}",
                    notification.device_id
                )
            })?;

        // Convert daemon notification to crate notification.
        let crate_notification = symbiotic_push::types::PushNotification::new(
            &notification.title,
            &notification.body,
            symbiotic_push::types::NotificationCategory::System,
        )
        .with_token(&device_token)
        .with_priority(if notification.priority == "critical" {
            symbiotic_push::types::PushPriority::High
        } else {
            symbiotic_push::types::PushPriority::Normal
        })
        .with_data("event_type", &notification.event_type)
        .with_data("event_status", &notification.event_status)
        .with_data("rid", &notification.rid);

        // Bridge async to sync: spawn a scoped thread that calls block_on
        // to avoid deadlocking the current tokio runtime thread.
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(|| handle.block_on(self.gateway.send(&crate_notification)))
                    .join()
                    .expect("APNs gateway thread should not panic")
            }),
            Err(_) => {
                return Err(anyhow!("no tokio runtime available for APNs gateway send"));
            }
        };

        match result {
            Ok(response) if response.success => Ok(()),
            Ok(response) => {
                // Auto-remove device if APNs says token is invalid (410 GONE).
                if response.token_invalid {
                    tracing::warn!(
                        device_id = %notification.device_id,
                        token_hash = %notification.token_hash,
                        "APNs: token invalid (410), removing device from registry"
                    );
                    let _ = self
                        .push_registry
                        .remove_by_token_hash(&notification.token_hash);
                }
                Err(anyhow!(
                    "APNs delivery failed: {}",
                    response
                        .error_reason
                        .unwrap_or_else(|| "unknown".to_string())
                ))
            }
            Err(e) => Err(anyhow!("APNs gateway error: {e}")),
        }
    }
}

/// Bridge that wraps the async `FcmGateway` from `symbiotic-push` into
/// the daemon's synchronous `PushProvider` trait.
pub(crate) struct RealFcmPushProvider {
    gateway: FcmGateway,
    push_registry: Arc<PushRegistry>,
}

impl RealFcmPushProvider {
    pub(crate) fn new(config: FcmConfig, push_registry: Arc<PushRegistry>) -> Self {
        Self {
            gateway: FcmGateway::new(config),
            push_registry,
        }
    }
}

impl PushProvider for RealFcmPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        // Only handle FCM (Android) notifications.
        if !notification.platform.eq_ignore_ascii_case("fcm")
            && !notification.platform.eq_ignore_ascii_case("android")
        {
            return Ok(());
        }

        // Decrypt the device token for delivery.
        let device_token = self
            .push_registry
            .decrypt_token(&notification.encrypted_token)
            .with_context(|| {
                format!(
                    "failed to decrypt push token for device {}",
                    notification.device_id
                )
            })?;

        // Convert daemon notification to crate notification.
        let crate_notification = symbiotic_push::types::PushNotification::new(
            &notification.title,
            &notification.body,
            symbiotic_push::types::NotificationCategory::System,
        )
        .with_token(&device_token)
        .with_priority(if notification.priority == "critical" {
            symbiotic_push::types::PushPriority::High
        } else {
            symbiotic_push::types::PushPriority::Normal
        })
        .with_data("event_type", &notification.event_type)
        .with_data("event_status", &notification.event_status)
        .with_data("rid", &notification.rid);

        // Bridge async to sync.
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(|| handle.block_on(self.gateway.send(&crate_notification)))
                    .join()
                    .expect("FCM gateway thread should not panic")
            }),
            Err(_) => {
                return Err(anyhow!("no tokio runtime available for FCM gateway send"));
            }
        };

        match result {
            Ok(response) if response.success => Ok(()),
            Ok(response) => {
                // Auto-remove device if FCM says token is invalid (404 NOT FOUND).
                if response.token_invalid {
                    tracing::warn!(
                        device_id = %notification.device_id,
                        token_hash = %notification.token_hash,
                        "FCM: token invalid, removing device from registry"
                    );
                    let _ = self
                        .push_registry
                        .remove_by_token_hash(&notification.token_hash);
                }
                Err(anyhow!(
                    "FCM delivery failed: {}",
                    response
                        .error_reason
                        .unwrap_or_else(|| "unknown".to_string())
                ))
            }
            Err(e) => Err(anyhow!("FCM gateway error: {e}")),
        }
    }
}

/// Bridge that wraps the async `PushGatewayClient` (managed mode, calls
/// `push.symbiotic.sh`) into the daemon's synchronous `PushProvider` trait.
///
/// In managed mode, the Symbiotic push gateway holds APNs/FCM credentials
/// and dispatches to Apple/Google on behalf of self-hosted daemons.
/// Only notification metadata is sent (category, priority, badge) — message
/// content is never included.
///
/// Configured via env vars: `SYMBIOTIC_PUSH_GATEWAY_URL` +
/// `SYMBIOTIC_RELAY_DAEMON_TOKEN`.
pub(crate) struct ManagedGatewayPushProvider {
    client: crate::push_client::PushGatewayClient,
}

impl ManagedGatewayPushProvider {
    pub(crate) fn new(config: crate::push_client::PushGatewayConfig) -> Self {
        Self {
            client: crate::push_client::PushGatewayClient::new(config),
        }
    }
}

impl PushProvider for ManagedGatewayPushProvider {
    fn send(&self, notification: &PushNotification) -> Result<()> {
        // Determine gateway platform string from notification platform.
        let platform = if notification.platform.eq_ignore_ascii_case("apns")
            || notification.platform.eq_ignore_ascii_case("ios")
        {
            "apns"
        } else if notification.platform.eq_ignore_ascii_case("fcm")
            || notification.platform.eq_ignore_ascii_case("android")
        {
            "fcm"
        } else {
            // Unknown platform — skip silently (other providers may handle it).
            return Ok(());
        };

        let priority = if notification.priority == "critical" {
            "high"
        } else {
            "normal"
        };

        // Map daemon event_type to a gateway notification category.
        let category = match notification.event_type.as_str() {
            s if s.starts_with("ingest.") || s.starts_with("archive.") => "brief_ready",
            s if s.starts_with("goal.") => "goal_progress",
            s if s.starts_with("auth.") || s.starts_with("install.") => "action_required",
            _ => "system",
        };

        // Bridge async to sync: spawn a scoped thread that calls block_on.
        let result = match tokio::runtime::Handle::try_current() {
            Ok(handle) => std::thread::scope(|s| {
                s.spawn(|| {
                    handle.block_on(self.client.notify(
                        &notification.device_id,
                        platform,
                        &notification.encrypted_token,
                        category,
                        priority,
                        notification.badge,
                    ))
                })
                .join()
                .expect("managed gateway push thread should not panic")
            }),
            Err(_) => {
                return Err(anyhow!(
                    "no tokio runtime available for managed push gateway send"
                ));
            }
        };

        result.map_err(|e| anyhow!("managed push gateway error: {e}"))
    }
}

/// Build the composite push provider from daemon configuration.
///
/// Always includes `FilePushProvider` as a fallback (audit trail).
/// Optionally adds:
/// - `ManagedGatewayPushProvider` (Symbiotic managed push gateway) if env configured
/// - `HttpPushProvider` (generic HTTP push gateway) if `push_gateway_url` is set
/// - `ApnsGatewayPushProvider` (curl-based APNs proxy) if `push_apns_gateway_url` is set
/// - `FcmGatewayPushProvider` (curl-based FCM proxy) if `push_fcm_gateway_url` is set
/// - `RealApnsPushProvider` (real APNs via ES256 JWT) if APNs credentials are set
/// - `RealFcmPushProvider` (real FCM via OAuth2) if FCM credentials are set
pub(crate) fn build_push_provider(
    config: &crate::DaemonConfig,
    push_registry: &Arc<PushRegistry>,
) -> Result<Arc<dyn PushProvider>> {
    let mut providers: Vec<Arc<dyn PushProvider>> =
        vec![Arc::new(FilePushProvider::open(&config.push_outbox_file)?)];

    // Managed push gateway (push.symbiotic.sh) — env-configured, reqwest-based.
    // Must be checked before clearing relay env vars in main.rs.
    if let Some(gateway_config) = crate::push_client::PushGatewayConfig::from_env() {
        tracing::info!(
            gateway_url = %gateway_config.gateway_url,
            "push: enabling managed push gateway (push.symbiotic.sh)"
        );
        providers.push(Arc::new(ManagedGatewayPushProvider::new(gateway_config)));
    }

    // Curl-based HTTP push providers (legacy gateway proxies).
    if CurlPushHttpClient::is_available() {
        let shared_http = Arc::new(CurlPushHttpClient);
        if let Some(url) = config
            .push_gateway_url
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            providers.push(Arc::new(HttpPushProvider::new(
                url,
                config.push_gateway_api_key.clone(),
                shared_http.clone(),
            )));
        }
        if let Some(url) = config
            .push_apns_gateway_url
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            providers.push(Arc::new(ApnsGatewayPushProvider::new(
                url,
                config.push_apns_gateway_api_key.clone(),
                shared_http.clone(),
            )));
        }
        if let Some(url) = config
            .push_fcm_gateway_url
            .clone()
            .filter(|value| !value.trim().is_empty())
        {
            providers.push(Arc::new(FcmGatewayPushProvider::new(
                url,
                config.push_fcm_gateway_api_key.clone(),
                shared_http,
            )));
        }
    }

    // Real APNs gateway (ES256 JWT, direct to APNs).
    if let (Some(team_id), Some(key_id), Some(private_key_pem)) = (
        config.push_apns_team_id.clone(),
        config.push_apns_key_id.clone(),
        config.push_apns_private_key_pem.clone(),
    ) {
        tracing::info!(
            team_id = %team_id,
            key_id = %key_id,
            sandbox = config.push_apns_sandbox,
            "push: enabling real APNs gateway (ES256 JWT auth)"
        );
        providers.push(Arc::new(RealApnsPushProvider::new(
            ApnsConfig {
                team_id,
                key_id,
                private_key_pem,
                sandbox: config.push_apns_sandbox,
                base_url_override: None,
            },
            push_registry.clone(),
        )));
    }

    // Real FCM gateway (OAuth2 service account auth, direct to FCM).
    if let (Some(project_id), Some(service_account_email), Some(private_key_pem)) = (
        config.push_fcm_project_id.clone(),
        config.push_fcm_service_account_email.clone(),
        config.push_fcm_private_key_pem.clone(),
    ) {
        tracing::info!(
            project_id = %project_id,
            service_account_email = %service_account_email,
            "push: enabling real FCM gateway (OAuth2 service account auth)"
        );
        providers.push(Arc::new(RealFcmPushProvider::new(
            FcmConfig {
                project_id,
                service_account_email,
                private_key_pem,
                api_url_override: None,
                token_url_override: None,
            },
            push_registry.clone(),
        )));
    }

    Ok(Arc::new(CompositePushProvider::new(providers)))
}
