use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use credential_gateway::auth_engine::AuthExecutionAttestation;
use credential_gateway::SessionType;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthJobConfig {
    pub approval_ttl_secs: u64,
    pub input_ttl_secs: u64,
}

impl Default for AuthJobConfig {
    fn default() -> Self {
        Self {
            approval_ttl_secs: 300,
            input_ttl_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthJobPhase {
    Approval,
    Input,
}

impl AuthJobPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Input => "input",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthJobStatus {
    AwaitingApproval,
    AwaitingInput,
    Started,
    Completed,
    Failed,
    Denied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFailureCode {
    Denied,
    TimedOut,
    NoCredentials,
    NoProfile,
    SandboxUnavailable,
    TargetBlocked,
    ScriptFailed,
    SessionCaptureFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthInputKind {
    TotpCode,
    SmsCode,
    Passkey,
    Captcha,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAuthRequest {
    pub request_id: String,
    pub target: String,
    pub scopes: Vec<String>,
    pub session_type: SessionType,
    pub purpose: String,
    pub room_id: String,
    pub thread_id: Option<String>,
    pub auth_profile: Option<String>,
    pub requested_by: String,
    pub goal_scope: Option<String>,
    pub goal_room: Option<String>,
    pub goal_template: Option<String>,
    pub goal_id: Option<String>,
    pub phase: AuthJobPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<AuthInputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AuthExecutionAttestation>,
    pub expires_at: u64,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthInputRequest {
    pub kind: AuthInputKind,
    pub prompt: String,
    pub masked_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthJobRequest {
    pub target: String,
    pub scopes: Vec<String>,
    pub session_type: SessionType,
    pub purpose: String,
    pub room_id: String,
    pub thread_id: Option<String>,
    pub auth_profile: Option<String>,
    pub requested_by: String,
    pub goal_scope: Option<String>,
    pub goal_room: Option<String>,
    pub goal_template: Option<String>,
    pub goal_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionHandleSummary {
    pub handle_id: String,
    pub target: String,
    pub session_type: SessionType,
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum CredentialAuthenticateResponse {
    Completed {
        request_id: String,
        message: String,
        handle: SessionHandleSummary,
    },
    AwaitingApproval {
        request_id: String,
        target: String,
        room_id: String,
        expires_at: u64,
        message: String,
    },
    AwaitingInput {
        request_id: String,
        target: String,
        room_id: String,
        input: AuthInputRequest,
        expires_at: u64,
        message: String,
    },
    Failed {
        request_id: String,
        code: AuthFailureCode,
        message: String,
        retryable: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthJobRecord {
    pub request_id: String,
    pub target: String,
    pub scopes: Vec<String>,
    pub session_type: SessionType,
    pub purpose: String,
    pub room_id: String,
    pub thread_id: Option<String>,
    pub auth_profile: Option<String>,
    pub requested_by: String,
    pub goal_scope: Option<String>,
    pub goal_room: Option<String>,
    pub goal_template: Option<String>,
    pub goal_id: Option<String>,
    pub status: AuthJobStatus,
    pub phase: AuthJobPhase,
    pub expires_at: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub denial_reason: Option<String>,
    pub failure_code: Option<AuthFailureCode>,
    pub session_handle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AuthExecutionAttestation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_request: Option<AuthInputRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation_state: Option<String>,
}

impl AuthJobRecord {
    pub fn from_request(request: AuthJobRequest, now: u64, config: AuthJobConfig) -> Self {
        let target = request.target.to_ascii_lowercase();
        let request_id = new_request_id(
            &format!(
                "{}:{}:{}",
                request.requested_by,
                target,
                request.goal_scope.as_deref().unwrap_or("global")
            ),
            now,
        );

        Self {
            request_id,
            target,
            scopes: request.scopes,
            session_type: request.session_type,
            purpose: request.purpose,
            room_id: request.room_id,
            thread_id: request.thread_id,
            auth_profile: request.auth_profile,
            requested_by: request.requested_by,
            goal_scope: request.goal_scope,
            goal_room: request.goal_room,
            goal_template: request.goal_template,
            goal_id: request.goal_id,
            status: AuthJobStatus::AwaitingApproval,
            phase: AuthJobPhase::Approval,
            expires_at: now + config.approval_ttl_secs,
            created_at: now,
            updated_at: now,
            denial_reason: None,
            failure_code: None,
            session_handle_id: None,
            attestation: None,
            input_request: None,
            continuation_state: None,
        }
    }

    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    pub fn refresh_input_expiry(&mut self, now: u64, config: AuthJobConfig) {
        self.expires_at = now + config.input_ttl_secs;
    }

    pub fn mark_started(&mut self, now: u64, continuation: bool) {
        self.status = AuthJobStatus::Started;
        self.phase = if continuation {
            AuthJobPhase::Input
        } else {
            AuthJobPhase::Approval
        };
        self.updated_at = now;
    }

    pub fn mark_awaiting_input(
        &mut self,
        now: u64,
        config: AuthJobConfig,
        input: AuthInputRequest,
        continuation_state: Option<String>,
        attestation: Option<AuthExecutionAttestation>,
    ) {
        self.status = AuthJobStatus::AwaitingInput;
        self.phase = AuthJobPhase::Input;
        self.refresh_input_expiry(now, config);
        self.updated_at = now;
        if let Some(attestation) = attestation {
            self.attestation = Some(attestation);
        }
        self.input_request = Some(input);
        self.continuation_state = continuation_state;
        self.failure_code = None;
        self.denial_reason = None;
    }

    pub fn mark_completed(
        &mut self,
        now: u64,
        handle_id: String,
        attestation: Option<AuthExecutionAttestation>,
    ) {
        self.status = AuthJobStatus::Completed;
        self.updated_at = now;
        self.session_handle_id = Some(handle_id);
        if let Some(attestation) = attestation {
            self.attestation = Some(attestation);
        }
        self.input_request = None;
        self.continuation_state = None;
        self.failure_code = None;
        self.denial_reason = None;
    }

    pub fn mark_failed(
        &mut self,
        now: u64,
        code: AuthFailureCode,
        attestation: Option<AuthExecutionAttestation>,
    ) {
        self.status = AuthJobStatus::Failed;
        self.updated_at = now;
        if let Some(attestation) = attestation {
            self.attestation = Some(attestation);
        }
        self.failure_code = Some(code);
        self.input_request = None;
        self.continuation_state = None;
        self.denial_reason = None;
    }

    pub fn mark_denied(&mut self, now: u64, reason: Option<String>) {
        self.status = AuthJobStatus::Denied;
        self.updated_at = now;
        self.denial_reason = reason;
        self.failure_code = Some(AuthFailureCode::Denied);
        self.input_request = None;
        self.continuation_state = None;
    }

    pub fn mark_timed_out(&mut self, now: u64) {
        self.mark_failed(now, AuthFailureCode::TimedOut, None);
    }

    pub fn pending_request(&self) -> PendingAuthRequest {
        PendingAuthRequest {
            request_id: self.request_id.clone(),
            target: self.target.clone(),
            scopes: self.scopes.clone(),
            session_type: self.session_type,
            purpose: self.purpose.clone(),
            room_id: self.room_id.clone(),
            thread_id: self.thread_id.clone(),
            auth_profile: self.auth_profile.clone(),
            requested_by: self.requested_by.clone(),
            goal_scope: self.goal_scope.clone(),
            goal_room: self.goal_room.clone(),
            goal_template: self.goal_template.clone(),
            goal_id: self.goal_id.clone(),
            phase: self.phase,
            input: self.input_request.clone(),
            attestation: self.attestation.clone(),
            expires_at: self.expires_at,
            message: self
                .input_request
                .as_ref()
                .map(|input| input.prompt.clone())
                .unwrap_or_else(|| auth_required_message(&self.target)),
        }
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "request_id": self.request_id,
            "target": self.target,
            "phase": self.phase,
            "purpose": self.purpose,
            "requested_by": self.requested_by,
            "goal_scope": self.goal_scope,
            "thread_id": self.thread_id,
            "expires_at": self.expires_at,
            "status": self.status,
            "room_id": self.room_id,
            "attestation": self.attestation.clone(),
        })
    }
}

pub struct AuthJobStore {
    root: PathBuf,
    jobs: HashMap<String, AuthJobRecord>,
}

impl AuthJobStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create auth job directory {}", root.display()))?;
        let mut jobs = HashMap::new();
        for entry in fs::read_dir(&root)
            .with_context(|| format!("failed to read auth job directory {}", root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let content = fs::read_to_string(&path)
                .with_context(|| format!("failed to read auth job file {}", path.display()))?;
            let record: AuthJobRecord = serde_json::from_str(&content)
                .with_context(|| format!("failed to parse auth job file {}", path.display()))?;
            jobs.insert(record.request_id.clone(), record);
        }
        Ok(Self { root, jobs })
    }

    pub fn create(&mut self, record: AuthJobRecord) -> Result<AuthJobRecord> {
        self.persist(&record)?;
        self.jobs.insert(record.request_id.clone(), record.clone());
        Ok(record)
    }

    pub fn get(&self, request_id: &str) -> Option<AuthJobRecord> {
        self.jobs.get(request_id).cloned()
    }

    pub fn update(&mut self, record: AuthJobRecord) -> Result<AuthJobRecord> {
        self.persist(&record)?;
        self.jobs.insert(record.request_id.clone(), record.clone());
        Ok(record)
    }

    fn persist(&self, record: &AuthJobRecord) -> Result<()> {
        let path = self.path_for(&record.request_id);
        let body = serde_json::to_string_pretty(record)?;
        fs::write(&path, body)
            .with_context(|| format!("failed to write auth job file {}", path.display()))
    }

    fn path_for(&self, request_id: &str) -> PathBuf {
        self.root.join(format!("{request_id}.json"))
    }
}

pub fn auth_required_message(target: &str) -> String {
    format!("Approval required for {} login", target)
}

pub fn auth_input_message(input: &AuthInputRequest) -> String {
    input.prompt.clone()
}

pub fn map_auth_error_code(error: &str) -> AuthFailureCode {
    let lower = error.to_ascii_lowercase();
    if lower.contains("no credentials") {
        AuthFailureCode::NoCredentials
    } else if lower.contains("script not found") || lower.contains("no script") {
        AuthFailureCode::NoProfile
    } else if lower.contains("sandbox unavailable") {
        AuthFailureCode::SandboxUnavailable
    } else if lower.contains("blocked target") || lower.contains("unsafe target") {
        AuthFailureCode::TargetBlocked
    } else if lower.contains("timed out") {
        AuthFailureCode::TimedOut
    } else {
        AuthFailureCode::ScriptFailed
    }
}

pub fn new_request_id(seed: &str, now: u64) -> String {
    format!(
        "authreq_{:x}",
        crate::events::simple_hash(&format!("{seed}:{now}"))
    )
}

pub fn require_request_id(value: Option<&str>) -> Result<&str> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("request_id is required"))
}
