//! Session profile management for browser automation.
//!
//! Profiles store per-site browser configuration and session state.
//! Profile metadata is persisted in `data/browser-profiles/profiles.json`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Configuration for a browser session profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionProfile {
    /// Unique profile identifier.
    pub id: String,
    /// Human-readable name (e.g., "X Primary Account").
    pub name: String,
    /// Target domain(s) this profile is used for.
    pub domains: Vec<String>,
    /// Path to the Chromium user data directory (relative to profiles dir).
    pub user_data_dir: String,
    /// Browser launch arguments.
    pub launch_args: Vec<String>,
    /// Viewport dimensions.
    pub viewport: Viewport,
    /// Whether to run headless (true for daemon, false for interactive login).
    pub headless: bool,
    /// Session health: last validated timestamp.
    pub last_validated_at: Option<u64>,
    /// Session health: is the session currently valid?
    pub session_valid: bool,
    /// Proxy configuration (optional).
    pub proxy: Option<ProxyConfig>,
}

/// Browser viewport dimensions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
        }
    }
}

/// Proxy configuration for browser sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProxyConfig {
    pub server: String,
    pub bypass: Vec<String>,
}

/// Manages browser session profiles on disk.
pub struct ProfileManager {
    profiles_dir: PathBuf,
    profiles: Vec<SessionProfile>,
}

impl ProfileManager {
    /// Load profiles from the given directory.
    ///
    /// If `profiles.json` does not exist, returns an empty manager.
    pub fn load(profiles_dir: &Path) -> Result<Self> {
        let profiles_file = profiles_dir.join("profiles.json");
        let profiles = if profiles_file.exists() {
            let data = std::fs::read_to_string(&profiles_file)?;
            serde_json::from_str(&data)?
        } else {
            Vec::new()
        };
        Ok(Self {
            profiles_dir: profiles_dir.to_path_buf(),
            profiles,
        })
    }

    /// Create a new empty manager for a given directory (does not read disk).
    pub fn new(profiles_dir: PathBuf) -> Self {
        Self {
            profiles_dir,
            profiles: Vec::new(),
        }
    }

    /// Get the profile for a target URL by matching against profile domains.
    pub fn profile_for_url(&self, url: &str) -> Option<&SessionProfile> {
        self.profiles
            .iter()
            .find(|p| p.domains.iter().any(|domain| url.contains(domain.as_str())))
    }

    /// Get a profile by ID.
    pub fn profile_by_id(&self, profile_id: &str) -> Option<&SessionProfile> {
        self.profiles.iter().find(|p| p.id == profile_id)
    }

    /// Mark a session as invalid (triggers login handoff on next use).
    pub fn invalidate(&mut self, profile_id: &str) -> Result<()> {
        let profile = self
            .profiles
            .iter_mut()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| anyhow!("profile not found: {profile_id}"))?;
        profile.session_valid = false;
        Ok(())
    }

    /// Mark a session as valid with a timestamp.
    pub fn validate(&mut self, profile_id: &str, validated_at: u64) -> Result<()> {
        let profile = self
            .profiles
            .iter_mut()
            .find(|p| p.id == profile_id)
            .ok_or_else(|| anyhow!("profile not found: {profile_id}"))?;
        profile.session_valid = true;
        profile.last_validated_at = Some(validated_at);
        Ok(())
    }

    /// Create a new profile.
    pub fn create(&mut self, profile: SessionProfile) -> Result<()> {
        if self.profiles.iter().any(|p| p.id == profile.id) {
            return Err(anyhow!("profile already exists: {}", profile.id));
        }
        self.profiles.push(profile);
        Ok(())
    }

    /// Save profiles to `profiles.json` in the profiles directory.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.profiles_dir)?;
        let profiles_file = self.profiles_dir.join("profiles.json");
        let data = serde_json::to_string_pretty(&self.profiles)?;
        std::fs::write(profiles_file, data)?;
        Ok(())
    }

    /// Return a reference to all profiles.
    pub fn profiles(&self) -> &[SessionProfile] {
        &self.profiles
    }

    /// The absolute path to a profile's user data directory.
    pub fn user_data_path(&self, profile: &SessionProfile) -> PathBuf {
        self.profiles_dir.join(&profile.user_data_dir)
    }
}

