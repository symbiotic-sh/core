pub mod auth_engine;
pub mod bloom;
pub mod git_push_session;
pub mod oauth;
pub mod oauth_x;
pub mod script_registry;
pub mod totp;

pub use git_push_session::{
    with_git_push_session, GitCredentialKind, GitPushEnv, GitPushSessionError,
};

/// Well-known vault key constants for daemon bootstrap credentials.
/// Used by the bootstrap orchestrator to store/retrieve Matrix auth
/// and room IDs in the encrypted credential vault.
pub const VAULT_KEY_MATRIX_PASSWORD: &str = "matrix.password";
pub const VAULT_KEY_MATRIX_ROOM_CONTROL: &str = "matrix.room.control";
pub const VAULT_KEY_MATRIX_ROOM_INTAKE: &str = "matrix.room.intake";
pub const VAULT_KEY_MATRIX_ROOM_ALERTS: &str = "matrix.room.alerts";
pub const VAULT_KEY_MATRIX_ROOM_STATUS: &str = "matrix.room.status";
pub const VAULT_KEY_MATRIX_ROOM_CREDENTIALS: &str = "matrix.room.credentials";
pub const VAULT_KEY_MATRIX_ROOM_GOALS: &str = "matrix.room.goals";
pub const VAULT_KEY_MATRIX_ROOM_STREAM: &str = "matrix.room.stream";

use anyhow::{anyhow, Context, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Key, Nonce,
};
use rand::TryRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use symbiotic_core::{harden_dir_permissions, harden_file_permissions};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionType {
    Browser,
    Api,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPolicy {
    pub exportable: bool,
    pub requires_reauth: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandle {
    pub handle_id: String,
    pub issued_at: u64,
    pub expires_at: u64,
    pub scope: HashSet<String>,
    pub target: String,
    pub session_type: SessionType,
    pub policy: SessionPolicy,
    pub revoked: bool,
}

#[derive(Debug, Clone)]
pub struct AuthRequest {
    pub target: String,
    pub scopes: Vec<String>,
    pub session_type: SessionType,
    pub policy: SessionPolicy,
}

#[derive(Clone)]
pub struct CredentialRecord {
    pub service: String,
    pub username: String,
    pub secret: String,
    pub totp_secret: Option<String>,
}

impl std::fmt::Debug for CredentialRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialRecord")
            .field("service", &self.service)
            .field("username", &self.username)
            .field("secret", &"[REDACTED]")
            .field(
                "totp_secret",
                &self.totp_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct GatewayConfig {
    pub default_ttl_secs: u64,
    pub blocked_targets: HashSet<String>,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            default_ttl_secs: 24 * 60 * 60,
            blocked_targets: HashSet::new(),
        }
    }
}

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("target is blocked: {0}")]
    BlockedTarget(String),
    #[error("target is unsafe: {0}")]
    UnsafeTarget(String),
    #[error("missing credentials for target: {0}")]
    MissingCredentials(String),
    #[error("session handle not found: {0}")]
    HandleNotFound(String),
    #[error("session handle expired: {0}")]
    HandleExpired(String),
    #[error("session handle revoked: {0}")]
    HandleRevoked(String),
    #[error("scope not allowed: {0}")]
    ScopeDenied(String),
    #[error("target mismatch")]
    TargetMismatch,
    #[error("export not allowed without explicit approval")]
    ExportDenied,
}

pub trait ThreatChecker: Send + Sync {
    fn is_safe(&self, target: &str) -> Result<bool>;
}

pub trait CredentialVault: Send + Sync {
    fn put(&self, credential: CredentialRecord) -> Result<()>;
    fn get(&self, service: &str) -> Result<Option<CredentialRecord>>;
    fn delete(&self, service: &str) -> Result<bool>;
    fn list_services(&self) -> Result<Vec<String>>;
}

pub struct StaticThreatChecker {
    denied: HashSet<String>,
}

impl StaticThreatChecker {
    pub fn new(denied: HashSet<String>) -> Self {
        Self { denied }
    }
}

impl ThreatChecker for StaticThreatChecker {
    fn is_safe(&self, target: &str) -> Result<bool> {
        Ok(!self.denied.contains(&target.to_ascii_lowercase()))
    }
}

pub struct FileCredentialVault {
    file_path: PathBuf,
    key: [u8; 32],
    lock: Mutex<()>,
}

impl FileCredentialVault {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create vault directory {}", parent.display())
            })?;
            harden_dir_permissions(parent, 0o700)?;
        }
        if !path.exists() {
            fs::File::create(&path)
                .with_context(|| format!("failed to create vault file {}", path.display()))?;
        }
        harden_file_permissions(&path, 0o600)?;
        let key_path = path.with_extension(format!(
            "{}.key",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("vault")
        ));
        let key = load_or_create_vault_key(&key_path)?;

        // One-time migration from plaintext/legacy envelope to svlt2 AEAD envelope.
        let current = fs::read_to_string(&path)
            .with_context(|| format!("failed to read vault file {}", path.display()))?;
        if !current.trim().is_empty() {
            let records = load_vault_file(&path, &key)?;
            if should_rewrite_to_vault_v2(&current) {
                persist_vault_file(&path, &key, &records)?;
            }
        }

        Ok(Self {
            file_path: path,
            key,
            lock: Mutex::new(()),
        })
    }
}

fn should_rewrite_to_vault_v2(content: &str) -> bool {
    if content.trim().is_empty() {
        return false;
    }
    if !looks_like_encrypted_envelope(content) {
        return true;
    }
    match serde_json::from_str::<VaultEnvelope>(content) {
        Ok(envelope) => envelope.version != "svlt2",
        Err(_) => false,
    }
}

