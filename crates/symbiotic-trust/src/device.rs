//! Device trust bootstrap: SAS verification, trust cache, and revocation.
//!
//! See `docs/design/device-trust-bootstrap.md` for the full design.

use std::fs;
use std::path::PathBuf;
use symbiotic_core::now_unix;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unique identifier for a Matrix device, as returned by the homeserver.
pub type MatrixDeviceId = String;

/// Trust level for a device in the Symbiotic system.
///
/// Ordering: `Revoked < Unverified < Verified` for enforcement checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum DeviceTrustLevel {
    /// Device trust was revoked (must re-verify).
    Revoked = 0,
    /// Device registered but not yet verified via SAS.
    Unverified = 1,
    /// SAS emoji verification completed successfully.
    Verified = 2,
}

/// A record of a device's trust state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceTrustRecord {
    /// Matrix device ID (e.g., "ABCDEF1234").
    pub device_id: MatrixDeviceId,
    /// Current trust level.
    pub trust_level: DeviceTrustLevel,
    /// Unix timestamp when verification completed (0 if unverified).
    pub verified_at: u64,
    /// Unix timestamp when the record was last checked against the homeserver.
    pub last_checked: u64,
    /// Matrix user ID that owns this device.
    pub user_id: String,
}

/// Errors that can occur during device trust operations.
#[derive(Debug, Error)]
pub enum DeviceTrustError {
    #[error("device not found: {0}")]
    DeviceNotFound(MatrixDeviceId),
    #[error("device not verified: {0}")]
    DeviceNotVerified(MatrixDeviceId),
    #[error("device trust revoked: {0}")]
    DeviceRevoked(MatrixDeviceId),
    #[error("SAS verification failed: {0}")]
    SasVerificationFailed(String),
    #[error("trust cache corrupted: {0}")]
    CacheCorrupted(String),
    #[error("homeserver unreachable")]
    HomeserverUnreachable,
    #[error("verification timeout after {0} seconds")]
    VerificationTimeout(u64),
    #[error("bootstrap code invalid or expired")]
    BootstrapCodeInvalid,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for the trust cache file.
#[derive(Debug)]
pub struct TrustCacheConfig {
    /// Path to the trust cache file.
    pub cache_path: PathBuf,
    /// Maximum age in seconds before re-validation is required.
    pub max_age_secs: u64,
}

impl Default for TrustCacheConfig {
    fn default() -> Self {
        Self {
            cache_path: dirs_cache_path(),
            max_age_secs: 86400, // 24 hours
        }
    }
}

/// Returns the default trust cache path (~/.symbiotic/trust-cache.json).
fn dirs_cache_path() -> PathBuf {
    // Use home_dir from std; fall back to /tmp if unavailable.
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
        .join(".symbiotic")
        .join("trust-cache.json")
}

/// Serialized format of the trust cache file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TrustCacheFile {
    version: u32,
    devices: Vec<DeviceTrustRecord>,
    cache_written_at: u64,
}

/// In-memory trust cache with persistence to a JSON file.
#[derive(Debug)]
pub struct TrustCache {
    config: TrustCacheConfig,
    devices: Vec<DeviceTrustRecord>,
}

impl TrustCache {
    /// Create a new empty trust cache with the given config.
    pub fn new(config: TrustCacheConfig) -> Self {
        Self {
            config,
            devices: Vec::new(),
        }
    }

    /// Load trust cache from the configured file path.
    /// If the file does not exist, returns an empty cache.
    /// If the file is corrupted, returns `CacheCorrupted`.
    pub fn load(config: TrustCacheConfig) -> Result<Self, DeviceTrustError> {
        let path = &config.cache_path;
        if !path.exists() {
            return Ok(Self::new(config));
        }
        let content = fs::read_to_string(path)?;
        let file: TrustCacheFile = serde_json::from_str(&content)
            .map_err(|e| DeviceTrustError::CacheCorrupted(e.to_string()))?;
        if file.version != 1 {
            return Err(DeviceTrustError::CacheCorrupted(format!(
                "unsupported cache version: {}",
                file.version
            )));
        }
        Ok(Self {
            config,
            devices: file.devices,
        })
    }

