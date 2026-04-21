//! Git server container lifecycle management via bollard.
//!
//! The daemon never runs `git` directly. Instead, it manages a lightweight
//! Alpine+git container that serves bare repositories over HTTP. All git
//! operations (init, clone, push, merge) happen inside this container.
//!
//! The container runs `git-http-backend` behind a lightweight HTTP server,
//! and exposes a `pre-receive` hook that calls back to the daemon for
//! authorization.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use bollard::container::{
    Config, CreateContainerOptions, LogOutput, RemoveContainerOptions, StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::HostConfig;
use bollard::Docker;
use bollard::API_DEFAULT_VERSION;
use chrono::Utc;
use futures::StreamExt;
use tracing::{debug, info, warn};

use crate::types::{
    BranchRule, GitSwarmConfig, MergeRuleSet, SwarmRepo, SwarmRepoId, SwarmRepoStatus,
};

/// Result of executing a command inside the git server container.
#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    /// Returns an error if the command exited with non-zero status.
    pub fn check(&self, context: &str) -> Result<()> {
        if self.exit_code != 0 {
            Err(anyhow!(
                "{}: exit code {} — stderr: {}",
                context,
                self.exit_code,
                self.stderr.trim()
            ))
        } else {
            Ok(())
        }
    }
}

/// Manages the git server container and bare repositories within it.
///
/// The git server is a single persistent container managed for the lifetime
/// of the daemon. Bare repos are created/destroyed inside it on demand.
pub struct GitServerManager {
    docker: Docker,
    config: GitSwarmConfig,
    /// Container ID of the running git server (None if not started).
    container_id: Option<String>,
    /// Active repos tracked in memory.
    repos: HashMap<SwarmRepoId, SwarmRepo>,
}

/// Container name for the git server.
const GIT_SERVER_CONTAINER: &str = "symbiotic-git-server";

impl GitServerManager {
    /// Create a new manager. Does NOT start the container yet.
    pub fn new(config: GitSwarmConfig) -> Result<Self> {
        let docker = connect_docker().context("failed to connect to Docker daemon")?;
        Ok(Self {
            docker,
            config,
            container_id: None,
            repos: HashMap::new(),
        })
    }

    /// Create with an existing Docker client (for testing).
    pub fn with_docker(docker: Docker, config: GitSwarmConfig) -> Self {
        Self {
            docker,
            config,
            container_id: None,
            repos: HashMap::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Container lifecycle
    // -----------------------------------------------------------------------

    /// Start the git server container. Idempotent — if already running, no-op.
    pub async fn start(&mut self) -> Result<()> {
        // Check if container already exists
        if let Ok(inspect) = self
            .docker
            .inspect_container(GIT_SERVER_CONTAINER, None)
            .await
        {
            let status = inspect
                .state
                .as_ref()
                .and_then(|s| s.status)
                .map(|s| s.to_string())
                .unwrap_or_default();

            if status == "running" {
                self.container_id = inspect.id.clone();
                info!(
                    container = GIT_SERVER_CONTAINER,
                    "git server already running"
                );
                return Ok(());
            }

            // Exists but stopped — remove and recreate
            warn!(
                container = GIT_SERVER_CONTAINER,
                status = %status,
                "git server exists but not running, recreating"
            );
            let _ = self
                .docker
                .remove_container(
                    GIT_SERVER_CONTAINER,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
        }

        // Create and start the container
        let host_config = HostConfig {
            // Expose git HTTP on the Docker bridge interface
            port_bindings: Some({
                let mut map = HashMap::new();
                map.insert(
                    format!("{}/tcp", self.config.git_server_port),
                    Some(vec![bollard::models::PortBinding {
                        host_ip: Some(self.config.git_server_bind.clone()),
                        host_port: Some(self.config.git_server_port.to_string()),
                    }]),
                );
                map
            }),
            // Auto-restart so the git server survives daemon restarts
            restart_policy: Some(bollard::models::RestartPolicy {
                name: Some(bollard::models::RestartPolicyNameEnum::UNLESS_STOPPED),
                ..Default::default()
            }),
            ..Default::default()
        };

        let container_config = Config {
            image: Some(self.config.git_server_image.clone()),
            host_config: Some(host_config),
            env: Some(vec![
                format!("GIT_HTTP_EXPORT_ALL=1"),
                format!("REPOS_PATH={}", self.config.repos_base_path),
                format!(
                    "DAEMON_HTTP_PORT={}",
                    std::env::var("SYMBIOTIC_HTTP_PORT").unwrap_or_else(|_| "8090".to_string())
                ),
                format!(
                    "AUTH_CALLBACK_URL=http://host.docker.internal:{}/api/git/authorize",
                    std::env::var("SYMBIOTIC_HTTP_PORT").unwrap_or_else(|_| "8090".to_string())
                ),
            ]),
            ..Default::default()
        };

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: GIT_SERVER_CONTAINER,
                    ..Default::default()
                }),
                container_config,
            )
            .await
            .context("failed to create git server container")?;

        self.docker
            .start_container(GIT_SERVER_CONTAINER, None::<StartContainerOptions<String>>)
            .await
            .context("failed to start git server container")?;

        let inspect = self
            .docker
            .inspect_container(GIT_SERVER_CONTAINER, None)
            .await?;
        self.container_id = inspect.id.clone();

        info!(
            container = GIT_SERVER_CONTAINER,
            image = %self.config.git_server_image,
            bind = %self.config.git_server_bind,
            port = self.config.git_server_port,
            "git server started"
        );

        Ok(())
    }

