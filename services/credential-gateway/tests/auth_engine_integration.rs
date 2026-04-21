//! Integration test: Auth sandbox launcher against a mock login server.
//!
//! This test starts a Node.js mock HTTP server (`scripts/auth/test-server/server.ts`),
//! then runs the `localhost.ts` auth profile through `AuthSandboxLauncher`,
//! which spawns the real `credential-gateway auth-sandbox-run` worker and
//! captures the resulting session token.
//!
//! Requires: Node.js, npx, Playwright browser (chromium) installed.
//! Run with: `cargo test --test auth_engine_integration -- --ignored`

use credential_gateway::auth_engine::{AuthSandboxLauncher, AuthSandboxLauncherConfig};
use credential_gateway::script_registry::ScriptRegistry;
use credential_gateway::{CredentialRecord, GoalScopedVault};
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Guard that kills the server process on drop.
struct ServerGuard {
    child: Child,
    #[allow(dead_code)]
    port: u16,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Resolve the scripts/auth directory relative to the credential-gateway crate.
fn scripts_auth_dir() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent() // services/
        .unwrap()
        .parent() // runtime root
        .unwrap()
        .join("scripts")
        .join("auth")
}

/// Start the mock login server and wait until it prints READY.
fn start_mock_server(port: u16) -> ServerGuard {
    let auth_dir = scripts_auth_dir();
    let server_script = auth_dir.join("test-server").join("server.ts");

    assert!(
        server_script.exists(),
        "Mock server script not found at: {}",
        server_script.display()
    );

    let mut child = Command::new("npx")
        .args([
            "ts-node",
            server_script.to_str().unwrap(),
            &port.to_string(),
        ])
        .current_dir(&auth_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("Failed to start mock server -- is Node.js installed?");

    let stdout = child.stdout.take().expect("stdout piped");
    let reader = BufReader::new(stdout);
    let mut ready = false;

    for line in reader.lines() {
        match line {
            Ok(l) => {
                if l.starts_with("READY:") {
                    ready = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }

    assert!(ready, "Mock server did not emit READY line");

    ServerGuard { child, port }
}

#[tokio::test]
#[ignore] // Requires Node.js + Playwright installed
async fn launcher_authenticates_against_mock_server_success() {
    let port: u16 = 3847;
    let _server = start_mock_server(port);

    let auth_dir = scripts_auth_dir();
    let localhost_script = auth_dir.join("localhost.ts");

    assert!(
        localhost_script.exists(),
        "localhost.ts auth script not found at: {}",
        localhost_script.display()
    );

    let domain = format!("localhost:{port}");

    let mut registry = ScriptRegistry::empty();
    registry.register(&domain, localhost_script);

    let temp = tempfile::tempdir().expect("tempdir");
    let vault_root = temp.path().join("vault");
    let vault = GoalScopedVault::open(&vault_root).expect("open vault");
    vault
        .put_scoped(
            None,
            CredentialRecord {
                service: domain.clone(),
                username: "testuser".to_string(),
                secret: "testpass123".to_string(),
                totp_secret: None,
            },
        )
        .expect("seed credential");

    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir: auth_dir,
            timeout: Duration::from_secs(30),
            node_bin: None,
            goal_scope: None,
        },
    );

    let result = launcher
        .authenticate(&domain)
        .await
        .expect("authenticate should succeed");

    assert!(
        result.success,
        "Expected success but got error: {:?}",
        result.error
    );

    let session = result.session.expect("session should be present");
    assert!(
        session.contains("session_token=abc123"),
        "Session should contain session_token=abc123, got: {}",
        session
    );
}

#[tokio::test]
#[ignore] // Requires Node.js + Playwright installed
async fn launcher_authenticates_against_mock_server_bad_credentials() {
    let port: u16 = 3848;
    let _server = start_mock_server(port);

    let auth_dir = scripts_auth_dir();
    let localhost_script = auth_dir.join("localhost.ts");
    let domain = format!("localhost:{port}");

    let mut registry = ScriptRegistry::empty();
    registry.register(&domain, localhost_script);

    let temp = tempfile::tempdir().expect("tempdir");
    let vault_root = temp.path().join("vault");
    let vault = GoalScopedVault::open(&vault_root).expect("open vault");
    vault
        .put_scoped(
            None,
            CredentialRecord {
                service: domain.clone(),
                username: "wronguser".to_string(),
                secret: "wrongpass".to_string(),
                totp_secret: None,
            },
        )
        .expect("seed credential");

    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir: auth_dir,
            timeout: Duration::from_secs(30),
            node_bin: None,
            goal_scope: None,
        },
    );

    let result = launcher
        .authenticate(&domain)
        .await
        .expect("authenticate call should not error at launcher level");

    assert!(
        !result.success,
        "Expected failure with bad credentials, got success"
    );
    assert!(
        result.error.is_some(),
        "Expected an error message for bad credentials"
    );
}

#[tokio::test]
#[ignore] // Requires Node.js + Playwright installed
async fn launcher_uses_goal_scoped_vault_against_mock_server() {
    let port: u16 = 3849;
    let _server = start_mock_server(port);

    let auth_dir = scripts_auth_dir();
    let localhost_script = auth_dir.join("localhost.ts");
    let domain = format!("localhost:{port}");

    let mut registry = ScriptRegistry::empty();
    registry.register(&domain, localhost_script);

    let temp = tempfile::tempdir().expect("tempdir");
    let vault_root = temp.path().join("vault");
    let vault = GoalScopedVault::open(&vault_root).expect("open vault");
    vault
        .put_scoped(
            Some("wf-login"),
            CredentialRecord {
                service: domain.clone(),
                username: "testuser".to_string(),
                secret: "testpass123".to_string(),
                totp_secret: None,
            },
        )
        .expect("seed scoped credential");

    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir: auth_dir,
            timeout: Duration::from_secs(30),
            node_bin: None,
            goal_scope: None,
        },
    );

    let result = launcher
        .authenticate_scoped(&domain, Some("wf-login"))
        .await
        .expect("scoped auth should succeed");

    assert!(result.success, "Expected scoped authentication to succeed");
    assert!(
        result
            .session
            .as_deref()
            .unwrap_or_default()
            .contains("session_token=abc123"),
        "Expected scoped session capture to include session_token=abc123"
    );
}
