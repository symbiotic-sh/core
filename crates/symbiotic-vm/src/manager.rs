//! VmManager: orchestrates VM lifecycle, audit logging, and capability gating.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use symbiotic_trust::{AccessBroker, AccessRequest, AgentTrustLevel};

use crate::backend::VmBackend;
use crate::file_bridge::FileBridge;
use crate::types::{
    ExecResult, FileTransfer, TransferDirection, VmAction, VmAuditEntry, VmCreateRequest, VmId,
    VmInstance, VmState,
};

/// Required trust level for VM creation/execution scopes.
fn required_trust_for_vm_scope(scope: &str) -> AgentTrustLevel {
    if scope == "vm.network.modify" {
        AgentTrustLevel::ExternalAct
    } else {
        AgentTrustLevel::ArchiveWrite
    }
}

/// The VM manager handles the full lifecycle of sandbox VMs.
pub struct VmManager {
    backend: Box<dyn VmBackend>,
    instances: HashMap<VmId, VmInstance>,
    audit_log: Vec<VmAuditEntry>,
    audit_path: PathBuf,
    file_bridge: FileBridge,
    id_counter: u64,
}

impl VmManager {
    /// Create a new VmManager.
    pub fn new(backend: Box<dyn VmBackend>, audit_path: &Path, file_bridge: FileBridge) -> Self {
        Self {
            backend,
            instances: HashMap::new(),
            audit_log: Vec::new(),
            audit_path: audit_path.to_path_buf(),
            file_bridge,
            id_counter: 0,
        }
    }

    /// Create a new VM instance. The requesting agent must have "vm.create" scope.
    pub async fn create(
        &mut self,
        request: VmCreateRequest,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<VmId> {
        self.check_scope(
            broker,
            token_id,
            &request.requesting_agent,
            "vm.create",
            now,
        )?;

        let id = self.generate_id(&request.requesting_agent);
        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: id.clone(),
            agent_id: request.requesting_agent.clone(),
            action: VmAction::Created,
            details: format!("image={}, purpose={}", request.image, request.purpose),
        });