    /// Stop and remove the git server container.
    pub async fn stop(&mut self) -> Result<()> {
        if self.container_id.is_some() {
            self.docker
                .remove_container(
                    GIT_SERVER_CONTAINER,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await
                .context("failed to remove git server container")?;
            self.container_id = None;
            info!(container = GIT_SERVER_CONTAINER, "git server stopped");
        }
        Ok(())
    }

    /// Check if the git server container is running.
    pub fn is_running(&self) -> bool {
        self.container_id.is_some()
    }

    // -----------------------------------------------------------------------
    // Repository management (exec into container)
    // -----------------------------------------------------------------------

    /// Create a new bare git repository inside the server container.
    ///
    /// Initializes with `git init --bare`, disables hooks, and creates an
    /// empty initial commit on `main` so agents have a branch to base off.
    pub async fn create_repo(
        &mut self,
        id: &str,
        branch_rules: Option<Vec<BranchRule>>,
        merge_rules: Option<MergeRuleSet>,
    ) -> Result<SwarmRepo> {
        self.ensure_running()?;

        let repo_path = format!("{}/{}.git", self.config.repos_base_path, id);

        // Create bare repo
        self.exec(&format!("git init --bare {}", repo_path))
            .await?
            .check("git init --bare")?;

        // Allow push from any client (no receive.denyCurrentBranch issue on bare)
        self.exec(&format!(
            "git -C {} config http.receivepack true",
            repo_path
        ))
        .await?
        .check("enable http.receivepack")?;

        // Create an empty initial commit on `main` so there's a base branch.
        // We do this via a temporary worktree inside the container.
        let init_script = format!(
            r#"
            cd /tmp && rm -rf _init_{id} &&
            git clone {repo_path} _init_{id} &&
            cd _init_{id} &&
            git checkout -b main &&
            git config user.email "swarm@symbiotic.sh" &&
            git config user.name "Swarm Init" &&
            git commit --allow-empty -m "chore: initialize swarm repo" &&
            git push origin main &&
            cd /tmp && rm -rf _init_{id}
            "#,
            id = id,
            repo_path = repo_path,
        );
        self.exec(&init_script)
            .await?
            .check("create initial commit")?;

        self.exec(&install_hook_command(&repo_path))
            .await?
            .check("install pre-receive hook")?;

        let repo = SwarmRepo {
            id: id.to_string(),
            container_path: repo_path,
            created_at: Utc::now(),
            status: SwarmRepoStatus::Active,
            branch_rules: branch_rules.unwrap_or_else(|| self.config.default_branch_rules.clone()),
            default_merge_rules: merge_rules
                .unwrap_or_else(|| self.config.default_merge_rules.clone()),
        };

        self.repos.insert(id.to_string(), repo.clone());
        info!(repo_id = id, "created swarm repo");
        Ok(repo)
    }

    /// Destroy a bare repository inside the server container.
    pub async fn destroy_repo(&mut self, id: &str) -> Result<()> {
        self.ensure_running()?;

        let repo_path = format!("{}/{}.git", self.config.repos_base_path, id);
        self.exec(&format!("rm -rf {}", repo_path))
            .await?
            .check("destroy repo")?;

        self.repos.remove(id);
        info!(repo_id = id, "destroyed swarm repo");
        Ok(())
    }

    /// Get the HTTP clone URL for a repo (accessible from Docker network).
    pub fn repo_url(&self, id: &str) -> String {
        format!(
            "http://{}:{}/{}.git",
            self.config.git_server_bind, self.config.git_server_port, id
        )
    }

    /// List branches in a repo.
    pub async fn list_branches(&self, id: &str) -> Result<Vec<String>> {
        self.ensure_running()?;

        let repo_path = format!("{}/{}.git", self.config.repos_base_path, id);
        let output = self
            .exec(&format!(
                "git -C {} for-each-ref --format='%(refname:short)' refs/heads/",
                repo_path
            ))
            .await?;
        output.check("list branches")?;

        Ok(output
            .stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect())
    }

    /// Get the SHA of a branch tip.
    pub async fn branch_sha(&self, id: &str, branch: &str) -> Result<String> {
        self.ensure_running()?;

        let repo_path = format!("{}/{}.git", self.config.repos_base_path, id);
        let output = self
            .exec(&format!(
                "git -C {} rev-parse refs/heads/{}",
                repo_path, branch
            ))
            .await?;
        output.check("rev-parse")?;
        Ok(output.stdout.trim().to_string())
    }

    /// Fast-forward merge a branch into base (typically main).
    ///
    /// This updates the base ref to point to the branch tip, but only if
    /// the branch is a descendant of the current base. No working directory
    /// is needed — this operates purely on refs in the bare repo.
    pub async fn fast_forward_merge(&self, id: &str, branch: &str, base: &str) -> Result<String> {
        self.ensure_running()?;

        let repo_path = format!("{}/{}.git", self.config.repos_base_path, id);

        // Verify branch is fast-forwardable
        let check = self
            .exec(&format!(
                "git -C {} merge-base --is-ancestor refs/heads/{} refs/heads/{}",
                repo_path, base, branch
            ))
            .await?;
        if check.exit_code != 0 {
            return Err(anyhow!(
                "branch '{}' is not fast-forwardable onto '{}' — manual merge required",
                branch,
                base
            ));
        }

        // Get the branch tip SHA
        let branch_sha = self.branch_sha(id, branch).await?;

        // Update the base ref
        self.exec(&format!(
            "git -C {} update-ref refs/heads/{} {}",
            repo_path, base, branch_sha
        ))
        .await?
        .check("update-ref")?;

        info!(
            repo_id = id,
            branch = branch,
            base = base,
            sha = %branch_sha,
            "fast-forward merged"
        );
        Ok(branch_sha)
    }

    /// Get a repo by ID.
    pub fn get_repo(&self, id: &str) -> Option<&SwarmRepo> {
        self.repos.get(id)
    }

    /// Get mutable repo by ID.
    pub fn get_repo_mut(&mut self, id: &str) -> Option<&mut SwarmRepo> {
        self.repos.get_mut(id)
    }

    /// List all active repos.
    pub fn list_repos(&self) -> Vec<&SwarmRepo> {
        self.repos.values().collect()
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    fn ensure_running(&self) -> Result<()> {
        if self.container_id.is_none() {
            return Err(anyhow!(
                "git server container not running — call start() first"
            ));
        }
        Ok(())
    }

    /// Execute a shell command inside the git server container.
    async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let container = self.container_id.as_deref().unwrap_or(GIT_SERVER_CONTAINER);

        debug!(
            container = container,
            command = command,
            "exec in git server"
        );

        let exec_config = CreateExecOptions {
            cmd: Some(vec!["sh", "-c", command]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = self.docker.create_exec(container, exec_config).await?;
        let results = self.docker.start_exec(&exec.id, None).await?;

        let mut stdout = String::new();
        let mut stderr = String::new();

        if let StartExecResults::Attached { mut output, .. } = results {
            while let Some(msg) = output.next().await {
                match msg? {
                    LogOutput::StdOut { message } => {
                        stdout.push_str(&String::from_utf8_lossy(&message));
                    }
                    LogOutput::StdErr { message } => {
                        stderr.push_str(&String::from_utf8_lossy(&message));
                    }
                    _ => {}
                }
            }
        }

        let inspect = self.docker.inspect_exec(&exec.id).await?;
        let exit_code = inspect.exit_code.unwrap_or(-1) as i32;

        if exit_code != 0 {
            debug!(
                exit_code = exit_code,
                stderr = %stderr.trim(),
                "git server exec failed"
            );
        }

        Ok(ExecOutput {
            exit_code,
            stdout,
            stderr,
        })
    }
}

fn connect_docker() -> Result<Docker> {
    if std::env::var("DOCKER_HOST")
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Ok(Docker::connect_with_defaults()?);
    }

    match Docker::connect_with_local_defaults() {
        Ok(docker) => Ok(docker),
        Err(local_error) => {
            let Some(path) = docker_desktop_socket_path() else {
                return Err(local_error.into());
            };
            let path_str = path.to_string_lossy().to_string();
            Docker::connect_with_unix(&path_str, 120, API_DEFAULT_VERSION).map_err(|desktop_error| {
                anyhow!(
                    "failed to connect to Docker daemon via default socket ({local_error}) or Docker Desktop socket {} ({desktop_error})",
                    path.display()
                )
            })
        }
    }
}

fn docker_desktop_socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".docker/run/docker.sock");
    path.exists().then_some(path)
}