    /// Persist the current trust cache to disk.
    pub fn save(&self) -> Result<(), DeviceTrustError> {
        let path = &self.config.cache_path;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = TrustCacheFile {
            version: 1,
            devices: self.devices.clone(),
            cache_written_at: now_unix(),
        };
        let content = serde_json::to_string_pretty(&file)
            .map_err(|e| DeviceTrustError::CacheCorrupted(e.to_string()))?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Get the trust record for a device by ID.
    pub fn get(&self, device_id: &str) -> Option<&DeviceTrustRecord> {
        self.devices.iter().find(|d| d.device_id == device_id)
    }

    /// Get the trust level for a device, considering staleness.
    /// Returns `Unverified` if the device is not found or if the record is stale.
    pub fn trust_level(&self, device_id: &str, now: u64) -> DeviceTrustLevel {
        match self.get(device_id) {
            None => DeviceTrustLevel::Unverified,
            Some(record) => {
                if record.trust_level == DeviceTrustLevel::Revoked {
                    return DeviceTrustLevel::Revoked;
                }
                if record.trust_level == DeviceTrustLevel::Verified
                    && now.saturating_sub(record.last_checked) > self.config.max_age_secs
                {
                    // Stale record: treat as unverified until revalidated.
                    return DeviceTrustLevel::Unverified;
                }
                record.trust_level
            }
        }
    }

    /// Add or update a device record in the cache.
    pub fn upsert(&mut self, record: DeviceTrustRecord) {
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|d| d.device_id == record.device_id)
        {
            *existing = record;
        } else {
            self.devices.push(record);
        }
    }

    /// Remove a device from the cache entirely.
    pub fn remove(&mut self, device_id: &str) -> bool {
        let before = self.devices.len();
        self.devices.retain(|d| d.device_id != device_id);
        self.devices.len() < before
    }

    /// List all device records.
    pub fn devices(&self) -> &[DeviceTrustRecord] {
        &self.devices
    }

    /// Revoke trust for a device (user-initiated, automatic, or admin).
    pub fn revoke(&mut self, device_id: &str) -> Result<(), DeviceTrustError> {
        let record = self
            .devices
            .iter_mut()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| DeviceTrustError::DeviceNotFound(device_id.to_string()))?;
        record.trust_level = DeviceTrustLevel::Revoked;
        record.last_checked = now_unix();
        Ok(())
    }

    /// Mark a device as verified (after successful SAS verification).
    pub fn mark_verified(&mut self, device_id: &str, user_id: &str) {
        let now = now_unix();
        let record = DeviceTrustRecord {
            device_id: device_id.to_string(),
            trust_level: DeviceTrustLevel::Verified,
            verified_at: now,
            last_checked: now,
            user_id: user_id.to_string(),
        };
        self.upsert(record);
    }

    /// Refresh the last_checked timestamp for a device (after revalidation).
    pub fn refresh_check(&mut self, device_id: &str, now: u64) -> Result<(), DeviceTrustError> {
        let record = self
            .devices
            .iter_mut()
            .find(|d| d.device_id == device_id)
            .ok_or_else(|| DeviceTrustError::DeviceNotFound(device_id.to_string()))?;
        record.last_checked = now;
        Ok(())
    }
}

/// Check that a device is verified and return an error if not.
/// This is the trust gate used at enforcement points.
pub fn require_verified(
    cache: &TrustCache,
    device_id: &str,
    now: u64,
) -> Result<(), DeviceTrustError> {
    match cache.trust_level(device_id, now) {
        DeviceTrustLevel::Verified => Ok(()),
        DeviceTrustLevel::Revoked => Err(DeviceTrustError::DeviceRevoked(device_id.to_string())),
        DeviceTrustLevel::Unverified => {
            Err(DeviceTrustError::DeviceNotVerified(device_id.to_string()))
        }
    }
}

/// A one-time bootstrap code for first-device verification.
#[derive(Debug, Clone)]
pub struct BootstrapCode {
    /// The 6-digit numeric code.
    pub code: String,
    /// Unix timestamp when the code was generated.
    pub created_at: u64,
    /// Expiry duration in seconds (default 300 = 5 minutes).
    pub expires_in_secs: u64,
    /// Whether this code has been consumed.
    pub consumed: bool,
}