        let instance = self.backend.create(&id, &request).await?;
        self.instances.insert(id.clone(), instance);
        Ok(id)
    }

    /// Start a VM instance.
    pub async fn start(
        &mut self,
        vm_id: &VmId,
        agent_id: &str,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<()> {
        self.check_scope(broker, token_id, agent_id, "vm.create", now)?;
        self.require_instance(vm_id)?;

        self.backend.start(vm_id).await?;

        let instance = self.instances.get_mut(vm_id).expect("checked above");
        instance.state = VmState::Running;
        instance.started_at = Some(now);

        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: vm_id.clone(),
            agent_id: agent_id.to_string(),
            action: VmAction::Started,
            details: String::new(),
        });
        Ok(())
    }

    /// Execute a command inside the VM.
    pub async fn exec(
        &mut self,
        vm_id: &VmId,
        command: &str,
        agent_id: &str,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<ExecResult> {
        self.check_scope(broker, token_id, agent_id, "vm.exec", now)?;
        self.require_running(vm_id)?;

        let result = self.backend.exec(vm_id, command).await?;

        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: vm_id.clone(),
            agent_id: agent_id.to_string(),
            action: VmAction::CommandExecuted,
            details: format!("command={command}, exit_code={}", result.exit_code),
        });
        Ok(result)
    }

    /// Stop a VM instance (graceful shutdown).
    pub async fn stop(
        &mut self,
        vm_id: &VmId,
        agent_id: &str,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<()> {
        self.check_scope(broker, token_id, agent_id, "vm.create", now)?;
        self.require_running(vm_id)?;

        self.backend.stop(vm_id).await?;

        let instance = self.instances.get_mut(vm_id).expect("checked above");
        instance.state = VmState::Stopped;

        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: vm_id.clone(),
            agent_id: agent_id.to_string(),
            action: VmAction::Stopped,
            details: String::new(),
        });
        Ok(())
    }

    /// Destroy a VM instance (remove all resources).
    pub async fn destroy(
        &mut self,
        vm_id: &VmId,
        agent_id: &str,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<()> {
        self.check_scope(broker, token_id, agent_id, "vm.destroy", now)?;
        self.require_instance(vm_id)?;

        self.backend.destroy(vm_id).await?;
        self.instances.remove(vm_id);

        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: vm_id.clone(),
            agent_id: agent_id.to_string(),
            action: VmAction::Destroyed,
            details: String::new(),
        });
        Ok(())
    }

    /// Transfer a file between host and VM with file bridge validation.
    pub async fn transfer_file(
        &mut self,
        vm_id: &VmId,
        transfer: &FileTransfer,
        agent_id: &str,
        broker: &mut AccessBroker,
        token_id: &str,
        now: u64,
    ) -> Result<()> {
        let scope = match transfer.direction {
            TransferDirection::HostToVm => "vm.file.inject",
            TransferDirection::VmToHost => "vm.file.extract",
        };
        self.check_scope(broker, token_id, agent_id, scope, now)?;
        self.require_running(vm_id)?;

        // Validate via file bridge and pass the resolved host path to the backend
        // so host-side policy is enforced at the actual transfer callsite.
        let resolved_host_path = self.file_bridge.validate(vm_id, transfer)?;
        let mut effective_transfer = transfer.clone();
        effective_transfer.host_path = resolved_host_path.display().to_string();

        self.backend.transfer(vm_id, &effective_transfer).await?;

        let action = match transfer.direction {
            TransferDirection::HostToVm => VmAction::FileInjected,
            TransferDirection::VmToHost => VmAction::FileExtracted,
        };
        self.record_audit(VmAuditEntry {
            timestamp: now,
            vm_id: vm_id.clone(),
            agent_id: agent_id.to_string(),
            action,
            details: format!(
                "host={}, vm={}, direction={:?}",
                transfer.host_path, transfer.vm_path, transfer.direction
            ),
        });
        Ok(())
    }

    /// Get the current state of a VM.
    pub fn get_instance(&self, vm_id: &VmId) -> Option<&VmInstance> {
        self.instances.get(vm_id)
    }

    /// List all active VMs.
    pub fn list_active(&self) -> Vec<&VmInstance> {
        self.instances.values().collect()
    }

    /// Kill VMs that have exceeded their timeout.
    pub async fn reap_expired(&mut self, now: u64) -> Result<Vec<VmId>> {
        let expired: Vec<VmId> = self
            .instances
            .values()
            .filter(|inst| {
                if inst.state != VmState::Running {
                    return false;
                }
                if let Some(started) = inst.started_at {
                    now.saturating_sub(started) >= inst.resources.timeout_secs
                } else {
                    false
                }
            })
            .map(|inst| inst.id.clone())
            .collect();

        for vm_id in &expired {
            let agent_id = self
                .instances
                .get(vm_id)
                .map(|i| i.requesting_agent.clone())
                .unwrap_or_default();
            // Force stop and destroy
            let _ = self.backend.stop(vm_id).await;
            let _ = self.backend.destroy(vm_id).await;
            self.instances.remove(vm_id);
            self.record_audit(VmAuditEntry {
                timestamp: now,
                vm_id: vm_id.clone(),
                agent_id,
                action: VmAction::TimedOut,
                details: "exceeded timeout, reaped".to_string(),
            });
        }

        Ok(expired)
    }

    /// Get the audit log entries.
    pub fn audit_log(&self) -> &[VmAuditEntry] {
        &self.audit_log
    }

    /// Get the audit log path.
    pub fn audit_path(&self) -> &Path {
        &self.audit_path
    }

    fn generate_id(&mut self, agent_id: &str) -> VmId {
        self.id_counter += 1;
        format!("vm-{}-{}", agent_id, self.id_counter)
    }

    fn check_scope(
        &self,
        broker: &mut AccessBroker,
        token_id: &str,
        agent_id: &str,
        scope: &str,
        now: u64,
    ) -> Result<()> {
        let decision = broker.evaluate(
            token_id,
            &AccessRequest {
                subject: agent_id.to_string(),
                required_level: required_trust_for_vm_scope(scope),
                scope: scope.to_string(),
                goal_scope: None,
            },
            now,
        )?;
        if !decision.allowed {
            return Err(anyhow!(
                "capability denied: agent={agent_id}, scope={scope}, reason={}",
                decision.reason
            ));
        }
        Ok(())
    }

    fn require_instance(&self, vm_id: &VmId) -> Result<()> {
        if !self.instances.contains_key(vm_id) {
            return Err(anyhow!("VM not found: {vm_id}"));
        }
        Ok(())
    }

    fn require_running(&self, vm_id: &VmId) -> Result<()> {
        let instance = self
            .instances
            .get(vm_id)
            .ok_or_else(|| anyhow!("VM not found: {vm_id}"))?;
        if instance.state != VmState::Running {
            return Err(anyhow!(
                "VM {vm_id} is not running (state: {:?})",
                instance.state
            ));
        }
        Ok(())
    }

    fn record_audit(&mut self, entry: VmAuditEntry) {
        self.audit_log.push(entry);
    }
}
