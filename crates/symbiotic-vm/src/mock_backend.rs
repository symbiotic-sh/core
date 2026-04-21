//! Mock VM backend for testing and MVP development.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::backend::VmBackend;
use crate::types::{ExecResult, FileTransfer, VmCreateRequest, VmInstance, VmState};

/// Mock VM backend that simulates VM operations in-memory.
pub struct MockBackend {
    states: Mutex<HashMap<String, VmState>>,
    instances: Mutex<HashMap<String, VmInstance>>,
    /// If set, create() will return this error.
    fail_create: Mutex<Option<String>>,
    /// If set, start() will return this error.
    fail_start: Mutex<Option<String>>,
}

impl MockBackend {
    pub fn new() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
            instances: Mutex::new(HashMap::new()),
            fail_create: Mutex::new(None),
            fail_start: Mutex::new(None),
        }
    }

    /// Configure the backend to fail on create with the given message.
    pub fn set_fail_create(&self, msg: &str) {
        *self.fail_create.lock().expect("lock") = Some(msg.to_string());
    }

    /// Configure the backend to fail on start with the given message.
    pub fn set_fail_start(&self, msg: &str) {
        *self.fail_start.lock().expect("lock") = Some(msg.to_string());
    }
}

impl Default for MockBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl VmBackend for MockBackend {
    async fn create(&self, id: &str, request: &VmCreateRequest) -> Result<VmInstance> {
        if let Some(msg) = self.fail_create.lock().expect("lock").as_ref() {
            return Err(anyhow!("{msg}"));
        }

        let now = crate::time_now();
        let instance = VmInstance {
            id: id.to_string(),
            image: request.image.clone(),
            state: VmState::Creating,
            resources: request.resources.clone(),
            network: request.network.clone(),
            requesting_agent: request.requesting_agent.clone(),
            purpose: request.purpose.clone(),
            created_at: now,
            started_at: None,
        };
        self.states
            .lock()
            .expect("lock")
            .insert(id.to_string(), VmState::Creating);
        self.instances
            .lock()
            .expect("lock")
            .insert(id.to_string(), instance.clone());
        Ok(instance)
    }

    async fn start(&self, id: &str) -> Result<()> {
        if let Some(msg) = self.fail_start.lock().expect("lock").as_ref() {
            return Err(anyhow!("{msg}"));
        }

        let mut states = self.states.lock().expect("lock");
        let state = states
            .get(id)
            .ok_or_else(|| anyhow!("VM not found: {id}"))?;
        match state {
            VmState::Creating | VmState::Stopped => {
                states.insert(id.to_string(), VmState::Running);
                Ok(())
            }
            _ => Err(anyhow!("VM {id} cannot be started in state {state:?}")),
        }
    }

    async fn exec(&self, id: &str, command: &str) -> Result<ExecResult> {
        let states = self.states.lock().expect("lock");
        let state = states
            .get(id)
            .ok_or_else(|| anyhow!("VM not found: {id}"))?;
        if *state != VmState::Running {
            return Err(anyhow!("VM {id} is not running (state: {state:?})"));
        }
        Ok(ExecResult {
            exit_code: 0,
            stdout: format!("[mock] executed: {command}"),
            stderr: String::new(),
        })
    }

    async fn stop(&self, id: &str) -> Result<()> {
        let mut states = self.states.lock().expect("lock");
        let state = states
            .get(id)
            .ok_or_else(|| anyhow!("VM not found: {id}"))?;
        if *state != VmState::Running {
            return Err(anyhow!("VM {id} is not running (state: {state:?})"));
        }
        states.insert(id.to_string(), VmState::Stopped);
        Ok(())
    }

    async fn destroy(&self, id: &str) -> Result<()> {
        let mut states = self.states.lock().expect("lock");
        if !states.contains_key(id) {
            return Err(anyhow!("VM not found: {id}"));
        }
        states.remove(id);
        self.instances.lock().expect("lock").remove(id);
        Ok(())
    }

    async fn transfer(&self, id: &str, _transfer: &FileTransfer) -> Result<()> {
        let states = self.states.lock().expect("lock");
        let state = states
            .get(id)
            .ok_or_else(|| anyhow!("VM not found: {id}"))?;
        if *state != VmState::Running {
            return Err(anyhow!(
                "VM {id} must be running for file transfer (state: {state:?})"
            ));
        }
        Ok(())
    }

    async fn get_state(&self, id: &str) -> Result<VmState> {
        self.states
            .lock()
            .expect("lock")
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("VM not found: {id}"))
    }
}