impl CredentialVault for FileCredentialVault {
    fn put(&self, credential: CredentialRecord) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("credential vault lock poisoned"))?;
        let mut existing = load_vault_file(&self.file_path, &self.key)?;
        existing.insert(credential.service.clone(), credential);
        persist_vault_file(&self.file_path, &self.key, &existing)?;
        Ok(())
    }

    fn get(&self, service: &str) -> Result<Option<CredentialRecord>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("credential vault lock poisoned"))?;
        let existing = load_vault_file(&self.file_path, &self.key)?;
        Ok(existing.get(service).cloned())
    }

    fn delete(&self, service: &str) -> Result<bool> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("credential vault lock poisoned"))?;
        let mut existing = load_vault_file(&self.file_path, &self.key)?;
        let found = existing.remove(service).is_some();
        if found {
            persist_vault_file(&self.file_path, &self.key, &existing)?;
        }
        Ok(found)
    }

    fn list_services(&self) -> Result<Vec<String>> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow!("credential vault lock poisoned"))?;
        let existing = load_vault_file(&self.file_path, &self.key)?;
        let mut services: Vec<String> = existing.keys().cloned().collect();
        services.sort();
        Ok(services)
    }
}

/// Per-goal namespace vault sandboxing errors.
#[derive(Debug, Error)]
pub enum GoalVaultError {
    #[error("invalid goal slug: {0}")]
    InvalidSlug(String),
    #[error("goal scope mismatch: requested {requested:?}, session has {session:?}")]
    ScopeMismatch {
        requested: Option<String>,
        session: Option<String>,
    },
}

/// Validates that a goal slug is safe for use as a directory name.
/// Allows lowercase alphanumeric and hyphens, 1-64 characters.
fn validate_goal_slug(slug: &str) -> Result<()> {
    if slug.is_empty() || slug.len() > 64 {
        return Err(GoalVaultError::InvalidSlug(slug.to_string()).into());
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(GoalVaultError::InvalidSlug(slug.to_string()).into());
    }
    if slug.starts_with('-') || slug.ends_with('-') || slug.contains("--") {
        return Err(GoalVaultError::InvalidSlug(slug.to_string()).into());
    }
    Ok(())
}

/// A goal-scoped credential vault that namespaces credential storage by goal ID.
///
/// Storage layout:
/// - `{base_dir}/global/vault.tsv`      -- global (non-goal) credentials
/// - `{base_dir}/{goal-slug}/vault.tsv`  -- goal-specific credentials
///
/// Each namespace has its own encryption key and vault file.
pub struct GoalScopedVault {
    base_dir: PathBuf,
    vaults: Mutex<HashMap<String, Arc<FileCredentialVault>>>,
}

impl GoalScopedVault {
    /// Open a goal-scoped vault rooted at `base_dir`.
    /// Creates the base directory and the global namespace if they do not exist.
    pub fn open(base_dir: impl AsRef<Path>) -> Result<Self> {
        let base_dir = base_dir.as_ref().to_path_buf();
        fs::create_dir_all(&base_dir).with_context(|| {
            format!(
                "failed to create vault base directory {}",
                base_dir.display()
            )
        })?;
        harden_dir_permissions(&base_dir, 0o700)?;

        let instance = Self {
            base_dir,
            vaults: Mutex::new(HashMap::new()),
        };
        // Eagerly open the global vault so it's ready.
        instance.vault_for_scope(None)?;
        Ok(instance)
    }

    /// Get or create the `FileCredentialVault` for a given goal scope.
    /// `None` maps to the "global" namespace.
    pub fn vault_for_scope(&self, goal_scope: Option<&str>) -> Result<Arc<FileCredentialVault>> {
        let namespace = match goal_scope {
            Some(slug) => {
                validate_goal_slug(slug)?;
                slug.to_string()
            }
            None => "global".to_string(),
        };

        let mut vaults = self
            .vaults
            .lock()
            .map_err(|_| anyhow!("goal vault registry lock poisoned"))?;

        if let Some(vault) = vaults.get(&namespace) {
            return Ok(Arc::clone(vault));
        }

        let vault_dir = self.base_dir.join(&namespace);
        fs::create_dir_all(&vault_dir).with_context(|| {
            format!(
                "failed to create goal vault directory {}",
                vault_dir.display()
            )
        })?;
        harden_dir_permissions(&vault_dir, 0o700)?;

        let vault_path = vault_dir.join("vault.tsv");
        let vault = Arc::new(FileCredentialVault::open(&vault_path)?);
        vaults.insert(namespace, Arc::clone(&vault));
        Ok(vault)
    }

    /// Store a credential in a goal-scoped namespace.
    pub fn put_scoped(&self, goal_scope: Option<&str>, credential: CredentialRecord) -> Result<()> {
        let vault = self.vault_for_scope(goal_scope)?;
        vault.put(credential)
    }

    /// Retrieve a credential from a goal-scoped namespace.
    pub fn get_scoped(
        &self,
        goal_scope: Option<&str>,
        service: &str,
    ) -> Result<Option<CredentialRecord>> {
        let vault = self.vault_for_scope(goal_scope)?;
        vault.get(service)
    }

    /// Provision a credential into a specific goal's namespace.
    ///
    /// This is used for JIT credential provisioning: copying or creating
    /// a credential scoped to a specific goal so that goal's agents can
    /// access it without touching global or other goal namespaces.
    pub fn provision_for_goal(
        &self,
        goal_slug: &str,
        credential_id: &str,
        encrypted_data: &[u8],
    ) -> Result<()> {
        validate_goal_slug(goal_slug)?;
        let vault = self.vault_for_scope(Some(goal_slug))?;
        vault.put(CredentialRecord {
            service: credential_id.to_string(),
            username: format!("provisioned:{goal_slug}"),
            secret: String::from_utf8_lossy(encrypted_data).to_string(),
            totp_secret: None,
        })
    }
}

impl CredentialVault for GoalScopedVault {
    fn put(&self, credential: CredentialRecord) -> Result<()> {
        self.put_scoped(None, credential)
    }

    fn get(&self, service: &str) -> Result<Option<CredentialRecord>> {
        self.get_scoped(None, service)
    }

    fn delete(&self, service: &str) -> Result<bool> {
        let vault = self.vault_for_scope(None)?;
        vault.delete(service)
    }

