//! Daemon bootstrap orchestrator — resolves Matrix credentials and rooms
//! from a secure priority chain (Docker secret → vault → self-register).
//!
//! Plaintext env var overrides are intentionally NOT supported — they are
//! insecure (visible in /proc, env dumps, docker inspect).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use credential_gateway::{
    CredentialRecord, CredentialVault, VAULT_KEY_MATRIX_PASSWORD, VAULT_KEY_MATRIX_ROOM_ALERTS,
    VAULT_KEY_MATRIX_ROOM_CONTROL, VAULT_KEY_MATRIX_ROOM_CREDENTIALS, VAULT_KEY_MATRIX_ROOM_GOALS,
    VAULT_KEY_MATRIX_ROOM_INTAKE, VAULT_KEY_MATRIX_ROOM_STATUS, VAULT_KEY_MATRIX_ROOM_STREAM,
};
use symbiotic_matrix::registration::{
    ensure_rooms, generate_password, login_password, register_user, RegistrationResult,
};

use crate::secrets::read_docker_secret;

/// Configuration for the bootstrap process.
#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    /// Matrix homeserver URL (required).
    pub homeserver: String,
    /// Matrix username (default: "symbiotic-daemon").
    pub username: String,
    /// Server name for room aliases (e.g. "symbiotic.local").
    pub server_name: String,
    /// Allow self-registration when no credentials found.
    pub allow_self_registration: bool,
}

/// Result of the bootstrap process.
#[derive(Debug, Clone)]
pub struct BootstrapResult {
    /// Resolved password (from Docker secret, vault, or generated).
    pub password: Option<String>,
    /// Access token obtained from login.
    pub access_token: Option<String>,
    /// Room ID map: role name → room_id.
    pub rooms: HashMap<String, String>,
    /// Whether we self-registered a new user.
    pub self_registered: bool,
}

/// Run the full bootstrap sequence:
/// 1. Resolve password from secure sources (Docker secret → vault → self-register)
/// 2. Log in to Matrix
/// 3. Store credentials in vault
/// 4. Resolve/create rooms (vault → create)
/// 5. Store room IDs in vault
pub async fn bootstrap(
    config: &BootstrapConfig,
    vault: &Arc<dyn CredentialVault>,
) -> Result<BootstrapResult> {
    let (password, self_registered) = resolve_password(config, vault).await?;

    // Login to get an access token for room operations.
    // If secret-backed credentials point to a not-yet-created user, allow a
    // self-registration fallback when policy allows it.
    let access_token = match login_password(&config.homeserver, &config.username, &password).await {
        Ok(token) => token,
        Err(login_err) if config.allow_self_registration => {
            println!(
                "bootstrap: initial login failed; attempting self-registration fallback for {}",
                config.username
            );
            match register_user(&config.homeserver, &config.username, &password).await? {
                RegistrationResult::Created => {
                    println!(
                        "bootstrap: self-registration fallback created user {}",
                        config.username
                    );
                }
                RegistrationResult::AlreadyExists => {
                    return Err(login_err).context(
                        "bootstrap: login failed and fallback registration reported existing user",
                    );
                }
            }

            login_password(&config.homeserver, &config.username, &password)
                .await
                .context("bootstrap: login failed after fallback self-registration")?
        }
        Err(login_err) => {
            return Err(login_err).context("bootstrap: login failed");
        }
    };
    println!("bootstrap: logged in as {}", config.username);

    // Store password in vault for future boots
    vault
        .put(CredentialRecord {
            service: VAULT_KEY_MATRIX_PASSWORD.to_string(),
            username: config.username.clone(),
            secret: password.clone(),
            totp_secret: None,
        })
        .context("bootstrap: failed to store password in vault")?;

    // --- Room resolution ---
    let rooms = resolve_rooms(config, vault, &access_token).await?;

    Ok(BootstrapResult {
        password: Some(password),
        access_token: Some(access_token),
        rooms,
        self_registered,
    })
}

