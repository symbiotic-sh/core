//! Script registry for domain-to-script path lookup.
//!
//! Maps authentication target domains to their login scripts.
//! Falls back to a generic script if no domain-specific script exists.

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("scripts directory not found: {0}")]
    ScriptsNotFound(String),
    #[error("no script found for domain '{0}' and no generic fallback available")]
    NoScript(String),
    #[error("failed to read script '{path}': {reason}")]
    ScriptReadFailed { path: String, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptMatchKind {
    Exact,
    Parent,
    Generic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    TypeScript,
    Shell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedScriptProfile {
    pub requested_domain: String,
    pub profile_id: String,
    pub match_kind: ScriptMatchKind,
    pub script_kind: ScriptKind,
    pub path: PathBuf,
    pub sha256: String,
}

/// Registry mapping domains to auth script paths.
#[derive(Debug, Clone)]
pub struct ScriptRegistry {
    /// Domain -> script path overrides
    scripts: HashMap<String, PathBuf>,
    /// Generic fallback script (e.g., _generic.ts)
    generic_fallback: Option<PathBuf>,
}

impl ScriptRegistry {
    /// Create a new registry from a scripts directory.
    /// Scans for files matching `{domain}.ts` or `{domain}.sh` patterns.
    /// Looks for `_generic.ts` or `_generic.sh` as the fallback.
    pub fn from_dir(scripts_dir: &Path) -> Result<Self, RegistryError> {
        if !scripts_dir.is_dir() {
            return Err(RegistryError::ScriptsNotFound(
                scripts_dir.display().to_string(),
            ));
        }

        let mut scripts = HashMap::new();
        let mut generic_fallback = None;

        let entries = std::fs::read_dir(scripts_dir)
            .map_err(|_| RegistryError::ScriptsNotFound(scripts_dir.display().to_string()))?;

        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            let path = entry.path();

            // Only consider .ts and .sh files
            match path.extension().and_then(|e| e.to_str()) {
                Some("ts") | Some("sh") => {}
                _ => continue,
            }

            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };

            if stem == "_generic" {
                generic_fallback = Some(path);
            } else {
                scripts.insert(stem, path);
            }
        }

        Ok(Self {
            scripts,
            generic_fallback,
        })
    }

    /// Create an empty registry (for testing).
    pub fn empty() -> Self {
        Self {
            scripts: HashMap::new(),
            generic_fallback: None,
        }
    }

    /// Register a script for a domain.
    pub fn register(&mut self, domain: &str, path: PathBuf) {
        self.scripts.insert(domain.to_string(), path);
    }

    /// Set the generic fallback script.
    pub fn set_fallback(&mut self, path: PathBuf) {
        self.generic_fallback = Some(path);
    }

    /// Look up the script for a domain, falling back to generic.
    ///
    /// Resolution order:
    /// 1. Exact domain match (e.g., "api.github.com")
    /// 2. Parent domain match (e.g., "github.com" for "api.github.com")
    /// 3. Generic fallback
    pub fn resolve(&self, domain: &str) -> Result<&Path, RegistryError> {
        if let Some(path) = self.scripts.get(domain) {
            return Ok(path.as_path());
        }

        if let Some(dot_pos) = domain.find('.') {
            let parent = &domain[dot_pos + 1..];
            if parent.contains('.') {
                if let Some(path) = self.scripts.get(parent) {
                    return Ok(path.as_path());
                }
            }
        }

        if let Some(ref path) = self.generic_fallback {
            return Ok(path.as_path());
        }

        Err(RegistryError::NoScript(domain.to_string()))
    }

    pub fn resolve_profile(&self, domain: &str) -> Result<ResolvedScriptProfile, RegistryError> {
        // 1. Exact match
        if let Some(path) = self.scripts.get(domain) {
            return build_resolved_profile(domain, domain, ScriptMatchKind::Exact, path.clone());
        }

        // 2. Parent domain fallback (strip first subdomain)
        if let Some(dot_pos) = domain.find('.') {
            let parent = &domain[dot_pos + 1..];
            // Only try if parent has at least one dot (i.e., is a valid domain, not just "com")
            if parent.contains('.') {
                if let Some(path) = self.scripts.get(parent) {
                    return build_resolved_profile(
                        domain,
                        parent,
                        ScriptMatchKind::Parent,
                        path.clone(),
                    );
                }
            }
        }

        // 3. Generic fallback
        if let Some(ref path) = self.generic_fallback {
            return build_resolved_profile(
                domain,
                "_generic",
                ScriptMatchKind::Generic,
                path.clone(),
            );
        }

        Err(RegistryError::NoScript(domain.to_string()))
    }

    /// List all registered domains.
    pub fn domains(&self) -> Vec<&str> {
        self.scripts.keys().map(|s| s.as_str()).collect()
    }
}

