//! Per-API credential connector for external git push. Scope-guarded
//! materialization — credential never outlives the closure scope.
//! See `docs/design/git-push-session.md`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::GoalScopedVault;

/// Credential kind — determines how the credential materializes for git.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitCredentialKind {
    /// SSH private key. Materialized as a tempfile + `GIT_SSH_COMMAND` env var.
    SshPrivateKey,
    /// HTTPS token (PAT, OAuth2 bearer, etc.). Materialized via `GIT_ASKPASS`
    /// + a short-lived askpass script that echoes the token.
    HttpsToken,
}

/// Errors returned from `with_git_push_session` and related helpers.
#[derive(Debug, Error)]
pub enum GitPushSessionError {
    #[error("credential not found: {0}")]
    CredentialNotFound(String),
    #[error("credential scope denied: {0}")]
    ScopeDenied(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("subprocess failed: {0}")]
    Subprocess(String),
}

/// The materialized env for a single git invocation. Custom `Debug`
/// deliberately redacts all credential material.
pub struct GitPushEnv {
    /// Env vars to pass to `Command.envs(...)`. NEVER logged.
    pub(crate) env_vars: Vec<(String, String)>,
    /// Tempfile paths owned by the session (private key file, askpass script).
    /// Dropped when the session ends; files are explicitly removed on drop.
    _owned_tempfiles: Vec<TempFile>,
}

impl GitPushEnv {
    /// Apply this env to a git Command invocation.
    pub fn apply(&self, cmd: &mut std::process::Command) {
        for (k, v) in &self.env_vars {
            cmd.env(k, v);
        }
    }
}

impl std::fmt::Debug for GitPushEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitPushEnv")
            .field("env_var_count", &self.env_vars.len())
            .field("tempfile_count", &self._owned_tempfiles.len())
            // Deliberately NOT including env_vars or tempfile paths.
            .finish()
    }
}