    fn list_services(&self) -> Result<Vec<String>> {
        let vault = self.vault_for_scope(None)?;
        vault.list_services()
    }
}

pub struct CredentialGateway {
    config: GatewayConfig,
    checker: Arc<dyn ThreatChecker>,
    vault: Arc<dyn CredentialVault>,
    scoped_vault: Option<Arc<GoalScopedVault>>,
    handles: Mutex<HashMap<String, SessionHandle>>,
}

impl CredentialGateway {
    pub fn new(
        config: GatewayConfig,
        checker: Arc<dyn ThreatChecker>,
        vault: Arc<dyn CredentialVault>,
    ) -> Self {
        Self {
            config,
            checker,
            vault,
            scoped_vault: None,
            handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn new_scoped(
        config: GatewayConfig,
        checker: Arc<dyn ThreatChecker>,
        vault: Arc<GoalScopedVault>,
    ) -> Self {
        Self {
            config,
            checker,
            vault: vault.clone(),
            scoped_vault: Some(vault),
            handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn put_credential(&self, mut credential: CredentialRecord) -> Result<()> {
        credential.service = normalize_target_host(&credential.service)?;
        self.vault.put(credential)
    }

    pub fn get_credential(&self, service: &str) -> Result<Option<CredentialRecord>> {
        let normalized = normalize_target_host(service)?;
        self.vault.get(&normalized)
    }

    pub fn delete_credential(&self, service: &str) -> Result<bool> {
        let normalized = normalize_target_host(service)?;
        self.vault.delete(&normalized)
    }

    pub fn list_credential_services(&self) -> Result<Vec<String>> {
        self.vault.list_services()
    }

    pub fn issue_session_handle(&self, request: AuthRequest, now: u64) -> Result<SessionHandle> {
        self.issue_session_handle_scoped(None, request, now)
    }

    pub fn issue_session_handle_scoped(
        &self,
        goal_scope: Option<&str>,
        request: AuthRequest,
        now: u64,
    ) -> Result<SessionHandle> {
        let target = normalize_target_host(&request.target)?;
        if self.config.blocked_targets.contains(&target) {
            return Err(GatewayError::BlockedTarget(target).into());
        }
        if is_reserved_target(&target) {
            return Err(GatewayError::BlockedTarget(target).into());
        }

        if !self.checker.is_safe(&target)? {
            return Err(GatewayError::UnsafeTarget(target).into());
        }

        let credential = match (goal_scope, &self.scoped_vault) {
            (Some(scope), Some(vault)) => vault.get_scoped(Some(scope), &target)?,
            (Some(_), None) => {
                return Err(anyhow!(
                    "goal-scoped credential access requested but scoped vault is unavailable"
                ))
            }
            (None, _) => self.vault.get(&target)?,
        };
        if credential.is_none() {
            return Err(GatewayError::MissingCredentials(target).into());
        }

        let scopes: HashSet<String> = request
            .scopes
            .iter()
            .map(|scope| scope.trim().to_ascii_lowercase())
            .filter(|scope| !scope.is_empty())
            .collect();
        if scopes.is_empty() {
            return Err(anyhow!("at least one scope is required"));
        }

        let handle = SessionHandle {
            handle_id: generate_handle_id(&target, now, scopes.len())?,
            issued_at: now,
            expires_at: now + self.config.default_ttl_secs.max(1),
            scope: scopes,
            target,
            session_type: request.session_type,
            policy: request.policy,
            revoked: false,
        };

        self.handles
            .lock()
            .map_err(|_| anyhow!("session handles lock poisoned"))?
            .insert(handle.handle_id.clone(), handle.clone());
        Ok(handle)
    }

    pub fn validate_session_handle(
        &self,
        handle_id: &str,
        target: &str,
        scope: &str,
        now: u64,
    ) -> Result<()> {
        let target = normalize_target_host(target)?;
        let handles = self
            .handles
            .lock()
            .map_err(|_| anyhow!("session handles lock poisoned"))?;
        let handle = handles
            .get(handle_id)
            .ok_or_else(|| GatewayError::HandleNotFound(handle_id.to_string()))?;

        if handle.revoked {
            return Err(GatewayError::HandleRevoked(handle_id.to_string()).into());
        }
        if handle.expires_at <= now {
            return Err(GatewayError::HandleExpired(handle_id.to_string()).into());
        }
        if handle.target != target {
            return Err(GatewayError::TargetMismatch.into());
        }
        if !handle.scope.contains(&scope.to_ascii_lowercase()) {
            return Err(GatewayError::ScopeDenied(scope.to_string()).into());
        }
        Ok(())
    }

    pub fn revoke_session_handle(&self, handle_id: &str) -> Result<()> {
        let mut handles = self
            .handles
            .lock()
            .map_err(|_| anyhow!("session handles lock poisoned"))?;
        let handle = handles
            .get_mut(handle_id)
            .ok_or_else(|| GatewayError::HandleNotFound(handle_id.to_string()))?;
        handle.revoked = true;
        Ok(())
    }

    pub fn export_session_handle(
        &self,
        handle_id: &str,
        explicit_approval: bool,
    ) -> Result<SessionHandle> {
        let handles = self
            .handles
            .lock()
            .map_err(|_| anyhow!("session handles lock poisoned"))?;
        let handle = handles
            .get(handle_id)
            .ok_or_else(|| GatewayError::HandleNotFound(handle_id.to_string()))?;
        if !handle.policy.exportable && !explicit_approval {
            return Err(GatewayError::ExportDenied.into());
        }
        Ok(handle.clone())
    }
}

fn generate_handle_id(target: &str, now: u64, scope_count: usize) -> Result<String> {
    let _ = (target, now, scope_count);
    let bytes =
        os_random_bytes(16).ok_or_else(|| anyhow!("failed to generate secure handle id"))?;
    Ok(format!("sh_{}", to_hex(&bytes)))
}

fn os_random_bytes(len: usize) -> Option<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    let mut rng = rand::rngs::SysRng;
    rng.try_fill_bytes(&mut bytes).ok()?;
    Some(bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn normalize_target_host(input: &str) -> Result<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(anyhow!("target cannot be empty"));
    }

    let candidate = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let url = Url::parse(&candidate)
        .with_context(|| format!("invalid target format `{}`", input.trim()))?;
    let host = url
        .host_str()
        .map(str::to_ascii_lowercase)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("target host is missing"))?;
    Ok(host)
}

fn is_reserved_target(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "metadata.google.internal"
        || lower == "metadata.aws.internal"
        || lower == "metadata.azure.internal"
    {
        return true;
    }

    match lower.parse::<IpAddr>() {
        Ok(ip) => is_reserved_ip(ip),
        Err(_) => false,
    }
}

fn is_reserved_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
                || (v4.octets()[0] == 198 && matches!(v4.octets()[1], 18 | 19))
                || v4 == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VaultEnvelope {
    version: String,
    nonce: String,
    ciphertext: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mac: Option<String>,
}

fn load_vault_file(path: &Path, key: &[u8; 32]) -> Result<HashMap<String, CredentialRecord>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read vault file {}", path.display()))?;
    if content.trim().is_empty() {
        return Ok(HashMap::new());
    }

    if looks_like_encrypted_envelope(&content) {
        let envelope: VaultEnvelope =
            serde_json::from_str(&content).context("invalid encrypted vault envelope JSON")?;
        let nonce = from_hex(&envelope.nonce).context("invalid vault envelope nonce")?;
        let ciphertext =
            from_hex(&envelope.ciphertext).context("invalid vault envelope ciphertext")?;
        let plaintext = match envelope.version.as_str() {
            "svlt2" => decrypt_vault_v2(key, &nonce, &ciphertext)?,
            "svlt1" => {
                let mac = envelope
                    .mac
                    .as_deref()
                    .ok_or_else(|| anyhow!("svlt1 vault envelope missing mac"))
                    .and_then(|raw| from_hex(raw).context("invalid vault envelope mac"))?;
                let expected_mac = compute_vault_mac(key, &nonce, &ciphertext);
                if mac != expected_mac {
                    return Err(anyhow!("vault integrity check failed"));
                }
                xor_with_stream_cipher(&ciphertext, key, &nonce)
            }
            other => return Err(anyhow!("unsupported vault version {other}")),
        };
        let tsv = String::from_utf8(plaintext).context("vault plaintext is not UTF-8")?;
        return parse_legacy_vault_records(&tsv, path);
    }

    parse_legacy_vault_records(&content, path)
}

fn persist_vault_file(
    path: &Path,
    key: &[u8; 32],
    records: &HashMap<String, CredentialRecord>,
) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("failed to create temp vault file {}", tmp.display()))?;
    harden_file_permissions(&tmp, 0o600)?;
    let plaintext = render_legacy_vault_records(records);
    let nonce = os_random_bytes(12)
        .ok_or_else(|| anyhow!("failed to read secure random bytes for vault nonce"))?;
    let ciphertext = encrypt_vault_v2(key, &nonce, plaintext.as_bytes())?;
    let envelope = VaultEnvelope {
        version: "svlt2".to_string(),
        nonce: to_hex(&nonce),
        ciphertext: to_hex(&ciphertext),
        mac: None,
    };
    let encoded = serde_json::to_string_pretty(&envelope)?;
    file.write_all(encoded.as_bytes())
        .with_context(|| format!("failed writing temp vault {}", tmp.display()))?;
    file.flush()
        .with_context(|| format!("failed flushing temp vault {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("failed replacing vault file {}", path.display()))?;
    harden_file_permissions(path, 0o600)?;
    Ok(())
}

fn looks_like_encrypted_envelope(content: &str) -> bool {
    content.trim_start().starts_with('{')
}

fn load_or_create_vault_key(path: &Path) -> Result<[u8; 32]> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create key directory {}", parent.display()))?;
        harden_dir_permissions(parent, 0o700)?;
    }

    if path.exists() {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read vault key {}", path.display()))?;
        let bytes = from_hex(raw.trim()).context("vault key must be hex")?;
        if bytes.len() != 32 {
            return Err(anyhow!("vault key must be exactly 32 bytes"));
        }
        harden_file_permissions(path, 0o600)?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }

    let bytes = os_random_bytes(32).ok_or_else(|| anyhow!("failed to generate vault key"))?;
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    fs::write(path, to_hex(&key))
        .with_context(|| format!("failed to write vault key {}", path.display()))?;
    harden_file_permissions(path, 0o600)?;
    Ok(key)
}

