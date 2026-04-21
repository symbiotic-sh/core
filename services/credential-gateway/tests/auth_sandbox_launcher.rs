use std::path::PathBuf;
use std::time::Duration;

use credential_gateway::auth_engine::{
    AuthEngineError, AuthSandboxLauncher, AuthSandboxLauncherConfig,
};
use credential_gateway::script_registry::ScriptRegistry;
use credential_gateway::{CredentialRecord, GoalScopedVault};

fn create_success_script(dir: &std::path::Path) -> PathBuf {
    let script_path = dir.join("example.com.sh");
    std::fs::write(
        &script_path,
        r#"#!/bin/sh
INPUT=$(cat)
echo '{"success": true, "session": "sandbox_session_token"}'
"#,
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
    }
    script_path
}

#[tokio::test]
async fn launcher_spawns_one_shot_auth_worker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    create_success_script(&scripts_dir);

    let vault_root = temp.path().join("vault");
    let vault = GoalScopedVault::open(&vault_root).expect("open vault");
    vault
        .put_scoped(
            None,
            CredentialRecord {
                service: "example.com".to_string(),
                username: "user".to_string(),
                secret: "pass".to_string(),
                totp_secret: None,
            },
        )
        .expect("seed credential");

    let registry = ScriptRegistry::from_dir(&scripts_dir).expect("load registry");
    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir,
            timeout: Duration::from_secs(5),
            node_bin: None,
            goal_scope: None,
        },
    );

    let output = launcher
        .authenticate("example.com")
        .await
        .expect("authenticate");
    assert!(output.success);
    assert_eq!(output.session.as_deref(), Some("sandbox_session_token"));
}

#[tokio::test]
async fn launcher_surfaces_missing_credentials_from_worker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    create_success_script(&scripts_dir);

    let vault_root = temp.path().join("vault");
    GoalScopedVault::open(&vault_root).expect("open vault");

    let registry = ScriptRegistry::from_dir(&scripts_dir).expect("load registry");
    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir,
            timeout: Duration::from_secs(5),
            node_bin: None,
            goal_scope: None,
        },
    );

    let error = launcher
        .authenticate("example.com")
        .await
        .expect_err("missing credential should fail");
    assert!(
        matches!(
            error,
            AuthEngineError::NoCredentials(_) | AuthEngineError::ScriptFailed(_)
        ),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn launcher_uses_request_goal_scope_for_scoped_vaults() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&scripts_dir).expect("create scripts dir");
    create_success_script(&scripts_dir);

    let vault_root = temp.path().join("vault");
    let vault = GoalScopedVault::open(&vault_root).expect("open vault");
    vault
        .put_scoped(
            Some("wf-1"),
            CredentialRecord {
                service: "example.com".to_string(),
                username: "scoped-user".to_string(),
                secret: "scoped-pass".to_string(),
                totp_secret: None,
            },
        )
        .expect("seed scoped credential");

    let registry = ScriptRegistry::from_dir(&scripts_dir).expect("load registry");
    let launcher = AuthSandboxLauncher::new(
        registry,
        AuthSandboxLauncherConfig {
            worker_bin: PathBuf::from(env!("CARGO_BIN_EXE_credential-gateway")),
            vault_root,
            scripts_dir,
            timeout: Duration::from_secs(5),
            node_bin: None,
            goal_scope: None,
        },
    );

    let output = launcher
        .authenticate_scoped("example.com", Some("wf-1"))
        .await
        .expect("authenticate with scoped vault");
    assert!(output.success);
    assert_eq!(output.session.as_deref(), Some("sandbox_session_token"));

    let error = launcher
        .authenticate("example.com")
        .await
        .expect_err("global vault should not see goal-scoped credential");
    assert!(
        matches!(
            error,
            AuthEngineError::NoCredentials(_) | AuthEngineError::ScriptFailed(_)
        ),
        "unexpected error: {error}"
    );
}