impl BootstrapCode {
    /// Generate a new 6-digit bootstrap code using a CSPRNG.
    pub fn generate() -> Self {
        use rand::RngExt;

        let code: u32 = rand::rng().random_range(0..1_000_000);
        let now = now_unix();
        Self {
            code: format!("{code:06}"),
            created_at: now,
            expires_in_secs: 300,
            consumed: false,
        }
    }

    /// Validate a user-supplied code against this bootstrap code.
    pub fn validate(&mut self, input: &str, now: u64) -> Result<(), DeviceTrustError> {
        if self.consumed {
            return Err(DeviceTrustError::BootstrapCodeInvalid);
        }
        if now.saturating_sub(self.created_at) > self.expires_in_secs {
            return Err(DeviceTrustError::BootstrapCodeInvalid);
        }
        if input != self.code {
            return Err(DeviceTrustError::BootstrapCodeInvalid);
        }
        self.consumed = true;
        Ok(())
    }

    /// Check if the code has expired.
    pub fn is_expired(&self, now: u64) -> bool {
        now.saturating_sub(self.created_at) > self.expires_in_secs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use tempfile::TempDir;

    fn test_config(dir: &Path) -> TrustCacheConfig {
        TrustCacheConfig {
            cache_path: dir.join("trust-cache.json"),
            max_age_secs: 86400,
        }
    }

    fn sample_record(
        device_id: &str,
        level: DeviceTrustLevel,
        last_checked: u64,
    ) -> DeviceTrustRecord {
        DeviceTrustRecord {
            device_id: device_id.to_string(),
            trust_level: level,
            verified_at: if level == DeviceTrustLevel::Verified {
                last_checked
            } else {
                0
            },
            last_checked,
            user_id: "@user:example.test".to_string(),
        }
    }

    // --- Trust level ordering ---

    #[test]
    fn device_trust_level_ordering() {
        assert!(DeviceTrustLevel::Revoked < DeviceTrustLevel::Unverified);
        assert!(DeviceTrustLevel::Unverified < DeviceTrustLevel::Verified);
        assert!(DeviceTrustLevel::Revoked < DeviceTrustLevel::Verified);
    }

    // --- Trust cache CRUD ---

    #[test]
    fn trust_cache_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        let record = sample_record("DEV001", DeviceTrustLevel::Verified, 1_700_000_000);
        cache.upsert(record.clone());
        cache.save().unwrap();

        let config2 = test_config(tmp.path());
        let loaded = TrustCache::load(config2).unwrap();
        let got = loaded.get("DEV001").expect("device should exist");
        assert_eq!(got.device_id, "DEV001");
        assert_eq!(got.trust_level, DeviceTrustLevel::Verified);
        assert_eq!(got.verified_at, 1_700_000_000);
        assert_eq!(got.user_id, "@user:example.test");
    }

    #[test]
    fn trust_cache_rejects_corrupted() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trust-cache.json");
        fs::write(&path, "not valid json {{{").unwrap();