/// Create a default session profile for a given site.
pub fn default_profile(id: &str, name: &str, domains: Vec<String>) -> SessionProfile {
    SessionProfile {
        id: id.to_string(),
        name: name.to_string(),
        domains,
        user_data_dir: id.to_string(),
        launch_args: Vec::new(),
        viewport: Viewport::default(),
        headless: true,
        last_validated_at: None,
        session_valid: false,
        proxy: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_profile(id: &str, domains: Vec<&str>) -> SessionProfile {
        SessionProfile {
            id: id.to_string(),
            name: format!("Profile {id}"),
            domains: domains.into_iter().map(String::from).collect(),
            user_data_dir: id.to_string(),
            launch_args: Vec::new(),
            viewport: Viewport::default(),
            headless: true,
            last_validated_at: None,
            session_valid: false,
            proxy: None,
        }
    }

    #[test]
    fn session_profile_serde_roundtrip() {
        let profile = sample_profile("x-primary", vec!["x.com", "twitter.com"]);
        let json = serde_json::to_string(&profile).expect("serialize");
        let parsed: SessionProfile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, profile);
    }

    #[test]
    fn viewport_default_is_1280x720() {
        let v = Viewport::default();
        assert_eq!(v.width, 1280);
        assert_eq!(v.height, 720);
    }

    #[test]
    fn profile_for_url_matches_domain() {
        let mut mgr = ProfileManager::new(PathBuf::from("/tmp/profiles"));
        mgr.create(sample_profile("x", vec!["x.com", "twitter.com"]))
            .unwrap();
        mgr.create(sample_profile("gh", vec!["github.com"]))
            .unwrap();

        let p = mgr
            .profile_for_url("https://x.com/user/status/123")
            .expect("should match");
        assert_eq!(p.id, "x");

        let p = mgr
            .profile_for_url("https://github.com/rust-lang/rust")
            .expect("should match");
        assert_eq!(p.id, "gh");

        assert!(mgr.profile_for_url("https://example.com").is_none());
    }

    #[test]
    fn create_rejects_duplicate_id() {
        let mut mgr = ProfileManager::new(PathBuf::from("/tmp/profiles"));
        mgr.create(sample_profile("x", vec!["x.com"])).unwrap();
        let err = mgr
            .create(sample_profile("x", vec!["twitter.com"]))
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn invalidate_and_validate_session() {
        let mut mgr = ProfileManager::new(PathBuf::from("/tmp/profiles"));
        mgr.create(sample_profile("x", vec!["x.com"])).unwrap();

        // Initially invalid
        assert!(!mgr.profile_by_id("x").unwrap().session_valid);

        // Validate it
        mgr.validate("x", 1700000000).unwrap();
        let p = mgr.profile_by_id("x").unwrap();
        assert!(p.session_valid);
        assert_eq!(p.last_validated_at, Some(1700000000));

        // Invalidate it
        mgr.invalidate("x").unwrap();
        assert!(!mgr.profile_by_id("x").unwrap().session_valid);
    }

    #[test]
    fn invalidate_unknown_profile_fails() {
        let mut mgr = ProfileManager::new(PathBuf::from("/tmp/profiles"));
        let err = mgr.invalidate("nonexistent").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join("symbiotic-browser-test-profiles");
        let _ = std::fs::remove_dir_all(&dir);

        let mut mgr = ProfileManager::new(dir.clone());
        mgr.create(sample_profile("x", vec!["x.com"])).unwrap();
        mgr.create(sample_profile("gh", vec!["github.com"]))
            .unwrap();
        mgr.save().unwrap();

        let loaded = ProfileManager::load(&dir).expect("load");
        assert_eq!(loaded.profiles().len(), 2);
        assert_eq!(loaded.profiles()[0].id, "x");
        assert_eq!(loaded.profiles()[1].id, "gh");

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_dir_returns_empty() {
        let dir = std::env::temp_dir().join("symbiotic-browser-test-nonexistent");
        let _ = std::fs::remove_dir_all(&dir);
        let mgr = ProfileManager::load(&dir).expect("load");
        assert!(mgr.profiles().is_empty());
    }

    #[test]
    fn user_data_path_joins_correctly() {
        let mgr = ProfileManager::new(PathBuf::from("/data/browser-profiles"));
        let profile = sample_profile("x-primary", vec!["x.com"]);
        assert_eq!(
            mgr.user_data_path(&profile),
            PathBuf::from("/data/browser-profiles/x-primary")
        );
    }

    #[test]
    fn default_profile_helper() {
        let p = default_profile("gh-main", "GitHub Main", vec!["github.com".to_string()]);
        assert_eq!(p.id, "gh-main");
        assert_eq!(p.name, "GitHub Main");
        assert_eq!(p.domains, vec!["github.com"]);
        assert!(p.headless);
        assert!(!p.session_valid);
        assert_eq!(p.viewport, Viewport::default());
    }
}
