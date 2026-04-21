//! Sysbox/Docker backend using Bollard.

use std::fs::File;
use std::path::{Path, PathBuf};

use crate::backend::VmBackend;
use crate::types::{ExecResult, FileTransfer, VmCreateRequest, VmInstance, VmState};
use anyhow::Result;
use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, LogOutput, RemoveContainerOptions, StartContainerOptions,
    UploadToContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::Runtime;
use bollard::Docker;
use bollard::API_DEFAULT_VERSION;
use futures::StreamExt;

pub const SYSBOX_RUNTIME_NAME: &str = "sysbox-runc";
const EXEC_EXIT_MARKER: &str = "__SYMBIOTIC_EXEC_EXIT_CODE__:";

pub struct SysboxBackend {
    docker: Docker,
    /// Whether to use sysbox-runc runtime (default: true).
    use_sysbox: bool,
}

impl SysboxBackend {
    pub fn new() -> Result<Self> {
        let use_sysbox = std::env::var("SYMBIOTIC_VM_USE_SYSBOX")
            .ok()
            .map(|value| !matches!(value.as_str(), "0" | "false" | "FALSE"))
            .unwrap_or(true);
        Self::with_runtime(use_sysbox)
    }

    pub fn with_runtime(use_sysbox: bool) -> Result<Self> {
        let docker = connect_docker()?;
        Ok(Self { docker, use_sysbox })
    }

    pub async fn sysbox_runtime_available() -> Result<bool> {
        docker_runtime_available(SYSBOX_RUNTIME_NAME).await
    }

    async fn run_exec_command(&self, id: &str, command: &str) -> Result<ExecOutput> {
        let wrapped_command = wrap_exec_command(command);
        let config = CreateExecOptions {
            cmd: Some(vec!["sh", "-c", &wrapped_command]),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        };

        let exec = self.docker.create_exec(id, config).await?;
        let results = self.docker.start_exec(&exec.id, None).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();

        if let StartExecResults::Attached { mut output, .. } = results {
            while let Some(msg) = output.next().await {
                match msg? {
                    LogOutput::StdOut { message } => stdout.extend_from_slice(&message),
                    LogOutput::StdErr { message } => stderr.extend_from_slice(&message),
                    _ => {}
                }
            }
        }

        let stderr_text = String::from_utf8_lossy(&stderr).to_string();
        if let Some((stderr_without_marker, exit_code)) = extract_exit_code_marker(&stderr_text) {
            return Ok(ExecOutput {
                exit_code,
                stdout,
                stderr: stderr_without_marker,
            });
        }

        let inspect = self.docker.inspect_exec(&exec.id).await?;
        Ok(ExecOutput {
            exit_code: inspect.exit_code.unwrap_or(-1) as i32,
            stdout,
            stderr: stderr_text,
        })
    }
}

fn wrap_exec_command(command: &str) -> String {
    let delimiter = format!("__SYMBIOTIC_EXEC_{}__", uuid::Uuid::new_v4().simple());
    format!(
        "tmp=$(mktemp /tmp/symbiotic-exec.XXXXXX) || exit 97\n\
cat > \"$tmp\" <<'{delimiter}'\n\
{command}\n\
{delimiter}\n\
/bin/sh \"$tmp\"\n\
status=$?\n\
rm -f \"$tmp\"\n\
printf '\\n{EXEC_EXIT_MARKER}%s\\n' \"$status\" >&2\n\
exit 0"
    )
}