fn build_resolved_profile(
    requested_domain: &str,
    profile_id: &str,
    match_kind: ScriptMatchKind,
    path: PathBuf,
) -> Result<ResolvedScriptProfile, RegistryError> {
    let script_kind = match path.extension().and_then(|e| e.to_str()) {
        Some("ts") => ScriptKind::TypeScript,
        Some("sh") => ScriptKind::Shell,
        _ => ScriptKind::Shell,
    };
    let content = std::fs::read(&path).map_err(|error| RegistryError::ScriptReadFailed {
        path: path.display().to_string(),
        reason: error.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(&content);
    let sha256 = format!("{:x}", hasher.finalize());

    Ok(ResolvedScriptProfile {
        requested_domain: requested_domain.to_string(),
        profile_id: profile_id.to_string(),
        match_kind,
        script_kind,
        path,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_domains() {
        let reg = ScriptRegistry::empty();
        assert!(reg.domains().is_empty());
    }

    #[test]
    fn empty_registry_resolve_fails() {
        let reg = ScriptRegistry::empty();
        let err = reg.resolve("github.com").unwrap_err();
        assert!(matches!(err, RegistryError::NoScript(_)));
    }

    #[test]
    fn register_and_resolve_exact() {
        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", PathBuf::from("/scripts/github.com.ts"));

        let path = reg.resolve("github.com").unwrap();
        assert_eq!(path, Path::new("/scripts/github.com.ts"));
    }

    #[test]
    fn resolve_parent_domain_fallback() {
        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", PathBuf::from("/scripts/github.com.ts"));

        // api.github.com should fall back to github.com
        let path = reg.resolve("api.github.com").unwrap();
        assert_eq!(path, Path::new("/scripts/github.com.ts"));
    }

    #[test]
    fn resolve_no_parent_for_tld() {
        // "example.com" should not try to match just "com"
        let mut reg = ScriptRegistry::empty();
        reg.register("com", PathBuf::from("/scripts/com.ts"));

        let err = reg.resolve("example.com").unwrap_err();
        assert!(matches!(err, RegistryError::NoScript(_)));
    }

    #[test]
    fn resolve_generic_fallback() {
        let mut reg = ScriptRegistry::empty();
        reg.set_fallback(PathBuf::from("/scripts/_generic.ts"));

        let path = reg.resolve("unknown.example.com").unwrap();
        assert_eq!(path, Path::new("/scripts/_generic.ts"));
    }

    #[test]
    fn exact_match_beats_fallback() {
        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", PathBuf::from("/scripts/github.com.ts"));
        reg.set_fallback(PathBuf::from("/scripts/_generic.ts"));

        let path = reg.resolve("github.com").unwrap();
        assert_eq!(path, Path::new("/scripts/github.com.ts"));
    }

    #[test]
    fn parent_match_beats_fallback() {
        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", PathBuf::from("/scripts/github.com.ts"));
        reg.set_fallback(PathBuf::from("/scripts/_generic.ts"));

        let path = reg.resolve("api.github.com").unwrap();
        assert_eq!(path, Path::new("/scripts/github.com.ts"));
    }

    #[test]
    fn list_domains() {
        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", PathBuf::from("/scripts/github.com.ts"));
        reg.register("gitlab.com", PathBuf::from("/scripts/gitlab.com.sh"));

        let mut domains = reg.domains();
        domains.sort();
        assert_eq!(domains, vec!["github.com", "gitlab.com"]);
    }

    #[test]
    fn from_dir_not_found() {
        let err = ScriptRegistry::from_dir(Path::new("/nonexistent/path")).unwrap_err();
        assert!(matches!(err, RegistryError::ScriptsNotFound(_)));
    }

    #[test]
    fn from_dir_scans_scripts() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Create domain-specific scripts
        std::fs::write(dir.path().join("github.com.ts"), "// github").expect("write");
        std::fs::write(dir.path().join("gitlab.com.sh"), "#!/bin/bash").expect("write");
        // Create generic fallback
        std::fs::write(dir.path().join("_generic.ts"), "// generic").expect("write");
        // Create non-script file (should be ignored)
        std::fs::write(dir.path().join("README.md"), "# readme").expect("write");

        let reg = ScriptRegistry::from_dir(dir.path()).unwrap();
        let mut domains = reg.domains();
        domains.sort();
        assert_eq!(domains, vec!["github.com", "gitlab.com"]);

        // Exact matches work
        assert!(reg.resolve("github.com").is_ok());
        assert!(reg.resolve("gitlab.com").is_ok());

        // Generic fallback works
        assert!(reg.resolve("unknown.example.com").is_ok());
    }

    #[test]
    fn resolve_profile_reports_exact_match_and_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("github.com.sh");
        std::fs::write(&path, "#!/bin/sh\necho ok\n").expect("write script");

        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", path.clone());

        let resolved = reg.resolve_profile("github.com").expect("resolve profile");
        assert_eq!(resolved.profile_id, "github.com");
        assert_eq!(resolved.match_kind, ScriptMatchKind::Exact);
        assert_eq!(resolved.script_kind, ScriptKind::Shell);
        assert_eq!(resolved.path, path);
        assert_eq!(resolved.sha256.len(), 64);
    }

    #[test]
    fn resolve_profile_reports_parent_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("github.com.ts");
        std::fs::write(&path, "console.log('ok')\n").expect("write script");

        let mut reg = ScriptRegistry::empty();
        reg.register("github.com", path);

        let resolved = reg
            .resolve_profile("api.github.com")
            .expect("resolve profile");
        assert_eq!(resolved.profile_id, "github.com");
        assert_eq!(resolved.match_kind, ScriptMatchKind::Parent);
        assert_eq!(resolved.script_kind, ScriptKind::TypeScript);
    }

    #[test]
    fn resolve_profile_reports_generic_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("_generic.sh");
        std::fs::write(&path, "#!/bin/sh\necho ok\n").expect("write script");

        let mut reg = ScriptRegistry::empty();
        reg.set_fallback(path);

        let resolved = reg
            .resolve_profile("unknown.example.com")
            .expect("resolve profile");
        assert_eq!(resolved.profile_id, "_generic");
        assert_eq!(resolved.match_kind, ScriptMatchKind::Generic);
    }
}
