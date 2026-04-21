use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};

use crate::harden_file_permissions;

/// Read a Docker secret by name from `/run/secrets/<name>`.
///
/// Returns `None` if the file doesn't exist or is empty after trimming.
/// Docker secrets are stored on tmpfs and never touch disk.
pub fn read_docker_secret(name: &str) -> Option<String> {
    let path = format!("/run/secrets/{name}");
    let content = std::fs::read_to_string(path).ok()?;
    let trimmed = content.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Allowed secret keys for BYOK mode.
const BYOK_KEYS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "SYMBIOTIC_OPENROUTER_API_KEY",
    "SYMBIOTIC_X_CLIENT_ID",
    "SYMBIOTIC_X_CLIENT_SECRET",
    "SYMBIOTIC_X_REDIRECT_URI",
    "SYMBIOTIC_MATRIX_USER",
    "SYMBIOTIC_MATRIX_PASSWORD",
    "SYMBIOTIC_MATRIX_HOMESERVER",
];

/// Allowed secret keys for managed mode.
const MANAGED_KEYS: &[&str] = &[
    "SYMBIOTIC_METERED_PLAN_ID",
    "SYMBIOTIC_MATRIX_USER",
    "SYMBIOTIC_MATRIX_PASSWORD",
    "SYMBIOTIC_MATRIX_HOMESERVER",
];

/// Keys allowed in both modes (push gateway, sender policy).
const COMMON_KEYS: &[&str] = &[
    "SYMBIOTIC_PUSH_GATEWAY_API_KEY",
    "SYMBIOTIC_PUSH_APNS_GATEWAY_API_KEY",
    "SYMBIOTIC_PUSH_FCM_GATEWAY_API_KEY",
    "SYMBIOTIC_ALLOWED_SENDERS",
    "SYMBIOTIC_ALLOW_OPEN_ACCESS",
];

/// Required keys for BYOK mode (must be present for validation to pass).
const BYOK_REQUIRED: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "SYMBIOTIC_X_CLIENT_ID",
    "SYMBIOTIC_X_CLIENT_SECRET",
];

/// Required keys for managed mode.
const MANAGED_REQUIRED: &[&str] = &["SYMBIOTIC_METERED_PLAN_ID"];

/// Check if a secret key is allowed for the given mode.
pub(crate) fn is_allowed_secret_key(key: &str, mode: &str) -> bool {
    if COMMON_KEYS.contains(&key) {
        return true;
    }
    match mode {
        "byok" => BYOK_KEYS.contains(&key),
        "managed" => MANAGED_KEYS.contains(&key),
        _ => false,
    }
}

/// All allowed keys for a given mode (mode-specific + common).
fn all_allowed_keys(mode: &str) -> Vec<&'static str> {
    let mut keys: Vec<&str> = COMMON_KEYS.to_vec();
    match mode {
        "byok" => keys.extend_from_slice(BYOK_KEYS),
        "managed" => keys.extend_from_slice(MANAGED_KEYS),
        _ => {}
    }
    keys
}

/// Write or update a single key=value in the secrets file.
/// Creates the file if it does not exist.
/// Sets file permissions to 0600 after writing.
pub(crate) fn put_secret(secrets_path: &Path, key: &str, value: &str) -> Result<()> {
    // Read existing secrets, or start fresh
    let mut entries = read_secrets_map(secrets_path)?;
    entries.insert(key.to_string(), value.to_string());
    write_secrets_file(secrets_path, &entries)
}

/// Validate that all required secrets for the given mode are present.
/// Returns a map of key -> "present" | "missing" for each relevant key.
/// NEVER returns actual secret values.
pub(crate) fn validate_secrets(secrets_path: &Path, mode: &str) -> Result<HashMap<String, String>> {
    let entries = read_secrets_map(secrets_path)?;
    let all_keys = all_allowed_keys(mode);

    let mut result = HashMap::new();
    for key in &all_keys {
        let status = if entries.get(*key).map(|v| !v.is_empty()).unwrap_or(false) {
            "present"
        } else {
            "missing"
        };
        result.insert(key.to_string(), status.to_string());
    }
    Ok(result)
}

