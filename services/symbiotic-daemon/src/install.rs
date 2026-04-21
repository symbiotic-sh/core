//! Install, provision, bootstrap, and verify handlers.
//!
//! Runtime now delegates install lifecycle orchestration to the control-plane API.
//! Legacy local installer execution has been removed from this module.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::Method;
use serde::{Deserialize, Serialize};
use symbiotic_queue::{now_unix, QueueJob};

use crate::events::{DaemonEvent, EventType};
use crate::{escape_field, unescape_field, EnqueueRequest, SymbioticDaemon};

// --- Payload types ---

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallRunPayload {
    pub mode: String,
    pub install_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallProvisionPayload {
    pub mode: String,
    pub install_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallVerifyPayload {
    pub mode: String,
    pub install_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ControlPlaneCreateInstallRequest {
    install_id: Option<String>,
    region: String,
    provider: String,
    matrix_domain: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ControlPlaneInstallStatus {
    install_id: String,
    status: String,
    message: Option<String>,
}

// --- Payload codecs ---

pub(crate) fn encode_install_run_payload(payload: &InstallRunPayload) -> String {
    format!(
        "{}|{}",
        escape_field(&payload.mode),
        escape_field(&payload.install_id)
    )
}

pub(crate) fn encode_install_provision_payload(payload: &InstallProvisionPayload) -> String {
    format!(
        "{}|{}",
        escape_field(&payload.mode),
        escape_field(&payload.install_id)
    )
}

pub(crate) fn encode_install_verify_payload(payload: &InstallVerifyPayload) -> String {
    match payload
        .install_id
        .as_ref()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(install_id) => format!(
            "{}|{}",
            escape_field(&payload.mode),
            escape_field(install_id)
        ),
        None => escape_field(&payload.mode),
    }
}

pub(crate) fn decode_install_run_payload(encoded: &str) -> Result<InstallRunPayload> {
    let parts = encoded.splitn(2, '|').collect::<Vec<_>>();
    if parts.len() != 2 {
        return Err(anyhow!("install payload must have 2 parts"));
    }
    let mode = unescape_field(parts[0]).to_ascii_lowercase();
    if mode != "byok" && mode != "managed" {
        return Err(anyhow!("install payload mode must be byok or managed"));
    }
    let install_id = unescape_field(parts[1]);
    if install_id.trim().is_empty() {
        return Err(anyhow!("install payload install_id cannot be empty"));
    }
    Ok(InstallRunPayload { mode, install_id })
}

pub(crate) fn decode_install_provision_payload(encoded: &str) -> Result<InstallProvisionPayload> {
    let parts = encoded.splitn(2, '|').collect::<Vec<_>>();
    let (mode_raw, install_raw) = if parts.len() == 2 {
        (unescape_field(parts[0]), unescape_field(parts[1]))
    } else {
        // Backward compatibility with older payloads containing only install_id.
        ("byok".to_string(), unescape_field(encoded))
    };
    let mode = mode_raw.trim().to_ascii_lowercase();
    if mode != "byok" && mode != "managed" {
        return Err(anyhow!(
            "install provision payload mode must be byok or managed"
        ));
    }
    let install_id = install_raw.trim().to_string();
    if install_id.is_empty() {
        return Err(anyhow!(
            "install provision payload install_id cannot be empty"
        ));
    }
    Ok(InstallProvisionPayload { mode, install_id })
}

pub(crate) fn decode_install_verify_payload(encoded: &str) -> Result<InstallVerifyPayload> {
    let parts = encoded.splitn(2, '|').collect::<Vec<_>>();
    let mode = unescape_field(parts[0]).trim().to_ascii_lowercase();
    if mode != "byok" && mode != "managed" {
        return Err(anyhow!(
            "install verify payload mode must be byok or managed"
        ));
    }
    let install_id = parts
        .get(1)
        .map(|value| unescape_field(value).trim().to_string())
        .filter(|value| !value.is_empty());
    Ok(InstallVerifyPayload { mode, install_id })
}

// --- SymbioticDaemon impl: queue helpers + job executors ---

impl SymbioticDaemon {
    pub fn queue_install_run(&self, mode: &str, install_id: &str) -> Result<String> {
        let mode = mode.to_ascii_lowercase();
        if mode != "byok" && mode != "managed" {
            return Err(anyhow!("install mode must be byok or managed"));
        }
        let install_id = install_id.trim();
        if install_id.is_empty() {
            return Err(anyhow!("install_id cannot be empty"));
        }
        let payload = encode_install_run_payload(&InstallRunPayload {
            mode,
            install_id: install_id.to_string(),
        });
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "install.run".to_string(),
            payload,
            idempotency_key: format!("install:{}:{}", install_id, now_unix()),
            max_attempts: 1,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    pub fn queue_install_provision(&self, mode: &str, install_id: &str) -> Result<String> {
        let mode = mode.trim().to_ascii_lowercase();
        if mode != "byok" && mode != "managed" {
            return Err(anyhow!("install provision mode must be byok or managed"));
        }
        let install_id = install_id.trim();
        if install_id.is_empty() {
            return Err(anyhow!("install_id cannot be empty"));
        }
        let payload = encode_install_provision_payload(&InstallProvisionPayload {
            mode: mode.clone(),
            install_id: install_id.to_string(),
        });
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "install.provision".to_string(),
            payload,
            idempotency_key: format!("install-provision:{}:{}:{}", mode, install_id, now_unix()),
            max_attempts: 1,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    pub fn queue_install_bootstrap(&self) -> Result<String> {
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "install.bootstrap".to_string(),
            payload: String::new(),
            idempotency_key: format!("install-bootstrap:{}", now_unix()),
            max_attempts: 1,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    pub fn queue_install_verify(&self, mode: &str, install_id: Option<&str>) -> Result<String> {
        let mode = mode.trim().to_ascii_lowercase();
        if mode != "byok" && mode != "managed" {
            return Err(anyhow!("install verify mode must be byok or managed"));
        }
        let payload = encode_install_verify_payload(&InstallVerifyPayload {
            mode: mode.clone(),
            install_id: install_id
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        });
        let outcome = self.queue.enqueue(EnqueueRequest {
            type_name: "install.verify".to_string(),
            payload,
            idempotency_key: format!("install-verify:{}:{}", mode, now_unix()),
            max_attempts: 1,
            next_run_at: now_unix(),
            force: false,
        })?;
        Ok(outcome.job_id)
    }

    fn control_plane_base_url(&self) -> Option<String> {
        self.config
            .vps_provision_endpoint
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::env::var("SYMBIOTIC_CONTROL_PLANE_URL")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
    }

    fn control_plane_enabled(&self) -> bool {
        self.control_plane_base_url().is_some()
    }

    fn control_plane_provider(&self) -> String {
        std::env::var("SYMBIOTIC_VPS_PROVIDER")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "hetzner".to_string())
    }

    fn control_plane_matrix_domain(&self) -> String {
        std::env::var("SYMBIOTIC_MATRIX_PUBLIC_DOMAIN")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "matrix.symbiotic.local".to_string())
    }

    fn active_install_id_file(&self) -> PathBuf {
        self.config
            .data_dir
            .join("install")
            .join("active-install-id")
    }

    fn store_active_install_id(&self, install_id: &str) -> Result<()> {
        let path = self.active_install_id_file();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create install state dir {}", parent.display())
            })?;
        }
        fs::write(&path, format!("{}\n", install_id.trim()))
            .with_context(|| format!("failed to write active install id to {}", path.display()))
    }

    fn load_active_install_id(&self) -> Result<String> {
        let path = self.active_install_id_file();
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read active install id from {}", path.display()))?;
        let value = raw.trim().to_string();
        if value.is_empty() {
            return Err(anyhow!(
                "active install id file is empty ({})",
                path.display()
            ));
        }
        Ok(value)
    }

    fn with_control_plane_auth(
        &self,
        request: RequestBuilder,
        idempotency_key: &str,
    ) -> RequestBuilder {
        let mut request = request
            .header("x-user-id", self.config.worker_id.clone())
            .header("idempotency-key", idempotency_key);

        if let Some(token) = self
            .config
            .vps_provision_token
            .as_ref()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                std::env::var("SYMBIOTIC_CONTROL_PLANE_TOKEN")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            })
        {
            request = request.bearer_auth(token);
        }

        request
    }

    fn control_plane_request<T: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        idempotency_key: &str,
        body: Option<&T>,
    ) -> Result<ControlPlaneInstallStatus> {
        let base = self
            .control_plane_base_url()
            .ok_or_else(|| anyhow!("control-plane URL not configured"))?;
        let url = format!(
            "{}/{}",
            base.trim_end_matches('/'),
            path.trim_start_matches('/')
        );

        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("failed to build control-plane client")?;

        let mut request =
            self.with_control_plane_auth(client.request(method.clone(), &url), idempotency_key);
        if let Some(payload) = body {
            request = request.json(payload);
        }

        let response = request.send().with_context(|| {
            format!(
                "control-plane request failed to send: {} {}",
                url, idempotency_key
            )
        })?;

        let status_code = response.status();
        let response_body = response
            .text()
            .unwrap_or_else(|_| "<unreadable response body>".to_string());

        if !status_code.is_success() {
            return Err(anyhow!(
                "control-plane request failed: {} {} => {} {}",
                method,
                url,
                status_code,
                response_body
            ));
        }

        serde_json::from_str::<ControlPlaneInstallStatus>(&response_body).with_context(|| {
            format!(
                "invalid control-plane response for {} {}: {}",
                method, url, response_body
            )
        })
    }

    fn control_plane_create_install(
        &self,
        install_id: &str,
        idempotency_key: &str,
    ) -> Result<ControlPlaneInstallStatus> {
        let payload = ControlPlaneCreateInstallRequest {
            install_id: Some(install_id.to_string()),
            region: self.config.vps_region.clone(),
            provider: self.control_plane_provider(),
            matrix_domain: self.control_plane_matrix_domain(),
        };
        self.control_plane_request(
            Method::POST,
            "/v1/installs",
            idempotency_key,
            Some(&payload),
        )
    }

    fn control_plane_transition_install(
        &self,
        install_id: &str,
        step: &str,
        idempotency_key: &str,
    ) -> Result<ControlPlaneInstallStatus> {
        let path = format!("/v1/installs/{}/{}", install_id, step);
        self.control_plane_request::<serde_json::Value>(Method::POST, &path, idempotency_key, None)
    }

    fn local_emulated_status(
        &self,
        install_id: &str,
        status: &str,
        message: &str,
    ) -> ControlPlaneInstallStatus {
        ControlPlaneInstallStatus {
            install_id: install_id.to_string(),
            status: status.to_string(),
            message: Some(message.to_string()),
        }
    }

    pub(crate) fn execute_install_provision_job(
        &self,
        job: QueueJob,
        now: u64,
    ) -> Result<DaemonEvent> {
        let payload = decode_install_provision_payload(&job.payload)
            .with_context(|| format!("invalid install provision payload for job {}", job.job_id))?;

        let (create, provisioned) = if self.control_plane_enabled() {
            let create = self.control_plane_create_install(
                &payload.install_id,
                &format!("install-create:{}:{}", payload.install_id, job.job_id),
            )?;
            let provisioned = self.control_plane_transition_install(
                &create.install_id,
                "provision",
                &format!("install-provision:{}:{}", create.install_id, job.job_id),
            )?;
            (create, provisioned)
        } else {
            let create =
                self.local_emulated_status(&payload.install_id, "requested", "local emulation");
            let provisioned = self.local_emulated_status(
                &payload.install_id,
                "provisioning",
                "local emulation: provisioned",
            );
            (create, provisioned)
        };

        self.store_active_install_id(&create.install_id)?;

        let event_status = if matches!(provisioned.status.as_str(), "failed" | "cancelled") {
            "failed"
        } else {
            "completed"
        };

        let detail = serde_json::json!({
            "install_id": create.install_id,
            "mode": payload.mode,
            "status": provisioned.status,
            "message": provisioned.message.unwrap_or_default(),
            "step": "nucleus",
            "delegated_to": if self.control_plane_enabled() { "control-plane" } else { "local-emulation" },
        })
        .to_string();

        self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
        Ok(DaemonEvent {
            event_type: EventType::InstallNucleus,
            status: event_status.to_string(),
            job_id: Some(job.job_id),
            detail,
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }

    pub(crate) fn execute_install_bootstrap_job(
        &self,
        job: QueueJob,
        now: u64,
    ) -> Result<DaemonEvent> {
        let install_id = self.load_active_install_id()?;

        let bootstrapped = if self.control_plane_enabled() {
            self.control_plane_transition_install(
                &install_id,
                "bootstrap",
                &format!("install-bootstrap:{}:{}", install_id, job.job_id),
            )?
        } else {
            self.local_emulated_status(
                &install_id,
                "attested",
                "local emulation: bootstrap attested",
            )
        };

        let event_status = if matches!(bootstrapped.status.as_str(), "failed" | "cancelled") {
            "failed"
        } else {
            "completed"
        };

        let detail = serde_json::json!({
            "install_id": install_id,
            "status": bootstrapped.status,
            "message": bootstrapped.message.unwrap_or_default(),
            "step": "matrix",
            "delegated_to": if self.control_plane_enabled() { "control-plane" } else { "local-emulation" },
        })
        .to_string();

        self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
        Ok(DaemonEvent {
            event_type: EventType::InstallMatrix,
            status: event_status.to_string(),
            job_id: Some(job.job_id),
            detail,
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }

    pub(crate) fn execute_install_verify_job(
        &self,
        job: QueueJob,
        now: u64,
    ) -> Result<DaemonEvent> {
        let payload = decode_install_verify_payload(&job.payload)
            .with_context(|| format!("invalid install verify payload for job {}", job.job_id))?;
        let install_id = match payload.install_id.clone() {
            Some(value) => value,
            None => match self.load_active_install_id() {
                Ok(value) => value,
                Err(_) if !self.control_plane_enabled() => "install-local".to_string(),
                Err(err) => return Err(err),
            },
        };

        let verified = if self.control_plane_enabled() {
            self.control_plane_transition_install(
                &install_id,
                "verify",
                &format!("install-verify:{}:{}", install_id, job.job_id),
            )?
        } else {
            self.local_emulated_status(&install_id, "ready", "local emulation: verification passed")
        };

        let event_status = if verified.status == "ready" {
            "completed"
        } else {
            "failed"
        };

        let detail = serde_json::json!({
            "install_id": install_id,
            "mode": payload.mode,
            "status": verified.status,
            "message": verified.message.unwrap_or_default(),
            "step": "recall",
            "delegated_to": if self.control_plane_enabled() { "control-plane" } else { "local-emulation" },
        })
        .to_string();

        self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
        Ok(DaemonEvent {
            event_type: EventType::InstallRecall,
            status: event_status.to_string(),
            job_id: Some(job.job_id),
            detail,
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }

    pub(crate) fn execute_install_run_job(&self, job: QueueJob, now: u64) -> Result<DaemonEvent> {
        let payload = decode_install_run_payload(&job.payload)
            .with_context(|| format!("invalid install payload for job {}", job.job_id))?;

        let (create, provisioned, bootstrapped, verified) = if self.control_plane_enabled() {
            let create = self.control_plane_create_install(
                &payload.install_id,
                &format!("install-run-create:{}:{}", payload.install_id, job.job_id),
            )?;
            let provisioned = self.control_plane_transition_install(
                &create.install_id,
                "provision",
                &format!("install-run-provision:{}:{}", create.install_id, job.job_id),
            )?;
            let bootstrapped = self.control_plane_transition_install(
                &create.install_id,
                "bootstrap",
                &format!("install-run-bootstrap:{}:{}", create.install_id, job.job_id),
            )?;
            let verified = self.control_plane_transition_install(
                &create.install_id,
                "verify",
                &format!("install-run-verify:{}:{}", create.install_id, job.job_id),
            )?;
            (create, provisioned, bootstrapped, verified)
        } else {
            (
                self.local_emulated_status(&payload.install_id, "requested", "local emulation"),
                self.local_emulated_status(&payload.install_id, "provisioning", "local emulation"),
                self.local_emulated_status(&payload.install_id, "attested", "local emulation"),
                self.local_emulated_status(&payload.install_id, "ready", "local emulation"),
            )
        };

        self.store_active_install_id(&create.install_id)?;

        let status = if verified.status == "ready" {
            "completed"
        } else {
            "failed"
        };

        let detail = serde_json::json!({
            "install_id": create.install_id,
            "mode": payload.mode,
            "provision_status": provisioned.status,
            "bootstrap_status": bootstrapped.status,
            "verify_status": verified.status,
            "delegated_to": if self.control_plane_enabled() { "control-plane" } else { "local-emulation" },
        })
        .to_string();

        self.queue.ack(&job.job_id, &self.config.worker_id, now)?;
        Ok(DaemonEvent {
            event_type: EventType::InstallAlive,
            status: status.to_string(),
            job_id: Some(job.job_id),
            detail,
            goal_room: None,
            goal_template: None,
            goal_run_id: None,
            goal_id: None,
            intake_run_id: None,
            url: None,
            title: None,
            sensitivity: None,
            quick_replies: None,
            thread_id: None,
        })
    }
}