/// Resolve password from secure sources. Returns (password, self_registered).
///
/// Priority chain (first match wins):
/// 1. Docker secret at /run/secrets/symbiotic_matrix_password
/// 2. Encrypted vault
/// 3. Self-register + generate password (when allowed)
async fn resolve_password(
    config: &BootstrapConfig,
    vault: &Arc<dyn CredentialVault>,
) -> Result<(String, bool)> {
    // 0. Environment variable (native dev mode)
    if let Ok(pw) = std::env::var("SYMBIOTIC_MATRIX_PASSWORD") {
        if !pw.is_empty() {
            println!("bootstrap: using password from environment variable");
            return Ok((pw, false));
        }
    }

    // 1. Docker secret
    if let Some(pw) = read_docker_secret("symbiotic_matrix_password") {
        println!("bootstrap: using password from Docker secret");
        return Ok((pw, false));
    }

    // 2. Vault
    if let Ok(Some(record)) = vault.get(VAULT_KEY_MATRIX_PASSWORD) {
        if !record.secret.is_empty() {
            println!("bootstrap: using password from vault");
            return Ok((record.secret, false));
        }
    }

    // 3. Self-register
    if config.allow_self_registration {
        println!("bootstrap: no credentials found, attempting self-registration");
        let password = generate_password(16); // 32 hex chars
        match register_user(&config.homeserver, &config.username, &password).await? {
            RegistrationResult::Created => {
                println!("bootstrap: registered new user {}", config.username);
            }
            RegistrationResult::AlreadyExists => {
                // User exists but we have no password — can't proceed
                return Err(anyhow!(
                    "bootstrap: user {} already exists but no password available. \
                     Provide a Docker secret at /run/secrets/symbiotic_matrix_password.",
                    config.username
                ));
            }
        }
        return Ok((password, true));
    }

    Err(anyhow!(
        "bootstrap: no Matrix password found. \
         Provide a Docker secret at /run/secrets/symbiotic_matrix_password, \
         or enable self-registration with SYMBIOTIC_BOOTSTRAP_SELF_REGISTER=true"
    ))
}