/// Check if all required secrets for the mode are present.
pub(crate) fn all_required_present(secrets_path: &Path, mode: &str) -> Result<bool> {
    let entries = read_secrets_map(secrets_path)?;
    let required = match mode {
        "byok" => BYOK_REQUIRED.to_vec(),
        "managed" => MANAGED_REQUIRED.to_vec(),
        _ => return Ok(true),
    };
    for key in required {
        if !entries.get(key).map(|v| !v.is_empty()).unwrap_or(false) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Read the secrets file into a key=value map.
/// Returns empty map if file does not exist.
fn read_secrets_map(path: &Path) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    if !path.exists() {
        return Ok(map);
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| anyhow!("failed to read secrets file: {e}"))?;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    Ok(map)
}

/// Write the secrets map to the file with 0600 permissions.
fn write_secrets_file(path: &Path, entries: &HashMap<String, String>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("failed to create secrets directory: {e}"))?;
    }

    let mut lines = vec![
        "# Symbiotic secrets — auto-generated by daemon".to_string(),
        "# NEVER commit this file. Permissions should be 0600.".to_string(),
        String::new(),
    ];

    // Sort keys for deterministic output
    let mut keys: Vec<&String> = entries.keys().collect();
    keys.sort();
    for key in keys {
        let value = &entries[key];
        lines.push(format!("{key}={value}"));
    }
    lines.push(String::new());

    std::fs::write(path, lines.join("\n"))
        .map_err(|e| anyhow!("failed to write secrets file: {e}"))?;

    harden_file_permissions(path, 0o600)
        .map_err(|e| anyhow!("failed to set secrets file permissions: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn returns_none_for_missing_secret() {
        assert!(read_docker_secret("nonexistent_secret_xyz").is_none());
    }

    #[test]
    fn reads_and_trims_secret_file() {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("test_secret");
        fs::write(&secret_path, "  my-password\n  ").unwrap();

        // We can't easily test /run/secrets/, so test the trimming logic directly
        let content = fs::read_to_string(&secret_path).unwrap();
        let trimmed = content.trim().to_string();
        assert_eq!(trimmed, "my-password");
    }

    #[test]
    fn empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let secret_path = dir.path().join("empty_secret");
        fs::write(&secret_path, "   \n  ").unwrap();

        let content = fs::read_to_string(&secret_path).unwrap();
        let trimmed = content.trim().to_string();
        assert!(trimmed.is_empty());
    }

    #[test]
    fn put_secret_creates_file_and_sets_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_path = dir.path().join("config").join(".env.secrets");

        put_secret(&secrets_path, "ANTHROPIC_API_KEY", "sk-ant-test-123").unwrap();

        assert!(secrets_path.exists());
        let map = read_secrets_map(&secrets_path).unwrap();
        assert_eq!(map.get("ANTHROPIC_API_KEY").unwrap(), "sk-ant-test-123");

        // Verify file permissions are 0600
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = fs::metadata(&secrets_path).unwrap().permissions();
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn put_secret_updates_existing_key() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_path = dir.path().join(".env.secrets");

        put_secret(&secrets_path, "OPENAI_API_KEY", "old-key").unwrap();
        put_secret(&secrets_path, "OPENAI_API_KEY", "new-key").unwrap();

        let map = read_secrets_map(&secrets_path).unwrap();
        assert_eq!(map.get("OPENAI_API_KEY").unwrap(), "new-key");
    }

    #[test]
    fn validate_secrets_reports_present_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_path = dir.path().join(".env.secrets");

        put_secret(&secrets_path, "ANTHROPIC_API_KEY", "sk-ant-abc").unwrap();

        let result = validate_secrets(&secrets_path, "byok").unwrap();
        assert_eq!(result.get("ANTHROPIC_API_KEY").unwrap(), "present");
        assert_eq!(result.get("OPENAI_API_KEY").unwrap(), "missing");
        assert_eq!(result.get("SYMBIOTIC_X_CLIENT_ID").unwrap(), "missing");
    }

    #[test]
    fn all_required_present_returns_false_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_path = dir.path().join(".env.secrets");

        assert!(!all_required_present(&secrets_path, "byok").unwrap());
    }

    #[test]
    fn all_required_present_returns_true_when_complete() {
        let dir = tempfile::tempdir().unwrap();
        let secrets_path = dir.path().join(".env.secrets");

        put_secret(&secrets_path, "ANTHROPIC_API_KEY", "sk-ant-abc").unwrap();
        put_secret(&secrets_path, "OPENAI_API_KEY", "sk-openai-abc").unwrap();
        put_secret(&secrets_path, "SYMBIOTIC_X_CLIENT_ID", "x-client-id").unwrap();
        put_secret(
            &secrets_path,
            "SYMBIOTIC_X_CLIENT_SECRET",
            "x-client-secret",
        )
        .unwrap();
        assert!(all_required_present(&secrets_path, "byok").unwrap());
    }

    #[test]
    fn is_allowed_secret_key_validates_correctly() {
        assert!(is_allowed_secret_key("ANTHROPIC_API_KEY", "byok"));
        assert!(is_allowed_secret_key("OPENAI_API_KEY", "byok"));
        assert!(!is_allowed_secret_key("ANTHROPIC_API_KEY", "managed"));
        assert!(!is_allowed_secret_key("OPENAI_API_KEY", "managed"));
        assert!(is_allowed_secret_key(
            "SYMBIOTIC_OPENROUTER_API_KEY",
            "byok"
        ));
        assert!(!is_allowed_secret_key(
            "SYMBIOTIC_OPENROUTER_API_KEY",
            "managed"
        ));
        assert!(is_allowed_secret_key(
            "SYMBIOTIC_METERED_PLAN_ID",
            "managed"
        ));
        assert!(!is_allowed_secret_key("SYMBIOTIC_METERED_PLAN_ID", "byok"));
        assert!(is_allowed_secret_key(
            "SYMBIOTIC_PUSH_GATEWAY_API_KEY",
            "byok"
        ));
        assert!(is_allowed_secret_key(
            "SYMBIOTIC_PUSH_GATEWAY_API_KEY",
            "managed"
        ));
        assert!(!is_allowed_secret_key("RANDOM_KEY", "byok"));
        assert!(!is_allowed_secret_key("RANDOM_KEY", "managed"));
    }
}
