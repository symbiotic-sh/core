//! VM sandboxing for isolated agent execution.
//!
//! Provides capability-gated VM lifecycle management with audit logging
//! and file bridge for controlled host-VM file transfer.

pub mod backend;
pub mod backends;
pub mod file_bridge;
pub mod manager;
pub mod mock_backend;
pub mod types;

use std::time::{SystemTime, UNIX_EPOCH};

/// Current unix timestamp in seconds.
pub fn time_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after epoch")
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use symbiotic_trust::{AccessBroker, AgentTrustLevel, CapabilityToken};

    use crate::backend::VmBackend;
    use crate::file_bridge::FileBridge;
    use crate::manager::VmManager;
    use crate::mock_backend::MockBackend;
    use crate::types::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "symbiotic-vm-{name}-{}-{}",
            std::process::id(),
            crate::time_now()
        ));
        fs::create_dir_all(&dir).expect("temp test dir should be created");
        dir
    }

    fn vm_scopes() -> HashSet<String> {
        [
            "vm.create",
            "vm.exec",
            "vm.destroy",
            "vm.file.inject",
            "vm.file.extract",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn setup_broker(agent_id: &str, now: u64) -> (AccessBroker, String) {
        let token_id = "tok-vm-1".to_string();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: token_id.clone(),
            subject: agent_id.to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: vm_scopes(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });
        (broker, token_id)
    }

    fn setup_manager() -> VmManager {
        let backend = Box::new(MockBackend::new());
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        VmManager::new(
            backend,
            Path::new("/project/data/runtime/vm-audit.jsonl"),
            bridge,
        )
    }

    fn create_request(agent_id: &str) -> VmCreateRequest {
        VmCreateRequest {
            image: "sandbox-code-v1".to_string(),
            resources: VmResources::default(),
            network: NetworkPolicy::default(),
            inject_files: Vec::new(),
            requesting_agent: agent_id.to_string(),
            purpose: "test execution".to_string(),
            env: Vec::new(),
            mounts: Vec::new(),
        }
    }

    #[derive(Clone, Default)]
    struct RecordingBackend {
        transfers: Arc<Mutex<Vec<FileTransfer>>>,
    }

    #[async_trait]
    impl VmBackend for RecordingBackend {
        async fn create(&self, id: &str, request: &VmCreateRequest) -> Result<VmInstance> {
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

        async fn start(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        async fn exec(&self, _id: &str, _command: &str) -> Result<ExecResult> {
            Ok(ExecResult {
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            })
        }

        async fn stop(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        async fn destroy(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        async fn transfer(&self, _id: &str, transfer: &FileTransfer) -> Result<()> {
            self.transfers.lock().expect("lock").push(transfer.clone());
            Ok(())
        }

        async fn get_state(&self, _id: &str) -> Result<VmState> {
            Ok(VmState::Running)
        }
    }

    // --- VM lifecycle tests ---

    #[tokio::test]
    async fn test_vm_lifecycle_create_start_exec_stop_destroy() {
        let agent_id = "agent-test-1";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        // Create
        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create should succeed");
        assert!(mgr.get_instance(&vm_id).is_some());
        assert_eq!(mgr.get_instance(&vm_id).unwrap().state, VmState::Creating);

        // Start
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start should succeed");
        assert_eq!(mgr.get_instance(&vm_id).unwrap().state, VmState::Running);
        assert_eq!(mgr.get_instance(&vm_id).unwrap().started_at, Some(now + 1));

        // Exec
        let result = mgr
            .exec(
                &vm_id,
                "cargo test",
                agent_id,
                &mut broker,
                &token_id,
                now + 2,
            )
            .await
            .expect("exec should succeed");
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("cargo test"));

        // Stop
        mgr.stop(&vm_id, agent_id, &mut broker, &token_id, now + 3)
            .await
            .expect("stop should succeed");
        assert_eq!(mgr.get_instance(&vm_id).unwrap().state, VmState::Stopped);

        // Destroy
        mgr.destroy(&vm_id, agent_id, &mut broker, &token_id, now + 4)
            .await
            .expect("destroy should succeed");
        assert!(mgr.get_instance(&vm_id).is_none());
    }

    // --- Capability gating tests ---

    #[tokio::test]
    async fn test_create_denied_without_vm_create_scope() {
        let agent_id = "agent-no-vm";
        let now = 1_000_000u64;
        let token_id = "tok-limited".to_string();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: token_id.clone(),
            subject: agent_id.to_string(),
            trust_level: AgentTrustLevel::ArchiveWrite,
            scopes: ["archive.read".to_string()].into_iter().collect(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        let mut mgr = setup_manager();
        let result = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("scope not permitted"));
    }

    #[tokio::test]
    async fn test_create_denied_with_insufficient_trust_level() {
        let agent_id = "agent-readonly";
        let now = 1_000_000u64;
        let token_id = "tok-readonly".to_string();
        let mut broker = AccessBroker::new();
        broker.issue_token(CapabilityToken {
            token_id: token_id.clone(),
            subject: agent_id.to_string(),
            trust_level: AgentTrustLevel::ReadOnly,
            scopes: vm_scopes(),
            expires_at: now + 3600,
            one_time: false,
            consumed: false,
            goal_scope: None,
        });

        let mut mgr = setup_manager();
        let result = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("insufficient trust"));
    }

    // --- Resource limit / timeout tests ---

    #[tokio::test]
    async fn test_reap_expired_vms() {
        let agent_id = "agent-test-2";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        // Create with short timeout
        let mut request = create_request(agent_id);
        request.resources.timeout_secs = 60;
        let vm_id = mgr
            .create(request, &mut broker, &token_id, now)
            .await
            .expect("create");
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start");

        // Not expired yet
        let reaped = mgr.reap_expired(now + 30).await.expect("reap");
        assert!(reaped.is_empty());
        assert!(mgr.get_instance(&vm_id).is_some());

        // Now expired
        let reaped = mgr.reap_expired(now + 62).await.expect("reap");
        assert_eq!(reaped.len(), 1);
        assert_eq!(reaped[0], vm_id);
        assert!(mgr.get_instance(&vm_id).is_none());
    }

    // --- File bridge tests ---

    #[tokio::test]
    async fn test_file_bridge_allows_project_path() {
        let agent_id = "agent-test-3";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let root = temp_test_dir("bridge-allow");
        let project = root.join("project");
        let data = project.join("data");
        fs::create_dir_all(project.join("src")).expect("project src should exist");
        fs::create_dir_all(&data).expect("data dir should exist");
        let source_file = project.join("src/main.rs");
        fs::write(&source_file, "fn main() {}\n").expect("source file should be written");

        let backend = Box::new(MockBackend::new());
        let bridge = FileBridge::new(&project, &data);
        let audit_path = data.join("runtime/vm-audit.jsonl");
        let mut mgr = VmManager::new(backend, &audit_path, bridge);

        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start");

        let transfer = FileTransfer {
            host_path: source_file.to_string_lossy().to_string(),
            vm_path: "/workspace/main.rs".to_string(),
            direction: TransferDirection::HostToVm,
        };
        let result = mgr
            .transfer_file(&vm_id, &transfer, agent_id, &mut broker, &token_id, now + 2)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_file_bridge_denies_outside_project() {
        let agent_id = "agent-test-4";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let root = temp_test_dir("bridge-deny");
        let project = root.join("project");
        let data = project.join("data");
        fs::create_dir_all(&project).expect("project dir should exist");
        fs::create_dir_all(&data).expect("data dir should exist");
        let outside = root.join("outside.txt");
        fs::write(&outside, "nope").expect("outside file should exist");

        let backend = Box::new(MockBackend::new());
        let bridge = FileBridge::new(&project, &data);
        let audit_path = data.join("runtime/vm-audit.jsonl");
        let mut mgr = VmManager::new(backend, &audit_path, bridge);

        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start");

        let transfer = FileTransfer {
            host_path: outside.to_string_lossy().to_string(),
            vm_path: "/workspace/passwd".to_string(),
            direction: TransferDirection::HostToVm,
        };
        let result = mgr
            .transfer_file(&vm_id, &transfer, agent_id, &mut broker, &token_id, now + 2)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not under any allowed inject root"));
    }

    #[tokio::test]
    async fn test_transfer_file_passes_validated_extract_path_to_backend() {
        let agent_id = "agent-transfer";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let root = temp_test_dir("bridge-extract-record");
        let project = root.join("project");
        let data = project.join("data");
        fs::create_dir_all(&project).expect("project dir should exist");
        fs::create_dir_all(&data).expect("data dir should exist");

        let backend = RecordingBackend::default();
        let transfers = Arc::clone(&backend.transfers);
        let bridge = FileBridge::new(&project, &data);
        let audit_path = data.join("runtime/vm-audit.jsonl");
        let mut mgr = VmManager::new(Box::new(backend), &audit_path, bridge);

        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start");

        let transfer = FileTransfer {
            host_path: "distillery-report.json".to_string(),
            vm_path: "/workspace/distillery-report.json".to_string(),
            direction: TransferDirection::VmToHost,
        };
        mgr.transfer_file(&vm_id, &transfer, agent_id, &mut broker, &token_id, now + 2)
            .await
            .expect("transfer should succeed");

        let recorded = transfers.lock().expect("lock");
        let recorded = recorded.last().expect("recorded transfer");
        assert!(recorded.host_path.ends_with(&format!(
            "runtime/vm-output/{}/distillery-report.json",
            vm_id
        )));
    }

    #[cfg(unix)]
    #[test]
    fn test_file_bridge_denies_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = temp_test_dir("bridge-symlink");
        let project = root.join("project");
        let data = project.join("data");
        let src = project.join("src");
        fs::create_dir_all(&src).expect("src dir should exist");
        fs::create_dir_all(&data).expect("data dir should exist");

        let outside = root.join("secret.txt");
        fs::write(&outside, "secret").expect("outside file should exist");

        let link = src.join("leak.txt");
        symlink(&outside, &link).expect("symlink should be created");

        let bridge = FileBridge::new(&project, &data);
        let transfer = FileTransfer {
            host_path: link.to_string_lossy().to_string(),
            vm_path: "/workspace/leak.txt".to_string(),
            direction: TransferDirection::HostToVm,
        };
        let result = bridge.validate("vm-1", &transfer);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not under any allowed inject root"));
    }

    // --- Audit log tests ---

    #[tokio::test]
    async fn test_audit_log_records_all_operations() {
        let agent_id = "agent-test-5";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");
        mgr.start(&vm_id, agent_id, &mut broker, &token_id, now + 1)
            .await
            .expect("start");
        mgr.exec(&vm_id, "ls", agent_id, &mut broker, &token_id, now + 2)
            .await
            .expect("exec");
        mgr.stop(&vm_id, agent_id, &mut broker, &token_id, now + 3)
            .await
            .expect("stop");
        mgr.destroy(&vm_id, agent_id, &mut broker, &token_id, now + 4)
            .await
            .expect("destroy");

        let log = mgr.audit_log();
        assert_eq!(log.len(), 5);
        assert_eq!(log[0].action, VmAction::Created);
        assert_eq!(log[1].action, VmAction::Started);
        assert_eq!(log[2].action, VmAction::CommandExecuted);
        assert_eq!(log[3].action, VmAction::Stopped);
        assert_eq!(log[4].action, VmAction::Destroyed);

        // All entries reference the same VM and agent
        for entry in log {
            assert_eq!(entry.vm_id, vm_id);
            assert_eq!(entry.agent_id, agent_id);
        }
    }

    // --- Error handling tests ---

    #[tokio::test]
    async fn test_exec_on_non_running_vm_fails() {
        let agent_id = "agent-test-6";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        let vm_id = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");

        // Exec without starting should fail
        let result = mgr
            .exec(&vm_id, "ls", agent_id, &mut broker, &token_id, now + 1)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not running"));
    }

    #[tokio::test]
    async fn test_destroy_nonexistent_vm_fails() {
        let agent_id = "agent-test-7";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        let result = mgr
            .destroy(
                &"vm-does-not-exist".to_string(),
                agent_id,
                &mut broker,
                &token_id,
                now,
            )
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("VM not found"));
    }

    #[tokio::test]
    async fn test_backend_create_failure_propagates() {
        let agent_id = "agent-test-8";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);

        let mock = MockBackend::new();
        mock.set_fail_create("disk space exhausted");
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        let mut mgr = VmManager::new(
            Box::new(mock),
            Path::new("/project/data/runtime/vm-audit.jsonl"),
            bridge,
        );

        let result = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("disk space exhausted"));
    }

    // --- File bridge unit tests ---

    #[test]
    fn test_file_bridge_size_check() {
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        assert!(bridge.check_size(1024).is_ok());
        assert!(bridge.check_size(100 * 1024 * 1024).is_ok());
        assert!(bridge.check_size(100 * 1024 * 1024 + 1).is_err());
    }

    #[test]
    fn test_file_bridge_output_dir() {
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        let dir = bridge.output_dir("vm-agent-1").expect("output_dir");
        assert_eq!(
            dir.to_string_lossy(),
            "/project/data/runtime/vm-output/vm-agent-1"
        );
    }

    #[test]
    fn test_file_bridge_extract_resolves_to_output_dir() {
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        let transfer = FileTransfer {
            host_path: "artifacts/results.json".to_string(),
            vm_path: "/workspace/results.json".to_string(),
            direction: TransferDirection::VmToHost,
        };
        let path = bridge.validate("vm-1", &transfer).expect("should succeed");
        assert!(path.starts_with("/project/data/runtime/vm-output/vm-1"));
        assert!(path.ends_with("artifacts/results.json"));
    }

    #[test]
    fn test_file_bridge_extract_rejects_traversal_components() {
        let bridge = FileBridge::new(Path::new("/project"), Path::new("/project/data"));
        let transfer = FileTransfer {
            host_path: "../escape.json".to_string(),
            vm_path: "/workspace/results.json".to_string(),
            direction: TransferDirection::VmToHost,
        };
        let result = bridge.validate("vm-1", &transfer);
        assert!(result.is_err());
    }

    // --- List active VMs ---

    #[tokio::test]
    async fn test_list_active_vms() {
        let agent_id = "agent-test-9";
        let now = 1_000_000u64;
        let (mut broker, token_id) = setup_broker(agent_id, now);
        let mut mgr = setup_manager();

        assert!(mgr.list_active().is_empty());

        let vm1 = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now)
            .await
            .expect("create");
        let vm2 = mgr
            .create(create_request(agent_id), &mut broker, &token_id, now + 1)
            .await
            .expect("create");

        assert_eq!(mgr.list_active().len(), 2);

        mgr.start(&vm1, agent_id, &mut broker, &token_id, now + 2)
            .await
            .expect("start");
        mgr.start(&vm2, agent_id, &mut broker, &token_id, now + 3)
            .await
            .expect("start");
        mgr.destroy(&vm1, agent_id, &mut broker, &token_id, now + 4)
            .await
            .expect("destroy");

        assert_eq!(mgr.list_active().len(), 1);
    }
}