/// Resolve room IDs from vault or by creating them.
async fn resolve_rooms(
    config: &BootstrapConfig,
    vault: &Arc<dyn CredentialVault>,
    access_token: &str,
) -> Result<HashMap<String, String>> {
    let room_defs: &[(&str, &str)] = &[
        ("control", VAULT_KEY_MATRIX_ROOM_CONTROL),
        ("intake", VAULT_KEY_MATRIX_ROOM_INTAKE),
        ("alerts", VAULT_KEY_MATRIX_ROOM_ALERTS),
        ("status", VAULT_KEY_MATRIX_ROOM_STATUS),
        ("goals", VAULT_KEY_MATRIX_ROOM_GOALS),
        ("stream", VAULT_KEY_MATRIX_ROOM_STREAM),
    ];

    let mut rooms = HashMap::new();
    let mut missing_aliases = Vec::new();

    for &(alias, vault_key) in room_defs {
        // 1. Vault
        if let Ok(Some(record)) = vault.get(vault_key) {
            if !record.secret.is_empty() {
                rooms.insert(alias.to_string(), record.secret);
                continue;
            }
        }
        // 2. Needs creation
        missing_aliases.push(alias);
    }

    // Also ensure credentials room exists
    let credentials_from_vault = vault
        .get(VAULT_KEY_MATRIX_ROOM_CREDENTIALS)
        .ok()
        .flatten()
        .filter(|r| !r.secret.is_empty());
    if credentials_from_vault.is_none() {
        missing_aliases.push("credentials");
    } else if let Some(record) = credentials_from_vault {
        rooms.insert("credentials".to_string(), record.secret);
    }

    if !missing_aliases.is_empty() {
        let alias_refs: Vec<&str> = missing_aliases.to_vec();
        println!(
            "bootstrap: creating missing rooms: {}",
            alias_refs.join(", ")
        );
        let created = ensure_rooms(
            &config.homeserver,
            access_token,
            &config.server_name,
            &alias_refs,
        )
        .await
        .context("bootstrap: failed to ensure rooms")?;

        let vault_key_map: HashMap<&str, &str> = [
            ("control", VAULT_KEY_MATRIX_ROOM_CONTROL),
            ("intake", VAULT_KEY_MATRIX_ROOM_INTAKE),
            ("alerts", VAULT_KEY_MATRIX_ROOM_ALERTS),
            ("status", VAULT_KEY_MATRIX_ROOM_STATUS),
            ("credentials", VAULT_KEY_MATRIX_ROOM_CREDENTIALS),
            ("goals", VAULT_KEY_MATRIX_ROOM_GOALS),
            ("stream", VAULT_KEY_MATRIX_ROOM_STREAM),
        ]
        .into_iter()
        .collect();

        for (alias, created_room) in &created {
            let action = if created_room.already_existed {
                "resolved"
            } else {
                "created"
            };
            println!(
                "bootstrap: {} room {} → {}",
                action, alias, created_room.room_id
            );
            rooms.insert(alias.clone(), created_room.room_id.clone());

            // Store in vault
            if let Some(&vault_key) = vault_key_map.get(alias.as_str()) {
                vault
                    .put(CredentialRecord {
                        service: vault_key.to_string(),
                        username: alias.clone(),
                        secret: created_room.room_id.clone(),
                        totp_secret: None,
                    })
                    .with_context(|| format!("bootstrap: failed to store room {alias} in vault"))?;
            }
        }
    }

    Ok(rooms)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// In-memory vault for testing.
    struct MemVault {
        store: Mutex<HashMap<String, CredentialRecord>>,
    }

    impl MemVault {
        fn new() -> Self {
            Self {
                store: Mutex::new(HashMap::new()),
            }
        }
    }

    impl CredentialVault for MemVault {
        fn put(&self, credential: CredentialRecord) -> Result<()> {
            self.store
                .lock()
                .map_err(|_| anyhow!("lock poisoned"))?
                .insert(credential.service.clone(), credential);
            Ok(())
        }

        fn get(&self, service: &str) -> Result<Option<CredentialRecord>> {
            Ok(self
                .store
                .lock()
                .map_err(|_| anyhow!("lock poisoned"))?
                .get(service)
                .cloned())
        }

        fn delete(&self, service: &str) -> Result<bool> {
            Ok(self
                .store
                .lock()
                .map_err(|_| anyhow!("lock poisoned"))?
                .remove(service)
                .is_some())
        }

        fn list_services(&self) -> Result<Vec<String>> {
            let store = self.store.lock().map_err(|_| anyhow!("lock poisoned"))?;
            let mut services: Vec<String> = store.keys().cloned().collect();
            services.sort();
            Ok(services)
        }
    }

    #[test]
    fn vault_password_used() {
        let vault: Arc<dyn CredentialVault> = Arc::new(MemVault::new());
        vault
            .put(CredentialRecord {
                service: VAULT_KEY_MATRIX_PASSWORD.to_string(),
                username: "daemon".to_string(),
                secret: "vault-password".to_string(),
                totp_secret: None,
            })
            .unwrap();

        let config = BootstrapConfig {
            homeserver: "http://localhost:8008".to_string(),
            username: "daemon".to_string(),
            server_name: "symbiotic.local".to_string(),
            allow_self_registration: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (pw, registered) = rt.block_on(resolve_password(&config, &vault)).unwrap();
        assert_eq!(pw, "vault-password");
        assert!(!registered);
    }

    #[test]
    fn no_credentials_and_no_self_reg_fails() {
        let vault: Arc<dyn CredentialVault> = Arc::new(MemVault::new());
        let config = BootstrapConfig {
            homeserver: "http://localhost:8008".to_string(),
            username: "daemon".to_string(),
            server_name: "symbiotic.local".to_string(),
            allow_self_registration: false,
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(resolve_password(&config, &vault));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("no Matrix password found"));
    }
}