fn parse_legacy_vault_records(
    content: &str,
    path: &Path,
) -> Result<HashMap<String, CredentialRecord>> {
    let mut out = HashMap::new();
    for (idx, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 3 && parts.len() != 4 {
            return Err(anyhow!(
                "invalid vault line {} in {}",
                idx + 1,
                path.display()
            ));
        }
        let service = unescape(parts[0]);
        let totp_secret = if parts.len() == 4 && !parts[3].is_empty() {
            Some(unescape(parts[3]))
        } else {
            None
        };
        out.insert(
            service.clone(),
            CredentialRecord {
                service,
                username: unescape(parts[1]),
                secret: unescape(parts[2]),
                totp_secret,
            },
        );
    }
    Ok(out)
}

fn render_legacy_vault_records(records: &HashMap<String, CredentialRecord>) -> String {
    let mut entries: Vec<_> = records.values().collect();
    entries.sort_by(|a, b| a.service.cmp(&b.service));

    let mut out = String::new();
    for record in entries {
        let totp_col = record
            .totp_secret
            .as_deref()
            .map(escape)
            .unwrap_or_default();
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            escape(&record.service),
            escape(&record.username),
            escape(&record.secret),
            totp_col,
        ));
    }
    out
}

fn encrypt_vault_v2(key: &[u8; 32], nonce: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != 12 {
        return Err(anyhow!("svlt2 nonce must be 12 bytes"));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(Nonce::from_slice(nonce), plaintext)
        .map_err(|_| anyhow!("vault encryption failed"))
}

fn decrypt_vault_v2(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if nonce.len() != 12 {
        return Err(anyhow!("svlt2 nonce must be 12 bytes"));
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|_| anyhow!("vault integrity check failed"))
}

fn xor_with_stream_cipher(input: &[u8], key: &[u8; 32], nonce: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut counter = 0u64;
    let mut offset = 0usize;
    while offset < input.len() {
        let mut hasher = Sha256::new();
        hasher.update(key);
        hasher.update(nonce);
        hasher.update(counter.to_le_bytes());
        let block = hasher.finalize();
        let block_bytes = block.as_slice();
        let remaining = input.len() - offset;
        let block_len = remaining.min(block_bytes.len());
        for index in 0..block_len {
            out.push(input[offset + index] ^ block_bytes[index]);
        }
        offset += block_len;
        counter = counter.wrapping_add(1);
    }
    out
}

fn compute_vault_mac(key: &[u8; 32], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(nonce);
    hasher.update(ciphertext);
    hasher.finalize().to_vec()
}

fn from_hex(input: &str) -> Result<Vec<u8>> {
    let bytes = input.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(anyhow!("hex input has odd length"));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0usize;
    while index < bytes.len() {
        let hi = decode_hex_nibble(bytes[index])?;
        let lo = decode_hex_nibble(bytes[index + 1])?;
        out.push((hi << 4) | lo);
        index += 2;
    }
    Ok(out)
}

fn decode_hex_nibble(byte: u8) -> Result<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(10 + (byte - b'a')),
        b'A'..=b'F' => Ok(10 + (byte - b'A')),
        _ => Err(anyhow!("invalid hex character `{}`", byte as char)),
    }
}

