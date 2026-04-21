//! Matrix user registration and room bootstrapping.
//!
//! Provides idempotent registration (handles M_USER_IN_USE) and room creation
//! (checks alias before creating). Used by the daemon bootstrap orchestrator.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Result of a user registration attempt.
#[derive(Debug)]
pub enum RegistrationResult {
    /// User was newly registered.
    Created,
    /// User already existed (M_USER_IN_USE) — caller should log in instead.
    AlreadyExists,
}

/// Register a Matrix user via the client API using `m.login.dummy` auth.
///
/// This only works when the homeserver has `allow_registration = true`.
/// Returns `RegistrationResult::AlreadyExists` if the username is taken,
/// so the caller can fall back to password login.
pub async fn register_user(
    homeserver: &str,
    username: &str,
    password: &str,
) -> Result<RegistrationResult> {
    let base = homeserver.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/register");

    let body = serde_json::json!({
        "username": username,
        "password": password,
        "auth": {
            "type": "m.login.dummy"
        },
        "inhibit_login": true
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("failed to send registration request")?;

    let status = resp.status();
    if status.is_success() {
        return Ok(RegistrationResult::Created);
    }

    let resp_body = resp.text().await.unwrap_or_else(|_| String::from("{}"));

    // Check for M_USER_IN_USE error
    if let Ok(error) = serde_json::from_str::<MatrixErrorResponse>(&resp_body) {
        if error.errcode == "M_USER_IN_USE" {
            return Ok(RegistrationResult::AlreadyExists);
        }
        return Err(anyhow!(
            "registration failed: {} — {}",
            error.errcode,
            error.error
        ));
    }

    Err(anyhow!(
        "registration failed with status {}: {}",
        status,
        resp_body
    ))
}

/// Log in to a Matrix homeserver with username/password, returning an access token.
pub async fn login_password(homeserver: &str, username: &str, password: &str) -> Result<String> {
    let base = homeserver.trim_end_matches('/');
    let url = format!("{base}/_matrix/client/v3/login");

    let body = serde_json::json!({
        "type": "m.login.password",
        "identifier": {
            "type": "m.id.user",
            "user": username
        },
        "password": password
    });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("failed to send login request")?;

    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_else(|_| String::from("{}"));

    if !status.is_success() {
        return Err(anyhow!(
            "login failed with status {}: {}",
            status,
            resp_body
        ));
    }

    #[derive(Deserialize)]
    struct LoginResponse {
        access_token: String,
    }
    let parsed: LoginResponse =
        serde_json::from_str(&resp_body).context("failed to parse login response")?;
    Ok(parsed.access_token)
}

/// Room creation result for a single room.
#[derive(Debug, Clone)]
pub struct CreatedRoom {
    pub room_id: String,
    pub alias: String,
    /// True if the room already existed and we resolved the alias.
    pub already_existed: bool,
}

/// Ensure a set of rooms exist by alias, creating any that are missing.
///
/// Returns a map of alias → room_id. Idempotent: checks alias before creating.
/// `server_name` is the local part of the homeserver (e.g. "symbiotic.local").
pub async fn ensure_rooms(
    homeserver: &str,
    access_token: &str,
    server_name: &str,
    aliases: &[&str],
) -> Result<HashMap<String, CreatedRoom>> {
    let base = homeserver.trim_end_matches('/');
    let client = reqwest::Client::new();
    let mut results = HashMap::new();

    for &alias_local in aliases {
        let full_alias = format!("#{}:{server_name}", alias_local);
        // Try to resolve existing alias first
        match resolve_alias(&client, base, access_token, &full_alias).await {
            Ok(room_id) => {
                results.insert(
                    alias_local.to_string(),
                    CreatedRoom {
                        room_id,
                        alias: full_alias,
                        already_existed: true,
                    },
                );
                continue;
            }
            Err(_) => {
                // Alias doesn't exist, create the room
            }
        }

        let room_id = create_room(&client, base, access_token, alias_local)
            .await
            .with_context(|| format!("failed to create room {alias_local}"))?;
        results.insert(
            alias_local.to_string(),
            CreatedRoom {
                room_id,
                alias: full_alias,
                already_existed: false,
            },
        );
    }

    Ok(results)
}

/// Ensure a single room exists by alias, creating it if missing.
///
/// Convenience wrapper around [`ensure_rooms`] for the common case of
/// creating one thread room at a time (e.g. `#thread-goal-abc123`).
pub async fn ensure_single_room(
    homeserver: &str,
    access_token: &str,
    server_name: &str,
    alias_local: &str,
) -> Result<CreatedRoom> {
    let mut result = ensure_rooms(homeserver, access_token, server_name, &[alias_local]).await?;
    result
        .remove(alias_local)
        .ok_or_else(|| anyhow::anyhow!("no room returned for alias {alias_local}"))
}

/// Resolve a room alias to a room ID.
async fn resolve_alias(
    client: &reqwest::Client,
    base: &str,
    access_token: &str,
    full_alias: &str,
) -> Result<String> {
    let encoded_alias =
        url::form_urlencoded::byte_serialize(full_alias.as_bytes()).collect::<String>();
    let url = format!("{base}/_matrix/client/v3/directory/room/{encoded_alias}");

    let resp = client
        .get(&url)
        .bearer_auth(access_token)
        .send()
        .await
        .context("failed to resolve room alias")?;

    if !resp.status().is_success() {
        return Err(anyhow!("alias not found: {}", full_alias));
    }

    #[derive(Deserialize)]
    struct AliasResponse {
        room_id: String,
    }
    let parsed: AliasResponse = resp
        .json()
        .await
        .context("failed to parse alias response")?;
    Ok(parsed.room_id)
}

/// Create a new Matrix room with a local alias.
async fn create_room(
    client: &reqwest::Client,
    base: &str,
    access_token: &str,
    alias_local: &str,
) -> Result<String> {
    let url = format!("{base}/_matrix/client/v3/createRoom");

    let body = serde_json::json!({
        "room_alias_name": alias_local,
        "name": alias_local,
        "visibility": "public",
        "preset": "public_chat",
        "initial_state": [{
            "type": "m.room.encryption",
            "state_key": "",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2"
            }
        }, {
            "type": "m.room.history_visibility",
            "state_key": "",
            "content": {
                "history_visibility": "shared"
            }
        }]
    });

    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await
        .context("failed to send room creation request")?;

    let status = resp.status();
    let resp_body = resp.text().await.unwrap_or_else(|_| String::from("{}"));

    if !status.is_success() {
        return Err(anyhow!(
            "room creation failed with status {}: {}",
            status,
            resp_body
        ));
    }

    #[derive(Deserialize)]
    struct CreateRoomResponse {
        room_id: String,
    }
    let parsed: CreateRoomResponse =
        serde_json::from_str(&resp_body).context("failed to parse room creation response")?;
    Ok(parsed.room_id)
}

/// Generate a random hex password of the given byte length (output is 2x chars).
pub fn generate_password(byte_len: usize) -> String {
    use rand::Rng;
    let mut bytes = vec![0u8; byte_len];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Deserialize)]
struct MatrixErrorResponse {
    errcode: String,
    #[serde(default)]
    error: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_password_correct_length() {
        let pw = generate_password(16);
        assert_eq!(pw.len(), 32);
        assert!(pw.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_password_unique() {
        let pw1 = generate_password(16);
        let pw2 = generate_password(16);
        assert_ne!(pw1, pw2);
    }
}