fn extract_exit_code_marker(output: &str) -> Option<(String, i32)> {
    let marker_index = output.rfind(EXEC_EXIT_MARKER)?;
    let (before_marker, marker_and_after) = output.split_at(marker_index);
    let marker_value = marker_and_after
        .strip_prefix(EXEC_EXIT_MARKER)?
        .trim()
        .lines()
        .next()?;
    let exit_code = marker_value.parse::<i32>().ok()?;
    Some((before_marker.trim_end_matches('\n').to_string(), exit_code))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

struct ExecOutput {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: String,
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
                anyhow::anyhow!(
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

pub async fn docker_runtime_available(runtime_name: &str) -> Result<bool> {
    let docker = connect_docker()?;
    let info = docker.info().await?;
    Ok(info
        .runtimes
        .as_ref()
        .is_some_and(|runtimes| runtime_configured(runtimes, runtime_name)))
}

fn runtime_configured(
    runtimes: &std::collections::HashMap<String, Runtime>,
    runtime_name: &str,
) -> bool {
    runtimes.contains_key(runtime_name)
}

/// Universal hardening floor: every container created by this backend
/// drops all Linux capabilities, blocks privilege escalation, runs with
/// a read-only root filesystem (caller-provided bind mounts stay
/// writable), and is capped to 256 PIDs. `network_mode = "none"` is
/// applied iff `request.network.deny_all` — the future-work scoped
/// allowlist path leaves Docker's default network in place.
///
/// Extracted into a pure function so the host-config shape is unit-
/// testable without a Docker daemon. Apologies to T116/T62 callers if
/// `readonly_rootfs: true` ever surfaces a write outside `/workspace` —
/// fix the path, not the policy.
pub(crate) fn build_host_config(
    request: &VmCreateRequest,
    use_sysbox: bool,
) -> bollard::service::HostConfig {
    let mut host_config = bollard::service::HostConfig {
        memory: Some(request.resources.memory_mb as i64 * 1024 * 1024),
        nano_cpus: Some((request.resources.cpus as f64 * 1e9) as i64),
        // T128 §13b — universal security cleanup. Applies to every
        // VmCreateRequest including T116 swarm-agent + T62 distillery
        // callers. See chunk file for ratification trail.
        network_mode: if request.network.deny_all {
            Some("none".to_string())
        } else {
            // Future: scoped allowlist via custom bridge networks.
            // Today the swarm path supplies its own allowlist semantically
            // and uses Docker's default bridge.
            None
        },
        cap_drop: Some(vec!["ALL".to_string()]),
        security_opt: Some(vec!["no-new-privileges:true".to_string()]),
        readonly_rootfs: Some(true),
        pids_limit: Some(256),
        ..Default::default()
    };

    if use_sysbox {
        host_config.runtime = Some(SYSBOX_RUNTIME_NAME.to_string());
    }
    if !request.mounts.is_empty() {
        host_config.binds = Some(
            request
                .mounts
                .iter()
                .map(|mount| {
                    if mount.read_only {
                        format!("{}:{}:ro", mount.host_path, mount.vm_path)
                    } else {
                        format!("{}:{}", mount.host_path, mount.vm_path)
                    }
                })
                .collect(),
        );
    }

    host_config
}

#[async_trait]
impl VmBackend for SysboxBackend {
    async fn create(&self, id: &str, request: &VmCreateRequest) -> Result<VmInstance> {
        let host_config = build_host_config(request, self.use_sysbox);

        let config = Config {
            image: Some(request.image.clone()),
            env: if request.env.is_empty() {
                None
            } else {
                Some(request.env.clone())
            },
            host_config: Some(host_config),
            ..Default::default()
        };

        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: id,
                    ..Default::default()
                }),
                config,
            )
            .await?;

        Ok(VmInstance {
            id: id.to_string(),
            image: request.image.clone(),
            state: VmState::Creating,
            resources: request.resources.clone(),
            network: request.network.clone(),
            requesting_agent: request.requesting_agent.clone(),
            purpose: request.purpose.clone(),
            created_at: crate::time_now(),
            started_at: None,
        })
    }

    async fn start(&self, id: &str) -> Result<()> {
        self.docker
            .start_container(id, None::<StartContainerOptions<String>>)
            .await?;
        Ok(())
    }

    async fn exec(&self, id: &str, command: &str) -> Result<ExecResult> {
        let output = self.run_exec_command(id, command).await?;
        Ok(ExecResult {
            exit_code: output.exit_code,
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: output.stderr,
        })
    }

    async fn stop(&self, id: &str) -> Result<()> {
        self.docker.stop_container(id, None).await?;
        Ok(())
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        self.docker
            .remove_container(
                id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await?;
        Ok(())
    }

    async fn transfer(&self, id: &str, _transfer: &FileTransfer) -> Result<()> {
        let transfer = _transfer;
        match transfer.direction {
            crate::types::TransferDirection::HostToVm => {
                let tar_payload =
                    build_upload_tar(Path::new(&transfer.host_path), Path::new(&transfer.vm_path))?;
                let parent = Path::new(&transfer.vm_path)
                    .parent()
                    .unwrap_or_else(|| Path::new("/"));
                self.docker
                    .upload_to_container(
                        id,
                        Some(UploadToContainerOptions {
                            path: parent.to_string_lossy().to_string(),
                            ..Default::default()
                        }),
                        tar_payload.into(),
                    )
                    .await?;
                Ok(())
            }
            crate::types::TransferDirection::VmToHost => {
                let output = self
                    .run_exec_command(id, &format!("cat -- {}", shell_quote(&transfer.vm_path)))
                    .await?;
                if output.exit_code != 0 {
                    return Err(anyhow::anyhow!(
                        "failed to extract '{}' from container: {}",
                        transfer.vm_path,
                        output.stderr.trim()
                    ));
                }
                if let Some(parent) = Path::new(&transfer.host_path).parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&transfer.host_path, output.stdout)?;
                Ok(())
            }
        }
    }

    async fn get_state(&self, id: &str) -> Result<VmState> {
        let inspect = self.docker.inspect_container(id, None).await?;
        let status = inspect
            .state
            .and_then(|s| s.status)
            .map(|s| s.to_string())
            .unwrap_or_default();

        match status.as_str() {
            "created" => Ok(VmState::Creating),
            "running" => Ok(VmState::Running),
            "exited" | "stopped" => Ok(VmState::Stopped),
            _ => Ok(VmState::Creating),
        }
    }
}