fn escape(input: &str) -> String {
    input
        .replace('%', "%25")
        .replace('\t', "%09")
        .replace('\n', "%0A")
}

fn unescape(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let a = chars.next();
        let b = chars.next();
        match (a, b) {
            (Some('2'), Some('5')) => out.push('%'),
            (Some('0'), Some('9')) => out.push('\t'),
            (Some('0'), Some('A')) => out.push('\n'),
            (Some(x), Some(y)) => {
                out.push('%');
                out.push(x);
                out.push(y);
            }
            _ => out.push('%'),
        }
    }
    out
}

/// Encrypts and decrypts opaque tokens (e.g. APNs/FCM device tokens) using
/// ChaCha20-Poly1305 AEAD. Key is loaded from (or generated at) a file path,
/// following the same pattern as the credential vault key.
pub struct TokenEncryptor {
    key: [u8; 32],
}

impl TokenEncryptor {
    /// Open (or create) an encryption key at the given path.
    pub fn open(key_path: impl AsRef<Path>) -> Result<Self> {
        let key = load_or_create_vault_key(key_path.as_ref())?;
        Ok(Self { key })
    }

    /// Create from an existing 32-byte key (for tests).
    pub fn from_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    /// Encrypt a plaintext token. Returns hex-encoded `nonce:ciphertext`.
    pub fn encrypt(&self, plaintext: &str) -> Result<String> {
        let nonce_bytes = os_random_bytes(12).ok_or_else(|| anyhow!("failed to generate nonce"))?;
        let ciphertext = encrypt_vault_v2(&self.key, &nonce_bytes, plaintext.as_bytes())?;
        Ok(format!("{}:{}", to_hex(&nonce_bytes), to_hex(&ciphertext)))
    }

    /// Decrypt a hex-encoded `nonce:ciphertext` back to the plaintext token.
    pub fn decrypt(&self, encrypted: &str) -> Result<String> {
        let (nonce_hex, ct_hex) = encrypted
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid encrypted token format"))?;
        let nonce_bytes = from_hex(nonce_hex).context("invalid nonce hex")?;
        let ciphertext = from_hex(ct_hex).context("invalid ciphertext hex")?;
        let plaintext = decrypt_vault_v2(&self.key, &nonce_bytes, &ciphertext)?;
        String::from_utf8(plaintext).context("decrypted token is not valid UTF-8")
    }
}