fn repo_hook_path(repo_path: &str) -> String {
    format!("{repo_path}/hooks/pre-receive")
}

fn install_hook_command(repo_path: &str) -> String {
    let hook_path = repo_hook_path(repo_path);
    format!("cp /usr/local/bin/git-pre-receive-hook {hook_path} && chmod +x {hook_path}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::GitSwarmConfig;

    /// Build a GitServerManager for unit tests without a live Docker connection.
    fn test_manager(running: bool) -> GitServerManager {
        let config = GitSwarmConfig::default();
        let docker = connect_docker()
            .or_else(|_| Docker::connect_with_http_defaults())
            .expect("test requires some reachable Docker constructor to succeed");
        GitServerManager {
            docker,
            config,
            container_id: if running {
                Some("test-container".to_string())
            } else {
                None
            },
            repos: HashMap::new(),
        }
    }

    #[test]
    fn repo_url_format() {
        let mgr = test_manager(true);
        let url = mgr.repo_url("task-116");
        assert_eq!(url, "http://172.17.0.1:80/task-116.git");
    }

    #[test]
    fn ensure_running_fails_when_not_started() {
        let mgr = test_manager(false);
        assert!(mgr.ensure_running().is_err());
    }

    #[test]
    fn ensure_running_ok_when_started() {
        let mgr = test_manager(true);
        assert!(mgr.ensure_running().is_ok());
    }

    #[test]
    fn is_running_reflects_state() {
        assert!(!test_manager(false).is_running());
        assert!(test_manager(true).is_running());
    }

    #[test]
    fn repo_hook_path_targets_bare_repo_hooks_dir() {
        assert_eq!(
            repo_hook_path("/repos/task-116.git"),
            "/repos/task-116.git/hooks/pre-receive"
        );
    }

    #[test]
    fn install_hook_command_copies_embedded_hook_into_repo() {
        let command = install_hook_command("/repos/task-116.git");
        assert!(command.contains("cp /usr/local/bin/git-pre-receive-hook"));
        assert!(command.contains("/repos/task-116.git/hooks/pre-receive"));
        assert!(command.contains("chmod +x"));
    }
}