fn build_upload_tar(host_path: &Path, vm_path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(host_path)?;
    let metadata = file.metadata()?;
    let file_name = vm_path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("vm_path must have a file name"))?;

    let mut payload = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut payload);
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, file_name, &mut file)?;
        builder.finish()?;
    }

    Ok(payload)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Read;
    use std::path::Path;

    use bollard::models::Runtime;

    use super::{
        build_host_config, build_upload_tar, extract_exit_code_marker, runtime_configured,
        shell_quote, wrap_exec_command, EXEC_EXIT_MARKER, SYSBOX_RUNTIME_NAME,
    };
    use crate::types::{NetworkPolicy, VmCreateRequest, VmResources};

    #[test]
    fn runtime_configured_matches_configured_runtime_name() {
        let mut runtimes = HashMap::new();
        runtimes.insert(
            SYSBOX_RUNTIME_NAME.to_string(),
            Runtime {
                path: Some("sysbox-runc".to_string()),
                runtime_args: None,
                status: None,
            },
        );

        assert!(runtime_configured(&runtimes, SYSBOX_RUNTIME_NAME));
        assert!(!runtime_configured(&runtimes, "runc"));
    }

    #[test]
    fn wrapped_exec_command_emits_exit_marker() {
        let wrapped = wrap_exec_command("echo hello");
        assert!(wrapped.contains("mktemp /tmp/symbiotic-exec."));
        assert!(wrapped.contains(EXEC_EXIT_MARKER));
        assert!(wrapped.contains("echo hello"));
    }

    #[test]
    fn extract_exit_code_marker_parses_and_strips_marker() {
        let (stderr, exit_code) =
            extract_exit_code_marker("warning line\n__SYMBIOTIC_EXEC_EXIT_CODE__:17\n")
                .expect("marker should parse");
        assert_eq!(stderr, "warning line");
        assert_eq!(exit_code, 17);
    }

    #[test]
    fn extract_exit_code_marker_returns_none_when_missing() {
        assert!(extract_exit_code_marker("plain stderr").is_none());
    }

    #[test]
    fn build_upload_tar_uses_vm_filename() {
        let temp = tempfile::tempdir().expect("tempdir");
        let host_file = temp.path().join("input.txt");
        std::fs::write(&host_file, "hello from host").expect("write host file");

        let tar_bytes =
            build_upload_tar(&host_file, Path::new("/workspace/output/report.json")).expect("tar");

        let mut archive = tar::Archive::new(std::io::Cursor::new(tar_bytes));
        let mut entries = archive.entries().expect("entries");
        let mut entry = entries.next().expect("entry").expect("valid tar entry");
        let path = entry.header().path().expect("tar path");
        assert_eq!(path.as_ref(), Path::new("report.json"));

        let mut content = String::new();
        entry
            .read_to_string(&mut content)
            .expect("read tar content");
        assert_eq!(content, "hello from host");
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    fn minimal_request() -> VmCreateRequest {
        VmCreateRequest {
            image: "test-image".to_string(),
            resources: VmResources::default(),
            network: NetworkPolicy::default(),
            inject_files: Vec::new(),
            requesting_agent: "agent-1".to_string(),
            purpose: "unit-test".to_string(),
            env: Vec::new(),
            mounts: Vec::new(),
        }
    }

    #[test]
    fn host_config_enforces_network_isolation_when_deny_all_set() {
        // T128 §13b — `NetworkPolicy { deny_all: true, .. }` must produce
        // `network_mode = "none"` so the container has zero connectivity.
        let request = minimal_request();
        assert!(request.network.deny_all, "default policy must deny_all");

        let host_config = build_host_config(&request, false);
        assert_eq!(
            host_config.network_mode.as_deref(),
            Some("none"),
            "deny_all must map to network_mode=none"
        );
    }

    #[test]
    fn host_config_leaves_network_default_when_deny_all_false() {
        // T116 swarm-agent path supplies allowlists with deny_all=false;
        // Docker's default bridge stays in place (None).
        let mut request = minimal_request();
        request.network = NetworkPolicy {
            deny_all: false,
            allowed_domains: vec!["github.com".to_string()],
            allowed_ports: vec![443],
            dns_servers: vec![],
        };
        let host_config = build_host_config(&request, false);
        assert!(
            host_config.network_mode.is_none(),
            "deny_all=false must leave network_mode unset (Docker default)"
        );
    }

    #[test]
    fn host_config_applies_universal_hardening_flags() {
        // T128 §13b — universal cleanup. Applies to every VmCreateRequest
        // regardless of network policy: drop all caps, no-new-privileges,
        // read-only root, PID cap.
        let request = minimal_request();
        let host_config = build_host_config(&request, true);

        assert_eq!(
            host_config.cap_drop.as_deref(),
            Some(&["ALL".to_string()] as &[String]),
            "all Linux capabilities must be dropped"
        );
        assert_eq!(
            host_config.security_opt.as_deref(),
            Some(&["no-new-privileges:true".to_string()] as &[String]),
            "no-new-privileges must be set"
        );
        assert_eq!(
            host_config.readonly_rootfs,
            Some(true),
            "rootfs must be read-only (mounted /workspace stays writable)"
        );
        assert_eq!(
            host_config.pids_limit,
            Some(256),
            "PID limit must be capped at 256"
        );
        // Sanity: sysbox runtime preserved when requested.
        assert_eq!(host_config.runtime.as_deref(), Some(SYSBOX_RUNTIME_NAME));
    }
}