// Re-export for backward compatibility with external callers.
pub use symbiotic_core::now_unix;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn unique_suffix() -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{}_{}_{}", now_unix(), std::process::id(), id)
    }

    fn make_gateway(name: &str) -> CredentialGateway {
        let root =
            std::env::temp_dir().join(format!("credential_gateway_{name}_{}", unique_suffix()));
        let vault = Arc::new(FileCredentialVault::open(root.join("vault.tsv")).expect("vault"));
        let checker = Arc::new(StaticThreatChecker::new(HashSet::new()));
        CredentialGateway::new(GatewayConfig::default(), checker, vault)
    }

    #[test]
    fn issues_and_validates_handle() {
        let gateway = make_gateway("issue");
        gateway
            .put_credential(CredentialRecord {
                service: "x.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "x.com".to_string(),
                    scopes: vec!["web.login".to_string()],
                    session_type: SessionType::Browser,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        gateway
            .validate_session_handle(&handle.handle_id, "x.com", "web.login", now + 5)
            .expect("validate");
    }

    #[test]
    fn denies_unsafe_target() {
        let root =
            std::env::temp_dir().join(format!("credential_gateway_unsafe_{}", unique_suffix()));
        let vault = Arc::new(FileCredentialVault::open(root.join("vault.tsv")).expect("vault"));
        vault
            .put(CredentialRecord {
                service: "evil.com".to_string(),
                username: "x".to_string(),
                secret: "y".to_string(),
                totp_secret: None,
            })
            .expect("put");
        let checker = Arc::new(StaticThreatChecker::new(
            ["evil.com".to_string()].into_iter().collect(),
        ));
        let gateway = CredentialGateway::new(GatewayConfig::default(), checker, vault);

        let err = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "evil.com".to_string(),
                    scopes: vec!["web.login".to_string()],
                    session_type: SessionType::Browser,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now_unix(),
            )
            .expect_err("should deny");
        assert!(err.to_string().contains("unsafe"));
    }

    #[test]
    fn revoke_and_expire_handle() {
        let gateway = make_gateway("revoke");
        gateway
            .put_credential(CredentialRecord {
                service: "example.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "example.com".to_string(),
                    scopes: vec!["api.request".to_string()],
                    session_type: SessionType::Api,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        gateway
            .revoke_session_handle(&handle.handle_id)
            .expect("revoke");
        let revoked = gateway.validate_session_handle(
            &handle.handle_id,
            "example.com",
            "api.request",
            now + 1,
        );
        let revoked_err = revoked.expect_err("revoked handle should fail validation");
        let gateway_err = revoked_err
            .downcast_ref::<GatewayError>()
            .expect("should be GatewayError");
        assert!(
            matches!(gateway_err, GatewayError::HandleRevoked(_)),
            "expected HandleRevoked, got {gateway_err:?}"
        );

        let short = GatewayConfig {
            default_ttl_secs: 1,
            ..GatewayConfig::default()
        };
        let root =
            std::env::temp_dir().join(format!("credential_gateway_expire_{}", unique_suffix()));
        let vault = Arc::new(FileCredentialVault::open(root.join("vault.tsv")).expect("vault"));
        vault
            .put(CredentialRecord {
                service: "ttl.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");
        let checker = Arc::new(StaticThreatChecker::new(HashSet::new()));
        let ttl_gateway = CredentialGateway::new(short, checker, vault);
        let ttl_handle = ttl_gateway
            .issue_session_handle(
                AuthRequest {
                    target: "ttl.com".to_string(),
                    scopes: vec!["api.request".to_string()],
                    session_type: SessionType::Api,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");
        let expired = ttl_gateway.validate_session_handle(
            &ttl_handle.handle_id,
            "ttl.com",
            "api.request",
            now + 2,
        );
        let expired_err = expired.expect_err("expired handle should fail validation");
        let gateway_err = expired_err
            .downcast_ref::<GatewayError>()
            .expect("should be GatewayError");
        assert!(
            matches!(gateway_err, GatewayError::HandleExpired(_)),
            "expected HandleExpired, got {gateway_err:?}"
        );
    }

    #[test]
    fn export_requires_approval_when_not_exportable() {
        let gateway = make_gateway("export");
        gateway
            .put_credential(CredentialRecord {
                service: "export.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");
        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "export.com".to_string(),
                    scopes: vec!["web.login".to_string()],
                    session_type: SessionType::Browser,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        let denied = gateway.export_session_handle(&handle.handle_id, false);
        let denied_err = denied.expect_err("non-exportable handle should deny export");
        let gateway_err = denied_err
            .downcast_ref::<GatewayError>()
            .expect("should be GatewayError");
        assert!(
            matches!(gateway_err, GatewayError::ExportDenied),
            "expected ExportDenied, got {gateway_err:?}"
        );

        let allowed = gateway
            .export_session_handle(&handle.handle_id, true)
            .expect("explicit approval should allow export");
        assert_eq!(allowed.handle_id, handle.handle_id);
        assert_eq!(allowed.target, "export.com");
        assert!(allowed.scope.contains("web.login"));
    }

    #[test]
    fn validate_denies_scope_not_in_handle() {
        let gateway = make_gateway("scope_denied");
        gateway
            .put_credential(CredentialRecord {
                service: "api.example.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "api.example.com".to_string(),
                    scopes: vec!["read".to_string()],
                    session_type: SessionType::Api,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        let err = gateway
            .validate_session_handle(&handle.handle_id, "api.example.com", "write", now + 1)
            .expect_err("should deny scope");
        let gateway_err = err
            .downcast_ref::<GatewayError>()
            .expect("should be GatewayError");
        assert!(
            matches!(gateway_err, GatewayError::ScopeDenied(_)),
            "expected ScopeDenied, got {gateway_err:?}"
        );
    }

    #[test]
    fn validate_denies_target_mismatch() {
        let gateway = make_gateway("target_mismatch");
        gateway
            .put_credential(CredentialRecord {
                service: "api.example.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "api.example.com".to_string(),
                    scopes: vec!["read".to_string()],
                    session_type: SessionType::Api,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        let err = gateway
            .validate_session_handle(&handle.handle_id, "evil.com", "read", now + 1)
            .expect_err("should deny target mismatch");
        let gateway_err = err
            .downcast_ref::<GatewayError>()
            .expect("should be GatewayError");
        assert!(
            matches!(gateway_err, GatewayError::TargetMismatch),
            "expected TargetMismatch, got {gateway_err:?}"
        );
    }

    #[test]
    fn issue_normalizes_url_targets_to_host() {
        let gateway = make_gateway("normalize_target");
        gateway
            .put_credential(CredentialRecord {
                service: "x.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let handle = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "https://X.com/i/bookmarks".to_string(),
                    scopes: vec!["web.login".to_string()],
                    session_type: SessionType::Browser,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect("issue");

        assert_eq!(handle.target, "x.com");
    }

    #[test]
    fn issue_denies_reserved_targets() {
        let gateway = make_gateway("reserved_targets");
        gateway
            .put_credential(CredentialRecord {
                service: "localhost".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let now = now_unix();
        let err = gateway
            .issue_session_handle(
                AuthRequest {
                    target: "http://localhost/admin".to_string(),
                    scopes: vec!["web.login".to_string()],
                    session_type: SessionType::Browser,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now,
            )
            .expect_err("reserved target should be blocked");
        assert!(err.to_string().contains("blocked"));
    }

    #[cfg(unix)]
    #[test]
    fn vault_file_is_hardened_to_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("credential_gateway_mode_{}", unique_suffix()));
        let vault_path = root.join("vault.tsv");
        let vault = FileCredentialVault::open(&vault_path).expect("vault");

        vault
            .put(CredentialRecord {
                service: "example.com".to_string(),
                username: "user".to_string(),
                secret: "secret".to_string(),
                totp_secret: None,
            })
            .expect("write should succeed");

        let file_mode = fs::metadata(&vault_path)
            .expect("vault metadata")
            .permissions()
            .mode()
            & 0o777;
        let key_path = vault_path.with_extension("tsv.key");
        let key_mode = fs::metadata(&key_path)
            .expect("vault key metadata")
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = fs::metadata(&root)
            .expect("root metadata")
            .permissions()
            .mode()
            & 0o777;

        assert_eq!(file_mode, 0o600);
        assert_eq!(key_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }

    #[test]
    fn vault_persists_encrypted_payload() {
        let root = std::env::temp_dir().join(format!("credential_gateway_enc_{}", unique_suffix()));
        let vault_path = root.join("vault.tsv");
        let vault = FileCredentialVault::open(&vault_path).expect("vault");
        vault
            .put(CredentialRecord {
                service: "secure.example".to_string(),
                username: "alice".to_string(),
                secret: "super-secret".to_string(),
                totp_secret: None,
            })
            .expect("put");

        let raw = fs::read_to_string(&vault_path).expect("vault file should exist");
        assert!(raw.contains("\"version\": \"svlt2\""));
        assert!(!raw.contains("super-secret"));
    }

    #[test]
    fn vault_open_migrates_plaintext_tsv_to_encrypted_envelope() {
        let root =
            std::env::temp_dir().join(format!("credential_gateway_migrate_{}", unique_suffix()));
        fs::create_dir_all(&root).expect("root");
        let vault_path = root.join("vault.tsv");
        fs::write(
            &vault_path,
            "x.com\tlegacy_user\tlegacy_secret\nexample.com\tuser\tpass\n",
        )
        .expect("seed plaintext vault");

        let vault = FileCredentialVault::open(&vault_path).expect("vault should open");
        let record = vault
            .get("x.com")
            .expect("read should succeed")
            .expect("record should exist");
        assert_eq!(record.username, "legacy_user");
        assert_eq!(record.secret, "legacy_secret");

        let raw = fs::read_to_string(&vault_path).expect("vault file should exist");
        assert!(raw.contains("\"version\": \"svlt2\""));
        assert!(!raw.contains("legacy_secret"));
    }

    #[test]
    fn vault_open_migrates_svlt1_to_svlt2() {
        let root =
            std::env::temp_dir().join(format!("credential_gateway_migrate_v1_{}", unique_suffix()));
        fs::create_dir_all(&root).expect("root");
        let vault_path = root.join("vault.tsv");
        fs::File::create(&vault_path).expect("create vault");
        let key_path = vault_path.with_extension("tsv.key");
        let key = [42u8; 32];
        fs::write(&key_path, to_hex(&key)).expect("write key");

        let plaintext = "x.com\tlegacy_user\tlegacy_secret\n";
        let nonce = vec![9u8; 24];
        let ciphertext = xor_with_stream_cipher(plaintext.as_bytes(), &key, &nonce);
        let legacy = VaultEnvelope {
            version: "svlt1".to_string(),
            nonce: to_hex(&nonce),
            ciphertext: to_hex(&ciphertext),
            mac: Some(to_hex(&compute_vault_mac(&key, &nonce, &ciphertext))),
        };
        fs::write(
            &vault_path,
            serde_json::to_string_pretty(&legacy).expect("legacy envelope"),
        )
        .expect("write legacy envelope");

        let vault = FileCredentialVault::open(&vault_path).expect("vault should migrate");
        let record = vault
            .get("x.com")
            .expect("read should succeed")
            .expect("record should exist");
        assert_eq!(record.username, "legacy_user");
        assert_eq!(record.secret, "legacy_secret");

        let raw = fs::read_to_string(&vault_path).expect("vault file should exist");
        assert!(raw.contains("\"version\": \"svlt2\""));
        assert!(!raw.contains("legacy_secret"));
    }

    #[test]
    fn token_encryptor_round_trip() {
        let root =
            std::env::temp_dir().join(format!("token_encryptor_round_trip_{}", unique_suffix()));
        fs::create_dir_all(&root).expect("create temp dir");
        let key_path = root.join("token.key");
        let enc = TokenEncryptor::open(&key_path).expect("encryptor should init");

        let plaintext = "apns-device-token-abc123xyz";
        let encrypted = enc.encrypt(plaintext).expect("encrypt should succeed");
        assert_ne!(encrypted, plaintext);
        assert!(encrypted.contains(':'), "format should be nonce:ciphertext");

        let decrypted = enc.decrypt(&encrypted).expect("decrypt should succeed");
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn token_encryptor_rejects_tampered_ciphertext() {
        let key = [0x42u8; 32];
        let enc = TokenEncryptor::from_key(key);
        let encrypted = enc.encrypt("secret-token").expect("encrypt should work");
        let tampered = format!("{}{}", &encrypted[..encrypted.len() - 2], "ff");
        assert!(enc.decrypt(&tampered).is_err());
    }

    #[test]
    fn token_encryptor_persists_key_across_opens() {
        let root =
            std::env::temp_dir().join(format!("token_encryptor_persist_{}", unique_suffix()));
        fs::create_dir_all(&root).expect("create temp dir");
        let key_path = root.join("token.key");
        let enc1 = TokenEncryptor::open(&key_path).expect("first open");
        let encrypted = enc1.encrypt("my-token").expect("encrypt");

        let enc2 = TokenEncryptor::open(&key_path).expect("second open");
        let decrypted = enc2
            .decrypt(&encrypted)
            .expect("decrypt with second instance");
        assert_eq!(decrypted, "my-token");
    }

    // --- Goal-scoped vault tests ---

    #[test]
    fn goal_scoped_vault_isolates_goal_namespaces() {
        let root = std::env::temp_dir().join(format!("goal_vault_isolate_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        // Put credential in "trading" goal
        scoped
            .put_scoped(
                Some("trading"),
                CredentialRecord {
                    service: "binance.com".to_string(),
                    username: "trader".to_string(),
                    secret: "api-key-123".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put trading credential");

        // Put credential in "email" goal
        scoped
            .put_scoped(
                Some("email"),
                CredentialRecord {
                    service: "gmail.com".to_string(),
                    username: "user".to_string(),
                    secret: "email-pass".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put email credential");

        // Trading goal can see its own credential
        let trading_cred = scoped
            .get_scoped(Some("trading"), "binance.com")
            .expect("get trading")
            .expect("should exist");
        assert_eq!(trading_cred.secret, "api-key-123");

        // Trading goal CANNOT see email credential
        let cross_access = scoped
            .get_scoped(Some("trading"), "gmail.com")
            .expect("get should not error");
        assert!(
            cross_access.is_none(),
            "trading goal must not access email credentials"
        );

        // Email goal CANNOT see trading credential
        let cross_access = scoped
            .get_scoped(Some("email"), "binance.com")
            .expect("get should not error");
        assert!(
            cross_access.is_none(),
            "email goal must not access trading credentials"
        );
    }

    #[test]
    fn goal_scoped_vault_global_isolated_from_goals() {
        let root = std::env::temp_dir().join(format!("goal_vault_global_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        // Put global credential
        scoped
            .put_scoped(
                None,
                CredentialRecord {
                    service: "matrix.org".to_string(),
                    username: "daemon".to_string(),
                    secret: "matrix-token".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put global credential");

        // Put goal-scoped credential
        scoped
            .put_scoped(
                Some("trading"),
                CredentialRecord {
                    service: "exchange.com".to_string(),
                    username: "bot".to_string(),
                    secret: "exchange-key".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put trading credential");

        // Global scope can see its own credential
        let global_cred = scoped
            .get_scoped(None, "matrix.org")
            .expect("get global")
            .expect("should exist");
        assert_eq!(global_cred.secret, "matrix-token");

        // Global scope CANNOT see goal-scoped credential
        let cross_access = scoped
            .get_scoped(None, "exchange.com")
            .expect("get should not error");
        assert!(
            cross_access.is_none(),
            "global namespace must not access goal-scoped credentials"
        );

        // Goal-scoped CANNOT see global credential
        let cross_access = scoped
            .get_scoped(Some("trading"), "matrix.org")
            .expect("get should not error");
        assert!(
            cross_access.is_none(),
            "goal-scoped namespace must not access global credentials"
        );
    }

    #[test]
    fn goal_scoped_vault_provision_for_goal() {
        let root = std::env::temp_dir().join(format!("goal_vault_provision_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        scoped
            .provision_for_goal("health-tracker", "fitbit-api-key", b"encrypted-key-data")
            .expect("provision should succeed");

        // Provisioned credential accessible via goal scope
        let record = scoped
            .get_scoped(Some("health-tracker"), "fitbit-api-key")
            .expect("get should not error")
            .expect("provisioned credential should exist");
        assert_eq!(record.service, "fitbit-api-key");
        assert_eq!(record.secret, "encrypted-key-data");

        // Not accessible from global scope
        let global_access = scoped
            .get_scoped(None, "fitbit-api-key")
            .expect("get should not error");
        assert!(
            global_access.is_none(),
            "provisioned credential must not be in global namespace"
        );

        // Not accessible from other goal scope
        let other_access = scoped
            .get_scoped(Some("trading"), "fitbit-api-key")
            .expect("get should not error");
        assert!(
            other_access.is_none(),
            "provisioned credential must not leak to other goals"
        );
    }

    #[test]
    fn goal_scoped_vault_rejects_invalid_slugs() {
        let root = std::env::temp_dir().join(format!("goal_vault_invalid_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        // Empty slug
        assert!(scoped
            .put_scoped(
                Some(""),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_err());

        // Uppercase
        assert!(scoped
            .put_scoped(
                Some("Trading"),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_err());

        // Path traversal attempt
        assert!(scoped
            .put_scoped(
                Some("../etc"),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_err());

        // Leading hyphen
        assert!(scoped
            .put_scoped(
                Some("-trading"),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_err());

        // Double hyphen
        assert!(scoped
            .put_scoped(
                Some("my--goal"),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_err());

        // Valid slug works fine
        assert!(scoped
            .put_scoped(
                Some("my-trading-bot"),
                CredentialRecord {
                    service: "x.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .is_ok());
    }

    #[test]
    fn goal_scoped_vault_none_defaults_to_global() {
        let root = std::env::temp_dir().join(format!("goal_vault_default_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        scoped
            .put_scoped(
                None,
                CredentialRecord {
                    service: "default-service".to_string(),
                    username: "user".to_string(),
                    secret: "pass".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put global credential");

        // Verify it's stored in the global directory
        let global_vault_file = root.join("global").join("vault.tsv");
        assert!(
            global_vault_file.exists(),
            "global vault file should exist at {:?}",
            global_vault_file
        );

        // Retrieving with None works
        let cred = scoped
            .get_scoped(None, "default-service")
            .expect("get should work")
            .expect("credential should exist");
        assert_eq!(cred.secret, "pass");
    }

    #[cfg(unix)]
    #[test]
    fn goal_scoped_vault_directories_are_hardened() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("goal_vault_perms_{}", unique_suffix()));
        let scoped = GoalScopedVault::open(&root).expect("open goal vault");

        scoped
            .put_scoped(
                Some("secure-goal"),
                CredentialRecord {
                    service: "s.com".to_string(),
                    username: "u".to_string(),
                    secret: "s".to_string(),
                    totp_secret: None,
                },
            )
            .expect("put");

        // Check base dir permissions
        let base_mode = fs::metadata(&root)
            .expect("base dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(base_mode, 0o700, "base dir should be 0700");

        // Check goal dir permissions
        let goal_dir = root.join("secure-goal");
        let goal_mode = fs::metadata(&goal_dir)
            .expect("goal dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(goal_mode, 0o700, "goal dir should be 0700");

        // Check vault file permissions
        let vault_file = goal_dir.join("vault.tsv");
        let file_mode = fs::metadata(&vault_file)
            .expect("vault file metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600, "vault file should be 0600");
    }
}