        let config = TrustCacheConfig {
            cache_path: path,
            max_age_secs: 86400,
        };
        let result = TrustCache::load(config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DeviceTrustError::CacheCorrupted(_)),
            "expected CacheCorrupted, got: {err}"
        );
    }

    #[test]
    fn trust_cache_invalidates_stale() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        // Record checked at t=1000, max_age=86400
        cache.upsert(sample_record("DEV_STALE", DeviceTrustLevel::Verified, 1000));

        // At t=1000+86400, still valid (boundary)
        assert_eq!(
            cache.trust_level("DEV_STALE", 1000 + 86400),
            DeviceTrustLevel::Verified
        );

        // At t=1000+86401, stale → treated as Unverified
        assert_eq!(
            cache.trust_level("DEV_STALE", 1000 + 86401),
            DeviceTrustLevel::Unverified
        );
    }

    #[test]
    fn trust_cache_upsert_updates_existing() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        cache.upsert(sample_record("DEV_UP", DeviceTrustLevel::Unverified, 1000));
        assert_eq!(
            cache.trust_level("DEV_UP", 1000),
            DeviceTrustLevel::Unverified
        );

        cache.upsert(sample_record("DEV_UP", DeviceTrustLevel::Verified, 2000));
        assert_eq!(
            cache.trust_level("DEV_UP", 2000),
            DeviceTrustLevel::Verified
        );
        assert_eq!(cache.devices().len(), 1, "should not duplicate");
    }

    #[test]
    fn trust_cache_remove() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        cache.upsert(sample_record("DEV_RM", DeviceTrustLevel::Verified, 1000));
        assert!(cache.remove("DEV_RM"));
        assert!(cache.get("DEV_RM").is_none());
        assert!(!cache.remove("DEV_RM"), "second remove should return false");
    }

    #[test]
    fn trust_cache_missing_device_returns_unverified() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let cache = TrustCache::new(config);
        assert_eq!(
            cache.trust_level("NONEXISTENT", now_unix()),
            DeviceTrustLevel::Unverified
        );
    }

    #[test]
    fn trust_cache_empty_file_loads_empty() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        // File does not exist
        let cache = TrustCache::load(config).unwrap();
        assert!(cache.devices().is_empty());
    }

    // --- Enforcement ---

    #[test]
    fn enforcement_blocks_unverified() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record("DEV_UV", DeviceTrustLevel::Unverified, 1000));

        let result = require_verified(&cache, "DEV_UV", 1000);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), DeviceTrustError::DeviceNotVerified(_)),
            "expected DeviceNotVerified"
        );
    }

    #[test]
    fn enforcement_allows_verified() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record("DEV_V", DeviceTrustLevel::Verified, 1000));

        let result = require_verified(&cache, "DEV_V", 1000);
        assert!(result.is_ok());
    }

    #[test]
    fn enforcement_blocks_revoked() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record("DEV_R", DeviceTrustLevel::Revoked, 1000));

        let result = require_verified(&cache, "DEV_R", 1000);
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), DeviceTrustError::DeviceRevoked(_)),
            "expected DeviceRevoked"
        );
    }

    #[test]
    fn enforcement_blocks_unknown_device() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let cache = TrustCache::new(config);

        let result = require_verified(&cache, "UNKNOWN", now_unix());
        assert!(result.is_err());
        assert!(
            matches!(result.unwrap_err(), DeviceTrustError::DeviceNotVerified(_)),
            "expected DeviceNotVerified for unknown device"
        );
    }

    // --- Revocation ---

    #[test]
    fn revocation_updates_cache() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record("DEV_REV", DeviceTrustLevel::Verified, 1000));

        // User-initiated revocation
        cache.revoke("DEV_REV").unwrap();
        let record = cache.get("DEV_REV").unwrap();
        assert_eq!(record.trust_level, DeviceTrustLevel::Revoked);

        // Subsequent trust gate check should fail
        let result = require_verified(&cache, "DEV_REV", now_unix());
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::DeviceRevoked(_)
        ));
    }

    #[test]
    fn revocation_nonexistent_device_errors() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        let result = cache.revoke("GHOST");
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::DeviceNotFound(_)
        ));
    }

    #[test]
    fn admin_revocation_persists() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record("DEV_ADMIN", DeviceTrustLevel::Verified, 1000));
        cache.revoke("DEV_ADMIN").unwrap();
        cache.save().unwrap();

        // Reload and verify
        let config2 = test_config(tmp.path());
        let loaded = TrustCache::load(config2).unwrap();
        assert_eq!(
            loaded.get("DEV_ADMIN").unwrap().trust_level,
            DeviceTrustLevel::Revoked
        );
    }

    // --- Mark verified ---

    #[test]
    fn mark_verified_creates_record() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        cache.mark_verified("DEV_NEW", "@alice:example.test");
        let record = cache.get("DEV_NEW").unwrap();
        assert_eq!(record.trust_level, DeviceTrustLevel::Verified);
        assert_eq!(record.user_id, "@alice:example.test");
        assert!(record.verified_at > 0);
    }

    #[test]
    fn mark_verified_upgrades_unverified() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);

        cache.upsert(sample_record(
            "DEV_UPGRADE",
            DeviceTrustLevel::Unverified,
            1000,
        ));
        cache.mark_verified("DEV_UPGRADE", "@alice:example.test");
        assert_eq!(
            cache.get("DEV_UPGRADE").unwrap().trust_level,
            DeviceTrustLevel::Verified
        );
    }

    // --- Refresh check ---

    #[test]
    fn refresh_check_updates_last_checked() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record(
            "DEV_REFRESH",
            DeviceTrustLevel::Verified,
            1000,
        ));

        cache.refresh_check("DEV_REFRESH", 2000).unwrap();
        assert_eq!(cache.get("DEV_REFRESH").unwrap().last_checked, 2000);
    }

    // --- Bootstrap code ---

    #[test]
    fn bootstrap_code_generates_six_digits() {
        let code = BootstrapCode::generate();
        assert_eq!(code.code.len(), 6);
        assert!(code.code.chars().all(|c| c.is_ascii_digit()));
        assert!(!code.consumed);
    }

    #[test]
    fn bootstrap_code_validates_correct() {
        let mut code = BootstrapCode::generate();
        let input = code.code.clone();
        let now = code.created_at + 10; // within 5 minutes
        assert!(code.validate(&input, now).is_ok());
        assert!(code.consumed);
    }

    #[test]
    fn bootstrap_code_rejects_wrong_code() {
        let mut code = BootstrapCode::generate();
        let now = code.created_at + 10;
        let result = code.validate("000000", now);
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::BootstrapCodeInvalid
        ));
    }

    #[test]
    fn bootstrap_code_rejects_expired() {
        let mut code = BootstrapCode::generate();
        let input = code.code.clone();
        let now = code.created_at + 301; // past 5 minutes
        let result = code.validate(&input, now);
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::BootstrapCodeInvalid
        ));
    }

    #[test]
    fn bootstrap_code_rejects_double_use() {
        let mut code = BootstrapCode::generate();
        let input = code.code.clone();
        let now = code.created_at + 10;
        code.validate(&input, now).unwrap();
        let result = code.validate(&input, now);
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::BootstrapCodeInvalid
        ));
    }

    #[test]
    fn bootstrap_code_is_expired_check() {
        let code = BootstrapCode::generate();
        assert!(!code.is_expired(code.created_at + 299));
        assert!(!code.is_expired(code.created_at + 300));
        assert!(code.is_expired(code.created_at + 301));
    }

    // --- Cache survives restart ---

    #[test]
    fn cache_survives_restart() {
        let tmp = TempDir::new().unwrap();
        let now = now_unix();

        // Write
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.mark_verified("DEV_PERSIST", "@bob:example.test");
        cache.save().unwrap();

        // Re-read
        let config2 = test_config(tmp.path());
        let loaded = TrustCache::load(config2).unwrap();
        let record = loaded.get("DEV_PERSIST").unwrap();
        assert_eq!(record.trust_level, DeviceTrustLevel::Verified);
        assert_eq!(record.user_id, "@bob:example.test");
        assert!(require_verified(&loaded, "DEV_PERSIST", now).is_ok());
    }

    // --- Revoked devices stay stale-resistant ---

    #[test]
    fn revoked_never_becomes_unverified_from_staleness() {
        let tmp = TempDir::new().unwrap();
        let config = test_config(tmp.path());
        let mut cache = TrustCache::new(config);
        cache.upsert(sample_record(
            "DEV_REVSTALE",
            DeviceTrustLevel::Revoked,
            1000,
        ));

        // Even far in the future, revoked stays revoked (not downgraded to unverified)
        assert_eq!(
            cache.trust_level("DEV_REVSTALE", 1_000_000_000),
            DeviceTrustLevel::Revoked
        );
    }

    // --- Unsupported cache version ---

    #[test]
    fn cache_rejects_unsupported_version() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("trust-cache.json");
        let content = r#"{"version":99,"devices":[],"cache_written_at":0}"#;
        fs::write(&path, content).unwrap();

        let config = TrustCacheConfig {
            cache_path: path,
            max_age_secs: 86400,
        };
        let result = TrustCache::load(config);
        assert!(matches!(
            result.unwrap_err(),
            DeviceTrustError::CacheCorrupted(_)
        ));
    }
}