/// Private tempfile helper. On drop, unlinks the file (best-effort).
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    /// Create a new tempfile in `std::env::temp_dir()` with the given prefix
    /// and Unix mode. Uses `create_new` for race-free perm setting.
    fn create(prefix: &str, mode: u32) -> Result<Self, std::io::Error> {
        let filename = format!("{}{}-{}", prefix, std::process::id(), uuid::Uuid::new_v4());
        let path = std::env::temp_dir().join(filename);

        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        {
            let _ = mode;
        }

        // Open then immediately close to establish the file with correct perms.
        let file = opts.open(&path)?;
        drop(file);

        Ok(Self { path })
    }

    /// Write the given contents to the tempfile (truncating any prior content).
    fn write_all(&self, contents: &[u8]) -> Result<(), std::io::Error> {
        let mut file = OpenOptions::new()
            .append(false)
            .truncate(true)
            .write(true)
            .open(&self.path)?;
        file.write_all(contents)?;
        file.flush()?;
        Ok(())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Shell-escape a string for single-quoted bash/sh use.
///
/// Wraps the input in single quotes and escapes internal single-quotes
/// as `'\''`. Safe for interpolation into `echo '{value}'`.
fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Scope-guarded credential materialization. All cleanup happens when `f`
/// returns (normal or panic) — the `GitPushEnv`'s owned tempfiles are
/// dropped, unlinking them.
///
/// `credential_id` is the opaque handle stored on `RepoCredentialBinding.id`;
/// resolved here as the vault's `service` key (see `GoalScopedVault::get_scoped`).
///
/// `kind` is derived by the caller from the remote URL:
///   - `git@github.com:...` / `ssh://...` → `SshPrivateKey`
///   - `https://...` → `HttpsToken`
pub fn with_git_push_session<R, F>(
    vault: &GoalScopedVault,
    credential_id: &str,
    kind: GitCredentialKind,
    goal_scope: Option<&str>,
    f: F,
) -> Result<R, GitPushSessionError>
where
    F: FnOnce(&GitPushEnv) -> Result<R, GitPushSessionError>,
{
    let record = vault
        .get_scoped(goal_scope, credential_id)
        .map_err(|e| GitPushSessionError::Io(std::io::Error::other(e.to_string())))?
        .ok_or_else(|| GitPushSessionError::CredentialNotFound(credential_id.to_string()))?;

    let env = match kind {
        GitCredentialKind::SshPrivateKey => {
            let tempfile = TempFile::create("symbiotic-push-ssh-", 0o600)?;
            tempfile.write_all(record.secret.as_bytes())?;
            let ssh_cmd = format!(
                "ssh -i {path} -o StrictHostKeyChecking=accept-new -o UserKnownHostsFile=/dev/null -o IdentitiesOnly=yes",
                path = tempfile.path().display()
            );
            GitPushEnv {
                env_vars: vec![("GIT_SSH_COMMAND".to_string(), ssh_cmd)],
                _owned_tempfiles: vec![tempfile],
            }
        }
        GitCredentialKind::HttpsToken => {
            let tempfile = TempFile::create("symbiotic-push-askpass-", 0o700)?;
            let esc_user = shell_escape(&record.username);
            let esc_secret = shell_escape(&record.secret);
            let script = format!(
                "#!/bin/sh\ncase \"$1\" in\n  Username*) echo {esc_user} ;;\n  Password*) echo {esc_secret} ;;\nesac\n"
            );
            tempfile.write_all(script.as_bytes())?;
            let path_str = tempfile.path().display().to_string();
            GitPushEnv {
                env_vars: vec![
                    ("GIT_ASKPASS".to_string(), path_str),
                    ("GIT_TERMINAL_PROMPT".to_string(), "0".to_string()),
                ],
                _owned_tempfiles: vec![tempfile],
            }
        }
    };

    // Invoke closure. `env` is dropped after this scope regardless of
    // normal return, error return, or panic — the Drop impl on owned
    // tempfiles unlinks them.
    f(&env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CredentialRecord;

    use std::panic::AssertUnwindSafe;
    use std::path::PathBuf;
    use std::sync::Mutex;

    fn unique_suffix() -> String {
        format!("{}-{}", std::process::id(), uuid::Uuid::new_v4())
    }

    fn build_vault() -> (tempfile::TempDir, GoalScopedVault) {
        let dir = tempfile::tempdir().expect("create tempdir");
        let vault = GoalScopedVault::open(dir.path()).expect("open vault");
        (dir, vault)
    }

    fn seed_credential(vault: &GoalScopedVault, service: &str, secret: &str) {
        vault
            .put_scoped(
                None,
                CredentialRecord {
                    service: service.to_string(),
                    username: "testuser".to_string(),
                    secret: secret.to_string(),
                    totp_secret: None,
                },
            )
            .expect("put credential");
    }

    fn seed_credential_full(vault: &GoalScopedVault, service: &str, username: &str, secret: &str) {
        vault
            .put_scoped(
                None,
                CredentialRecord {
                    service: service.to_string(),
                    username: username.to_string(),
                    secret: secret.to_string(),
                    totp_secret: None,
                },
            )
            .expect("put credential");
    }

    fn extract_ssh_keypath(env: &GitPushEnv) -> PathBuf {
        // env_vars[0] = ("GIT_SSH_COMMAND", "ssh -i <path> ...")
        let val = &env.env_vars[0].1;
        // Take the substring between "-i " and the next " -o".
        let after_i = val.split("-i ").nth(1).expect("ssh cmd has -i");
        let path_str = after_i.split(" -o").next().expect("path before -o");
        PathBuf::from(path_str)
    }

    fn extract_tempfile_path(env: &GitPushEnv) -> PathBuf {
        // Works for both SSH (via GIT_SSH_COMMAND) and HTTPS (via GIT_ASKPASS).
        for (k, v) in &env.env_vars {
            if k == "GIT_ASKPASS" {
                return PathBuf::from(v);
            }
        }
        extract_ssh_keypath(env)
    }

    #[test]
    fn ssh_materializes_key_with_correct_perms() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:ssh-1-{}", unique_suffix());
        let fake_key = "-----BEGIN OPENSSH PRIVATE KEY-----\nfake-key-contents\n-----END OPENSSH PRIVATE KEY-----\n";
        seed_credential(&vault, &cred_id, fake_key);

        with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::SshPrivateKey,
            None,
            |env| {
                let path = extract_ssh_keypath(env);

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(&path).expect("metadata");
                    let mode = meta.permissions().mode() & 0o777;
                    assert_eq!(mode, 0o600, "ssh key tempfile must be 0600");
                }

                let contents = std::fs::read_to_string(&path).expect("read key");
                assert_eq!(contents, fake_key);
                Ok(())
            },
        )
        .expect("session ok");
    }

    #[test]
    fn ssh_env_vars_set_correctly() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:ssh-2-{}", unique_suffix());
        seed_credential(&vault, &cred_id, "fake-ssh-key");

        with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::SshPrivateKey,
            None,
            |env| {
                assert_eq!(env.env_vars.len(), 1);
                let (k, v) = &env.env_vars[0];
                assert_eq!(k, "GIT_SSH_COMMAND");
                assert!(v.contains("-i "), "GIT_SSH_COMMAND missing -i: {v}");
                assert!(v.contains("accept-new"), "missing accept-new: {v}");
                assert!(
                    v.contains("IdentitiesOnly=yes"),
                    "missing IdentitiesOnly=yes: {v}"
                );
                assert!(
                    v.contains("UserKnownHostsFile=/dev/null"),
                    "missing UserKnownHostsFile=/dev/null: {v}"
                );
                Ok(())
            },
        )
        .expect("session ok");
    }

    #[test]
    fn https_materializes_askpass_script() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:https-1-{}", unique_suffix());
        let username = "gh-user";
        let secret = "ghp_testToken1234";
        seed_credential_full(&vault, &cred_id, username, secret);

        with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::HttpsToken,
            None,
            |env| {
                let path = extract_tempfile_path(env);

                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let meta = std::fs::metadata(&path).expect("metadata");
                    let mode = meta.permissions().mode() & 0o777;
                    assert_eq!(mode, 0o700, "askpass script must be 0700");
                }

                let out_user = std::process::Command::new("sh")
                    .arg(&path)
                    .arg("Username for 'https://github.com': ")
                    .output()
                    .expect("run askpass for username");
                assert!(
                    out_user.status.success(),
                    "askpass username run failed: stderr={}",
                    String::from_utf8_lossy(&out_user.stderr)
                );
                let u = String::from_utf8_lossy(&out_user.stdout).trim().to_string();
                assert_eq!(u, username);

                let out_pass = std::process::Command::new("sh")
                    .arg(&path)
                    .arg("Password for 'https://github.com': ")
                    .output()
                    .expect("run askpass for password");
                assert!(
                    out_pass.status.success(),
                    "askpass password run failed: stderr={}",
                    String::from_utf8_lossy(&out_pass.stderr)
                );
                let p = String::from_utf8_lossy(&out_pass.stdout).trim().to_string();
                assert_eq!(p, secret);
                Ok(())
            },
        )
        .expect("session ok");
    }

    #[test]
    fn https_env_vars_set_correctly() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:https-2-{}", unique_suffix());
        seed_credential_full(&vault, &cred_id, "user", "token");

        with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::HttpsToken,
            None,
            |env| {
                let has_askpass = env
                    .env_vars
                    .iter()
                    .any(|(k, v)| k == "GIT_ASKPASS" && !v.is_empty());
                assert!(has_askpass, "GIT_ASKPASS missing or empty");
                let has_prompt = env
                    .env_vars
                    .iter()
                    .any(|(k, v)| k == "GIT_TERMINAL_PROMPT" && v == "0");
                assert!(has_prompt, "GIT_TERMINAL_PROMPT=0 missing");
                Ok(())
            },
        )
        .expect("session ok");
    }

    #[test]
    fn tempfile_unlinked_on_normal_return() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:cleanup-1-{}", unique_suffix());
        seed_credential(&vault, &cred_id, "key-data");

        let captured: Mutex<Option<PathBuf>> = Mutex::new(None);

        with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::SshPrivateKey,
            None,
            |env| {
                let path = extract_ssh_keypath(env);
                assert!(path.exists(), "tempfile should exist inside closure");
                *captured.lock().unwrap() = Some(path);
                Ok(())
            },
        )
        .expect("session ok");

        let path = captured.lock().unwrap().clone().expect("path captured");
        assert!(
            !path.exists(),
            "tempfile should be unlinked after session returns: {}",
            path.display()
        );
    }

    #[test]
    fn tempfile_unlinked_on_panic() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:cleanup-panic-{}", unique_suffix());
        seed_credential(&vault, &cred_id, "key-data");

        let captured: Mutex<Option<PathBuf>> = Mutex::new(None);

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            with_git_push_session(
                &vault,
                &cred_id,
                GitCredentialKind::SshPrivateKey,
                None,
                |env| -> Result<(), GitPushSessionError> {
                    let path = extract_ssh_keypath(env);
                    *captured.lock().unwrap() = Some(path);
                    panic!("test panic");
                },
            )
        }));
        assert!(
            result.is_err(),
            "panic should propagate through catch_unwind"
        );

        let path = captured
            .lock()
            .unwrap()
            .clone()
            .expect("path captured before panic");
        assert!(
            !path.exists(),
            "tempfile must be unlinked even on panic: {}",
            path.display()
        );
    }

    #[test]
    fn credential_not_found_returns_typed_error() {
        let (_dir, vault) = build_vault();

        let result = with_git_push_session(
            &vault,
            "cred:missing",
            GitCredentialKind::SshPrivateKey,
            None,
            |_env| -> Result<(), GitPushSessionError> { Ok(()) },
        );

        match result {
            Err(GitPushSessionError::CredentialNotFound(id)) => {
                assert_eq!(id, "cred:missing");
            }
            other => panic!("expected CredentialNotFound, got {other:?}"),
        }
    }

    #[test]
    fn closure_result_propagates() {
        let (_dir, vault) = build_vault();
        let cred_id = format!("cred:propagate-{}", unique_suffix());
        seed_credential(&vault, &cred_id, "key");

        let out = with_git_push_session(
            &vault,
            &cred_id,
            GitCredentialKind::SshPrivateKey,
            None,
            |_env| Ok(42_i32),
        )
        .expect("session ok");

        assert_eq!(out, 42);
    }
}
