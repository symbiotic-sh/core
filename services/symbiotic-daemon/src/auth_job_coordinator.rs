#![allow(clippy::large_enum_variant)]

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use credential_gateway::auth_engine::{
    AuthContinuationInput, AuthEngineError, AuthExecutionAttestation, AuthSandboxLauncher,
    ScriptInputKind, ScriptInputRequest,
};
use credential_gateway::{
    AuthRequest, CredentialGateway, CredentialRecord, CredentialVault, GoalScopedVault,
};

use crate::auth_approval_policies::{
    AuthApprovalPolicy, AuthApprovalPolicyRequest, AuthApprovalPolicyStore,
};
use crate::auth_jobs::{
    map_auth_error_code, AuthFailureCode, AuthInputKind, AuthInputRequest, AuthJobConfig,
    AuthJobRecord, AuthJobRequest, AuthJobStatus, AuthJobStore, SessionHandleSummary,
};

#[allow(clippy::large_enum_variant)]
pub enum AuthJobCreateDisposition {
    AwaitingApproval {
        record: AuthJobRecord,
    },
    AutoApproved {
        record: AuthJobRecord,
        policy: AuthApprovalPolicy,
    },
}

#[allow(clippy::large_enum_variant)]
pub enum AuthJobDispatch {
    Execute {
        record: AuthJobRecord,
        continuation: Option<AuthContinuationInput>,
        approval_policy: Option<AuthApprovalPolicy>,
    },
    TimedOut {
        record: AuthJobRecord,
    },
}

pub struct AuthJobExecution {
    pub started_record: Option<AuthJobRecord>,
    pub outcome: AuthJobExecutionOutcome,
}

#[allow(clippy::large_enum_variant)]
pub enum AuthJobExecutionOutcome {
    AwaitingInput {
        record: AuthJobRecord,
        input: AuthInputRequest,
        approval_policy: Option<AuthApprovalPolicy>,
    },
    Completed {
        record: AuthJobRecord,
        handle: SessionHandleSummary,
        approval_policy: Option<AuthApprovalPolicy>,
    },
    Failed {
        record: AuthJobRecord,
        code: AuthFailureCode,
        reason: String,
        approval_policy: Option<AuthApprovalPolicy>,
    },
}

pub struct AuthJobCoordinator<'a> {
    store: &'a Mutex<AuthJobStore>,
    approval_policies: &'a Mutex<AuthApprovalPolicyStore>,
    auth_engine: Option<&'a AuthSandboxLauncher>,
    credential_gateway: &'a CredentialGateway,
    credential_vault: &'a GoalScopedVault,
    config: AuthJobConfig,
}

impl<'a> AuthJobCoordinator<'a> {
    pub fn new(
        store: &'a Mutex<AuthJobStore>,
        approval_policies: &'a Mutex<AuthApprovalPolicyStore>,
        auth_engine: Option<&'a AuthSandboxLauncher>,
        credential_gateway: &'a CredentialGateway,
        credential_vault: &'a GoalScopedVault,
        config: AuthJobConfig,
    ) -> Self {
        Self {
            store,
            approval_policies,
            auth_engine,
            credential_gateway,
            credential_vault,
            config,
        }
    }

    pub fn create(&self, request: AuthJobRequest, now: u64) -> Result<AuthJobCreateDisposition> {
        let mut record = AuthJobRecord::from_request(request, now, self.config);
        if let Some(attestation) = self.preflight_attestation(&record.target)? {
            record.attestation = Some(attestation.clone());
        }
        let record = self
            .store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .create(record)?;

        if let Some(attestation) = record.attestation.as_ref() {
            if let Some(policy) = self
                .approval_policies
                .lock()
                .map_err(|_| anyhow!("auth approval policy store lock poisoned"))?
                .find_matching(&record.target, &record.scopes, attestation, now)
            {
                return Ok(AuthJobCreateDisposition::AutoApproved { record, policy });
            }
        }

        Ok(AuthJobCreateDisposition::AwaitingApproval { record })
    }

    pub fn deny(
        &self,
        request_id: &str,
        now: u64,
        reason: Option<String>,
    ) -> Result<AuthJobRecord> {
        let mut record = self.get(request_id)?;
        record.mark_denied(now, reason);
        self.store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .update(record)
    }

    pub fn approve(
        &self,
        request_id: &str,
        now: u64,
        approval_policy: Option<AuthApprovalPolicy>,
    ) -> Result<AuthJobDispatch> {
        let record = self.get(request_id)?;
        if record.is_expired(now) {
            let mut record = record;
            record.mark_timed_out(now);
            let record = self
                .store
                .lock()
                .map_err(|_| anyhow!("auth job store lock poisoned"))?
                .update(record)?;
            return Ok(AuthJobDispatch::TimedOut { record });
        }
        if record.status != AuthJobStatus::AwaitingApproval {
            return Err(anyhow!(
                "auth request is not awaiting approval: {}",
                record.request_id
            ));
        }
        Ok(AuthJobDispatch::Execute {
            record,
            continuation: None,
            approval_policy,
        })
    }

    pub fn respond(&self, request_id: &str, value: String, now: u64) -> Result<AuthJobDispatch> {
        let record = self.get(request_id)?;
        if record.is_expired(now) {
            let mut record = record;
            record.mark_timed_out(now);
            let record = self
                .store
                .lock()
                .map_err(|_| anyhow!("auth job store lock poisoned"))?
                .update(record)?;
            return Ok(AuthJobDispatch::TimedOut { record });
        }
        if record.status != AuthJobStatus::AwaitingInput {
            return Err(anyhow!(
                "auth request is not awaiting input: {}",
                record.request_id
            ));
        }
        let input = record
            .input_request
            .clone()
            .ok_or_else(|| anyhow!("auth request is missing input metadata"))?;
        let continuation = AuthContinuationInput {
            kind: map_auth_input_kind_to_script(input.kind),
            value,
            state: record.continuation_state.clone(),
        };
        Ok(AuthJobDispatch::Execute {
            record,
            continuation: Some(continuation),
            approval_policy: None,
        })
    }

    pub fn remember_approval(
        &self,
        request_id: &str,
        created_by: &str,
        ttl_secs: u64,
        now: u64,
    ) -> Result<AuthApprovalPolicy> {
        let record = self.get(request_id)?;
        let attestation = record
            .attestation
            .clone()
            .ok_or_else(|| anyhow!("auth request is missing attested profile metadata"))?;
        self.approval_policies
            .lock()
            .map_err(|_| anyhow!("auth approval policy store lock poisoned"))?
            .create(
                AuthApprovalPolicyRequest {
                    target: record.target.clone(),
                    scopes: record.scopes.clone(),
                    attestation,
                    created_from_request_id: record.request_id.clone(),
                    created_by: created_by.to_string(),
                    purpose: record.purpose.clone(),
                    ttl_secs,
                },
                now,
            )
    }

    pub fn revoke_policy(&self, policy_id: &str, now: u64) -> Result<AuthApprovalPolicy> {
        self.approval_policies
            .lock()
            .map_err(|_| anyhow!("auth approval policy store lock poisoned"))?
            .revoke(policy_id, now)
    }

    pub fn list_policies(
        &self,
        include_inactive: bool,
        now: u64,
    ) -> Result<Vec<AuthApprovalPolicy>> {
        Ok(self
            .approval_policies
            .lock()
            .map_err(|_| anyhow!("auth approval policy store lock poisoned"))?
            .list(include_inactive, now))
    }

    pub fn execute(
        &self,
        mut record: AuthJobRecord,
        now: u64,
        continuation: Option<AuthContinuationInput>,
        approval_policy: Option<AuthApprovalPolicy>,
    ) -> Result<AuthJobExecution> {
        if self.auth_engine.is_none() {
            return self.render_sandbox_unavailable(record, now, approval_policy);
        }

        record.mark_started(now, continuation.is_some());
        let started_record = self
            .store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .update(record.clone())?;

        let result = self.execute_worker_sync(&record, continuation);
        self.finalize_execution(record, started_record, now, result, approval_policy)
    }

    pub async fn execute_async(
        &self,
        mut record: AuthJobRecord,
        now: u64,
        continuation: Option<AuthContinuationInput>,
        approval_policy: Option<AuthApprovalPolicy>,
    ) -> Result<AuthJobExecution> {
        if self.auth_engine.is_none() {
            return self.render_sandbox_unavailable(record, now, approval_policy);
        }

        record.mark_started(now, continuation.is_some());
        let started_record = self
            .store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .update(record.clone())?;

        let result = self.execute_worker_async(&record, continuation).await;
        self.finalize_execution(record, started_record, now, result, approval_policy)
    }

    fn get(&self, request_id: &str) -> Result<AuthJobRecord> {
        self.store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .get(request_id)
            .ok_or_else(|| anyhow!("auth request not found: {request_id}"))
    }

    fn render_sandbox_unavailable(
        &self,
        mut record: AuthJobRecord,
        now: u64,
        approval_policy: Option<AuthApprovalPolicy>,
    ) -> Result<AuthJobExecution> {
        let reason = "Auth sandbox not configured".to_string();
        let code = AuthFailureCode::SandboxUnavailable;
        record.mark_failed(now, code, None);
        let record = self
            .store
            .lock()
            .map_err(|_| anyhow!("auth job store lock poisoned"))?
            .update(record)?;
        Ok(AuthJobExecution {
            started_record: None,
            outcome: AuthJobExecutionOutcome::Failed {
                record,
                code,
                reason,
                approval_policy,
            },
        })
    }

    fn finalize_execution(
        &self,
        mut record: AuthJobRecord,
        started_record: AuthJobRecord,
        now: u64,
        result: Result<credential_gateway::auth_engine::ScriptOutput, AuthEngineError>,
        approval_policy: Option<AuthApprovalPolicy>,
    ) -> Result<AuthJobExecution> {
        let outcome = match result {
            Ok(output) if output.input.is_some() => {
                let attestation = output.attestation.clone();
                let input = output
                    .input
                    .as_ref()
                    .map(map_script_input_request)
                    .ok_or_else(|| anyhow!("auth script input request missing details"))?;
                record.mark_awaiting_input(
                    now,
                    self.config,
                    input.clone(),
                    output.continuation_state.clone(),
                    attestation,
                );
                let record = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("auth job store lock poisoned"))?
                    .update(record)?;
                AuthJobExecutionOutcome::AwaitingInput {
                    record,
                    input,
                    approval_policy,
                }
            }
            Ok(output) if output.success => {
                if let Some(ref session) = output.session {
                    let store_record = CredentialRecord {
                        service: if record.goal_scope.is_some() {
                            record.target.clone()
                        } else {
                            format!("{}:session", record.target)
                        },
                        username: record.target.clone(),
                        secret: session.clone(),
                        totp_secret: None,
                    };
                    if record.goal_scope.is_some() {
                        self.credential_vault
                            .put_scoped(record.goal_scope.as_deref(), store_record)?;
                    } else {
                        self.credential_vault.put(store_record)?;
                    }
                }

                let handle = self.credential_gateway.issue_session_handle_scoped(
                    record.goal_scope.as_deref(),
                    AuthRequest {
                        target: record.target.clone(),
                        scopes: record.scopes.clone(),
                        session_type: record.session_type,
                        policy: credential_gateway::SessionPolicy {
                            exportable: false,
                            requires_reauth: false,
                        },
                    },
                    now,
                )?;
                let summary = SessionHandleSummary {
                    handle_id: handle.handle_id.clone(),
                    target: handle.target.clone(),
                    session_type: handle.session_type,
                    expires_at: handle.expires_at,
                };
                record.mark_completed(now, handle.handle_id, output.attestation.clone());
                let record = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("auth job store lock poisoned"))?
                    .update(record)?;
                AuthJobExecutionOutcome::Completed {
                    record,
                    handle: summary,
                    approval_policy,
                }
            }
            Ok(output) => {
                let reason = output
                    .error
                    .unwrap_or_else(|| "authentication failed".to_string());
                let code = map_auth_error_code(&reason);
                record.mark_failed(now, code, output.attestation.clone());
                let record = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("auth job store lock poisoned"))?
                    .update(record)?;
                AuthJobExecutionOutcome::Failed {
                    record,
                    code,
                    reason,
                    approval_policy,
                }
            }
            Err(error) => {
                let reason = error.to_string();
                let code = map_auth_error_code(&reason);
                record.mark_failed(now, code, None);
                let record = self
                    .store
                    .lock()
                    .map_err(|_| anyhow!("auth job store lock poisoned"))?
                    .update(record)?;
                AuthJobExecutionOutcome::Failed {
                    record,
                    code,
                    reason,
                    approval_policy,
                }
            }
        };

        Ok(AuthJobExecution {
            started_record: Some(started_record),
            outcome,
        })
    }

    fn execute_worker_sync(
        &self,
        record: &AuthJobRecord,
        continuation: Option<AuthContinuationInput>,
    ) -> Result<credential_gateway::auth_engine::ScriptOutput, AuthEngineError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AuthEngineError::SandboxUnavailable(format!("runtime failed: {e}")))?;

        rt.block_on(self.execute_worker_async(record, continuation))
    }

    async fn execute_worker_async(
        &self,
        record: &AuthJobRecord,
        continuation: Option<AuthContinuationInput>,
    ) -> Result<credential_gateway::auth_engine::ScriptOutput, AuthEngineError> {
        let Some(engine) = self.auth_engine else {
            return Err(AuthEngineError::SandboxUnavailable(
                "Auth sandbox not configured".to_string(),
            ));
        };

        let scoped_attempt = engine
            .authenticate_with_response_scoped(
                &record.target,
                record.goal_scope.as_deref(),
                continuation.clone(),
            )
            .await;
        match scoped_attempt {
            Err(AuthEngineError::NoCredentials(_)) if record.goal_scope.is_some() => {
                engine
                    .authenticate_with_response_scoped(&record.target, None, continuation)
                    .await
            }
            other => other,
        }
    }

    fn preflight_attestation(
        &self,
        target: &str,
    ) -> Result<Option<AuthExecutionAttestation>, AuthEngineError> {
        match self.auth_engine {
            Some(engine) => Ok(Some(engine.resolve_attestation(target)?)),
            None => Ok(None),
        }
    }
}

fn map_script_input_request(input: &ScriptInputRequest) -> AuthInputRequest {
    AuthInputRequest {
        kind: match input.kind {
            ScriptInputKind::TotpCode => AuthInputKind::TotpCode,
            ScriptInputKind::SmsCode => AuthInputKind::SmsCode,
            ScriptInputKind::Passkey => AuthInputKind::Passkey,
            ScriptInputKind::Captcha => AuthInputKind::Captcha,
        },
        prompt: input.prompt.clone(),
        masked_hint: input.masked_hint.clone(),
    }
}

fn map_auth_input_kind_to_script(kind: AuthInputKind) -> ScriptInputKind {
    match kind {
        AuthInputKind::TotpCode => ScriptInputKind::TotpCode,
        AuthInputKind::SmsCode => ScriptInputKind::SmsCode,
        AuthInputKind::Passkey => ScriptInputKind::Passkey,
        AuthInputKind::Captcha => ScriptInputKind::Captcha,
    }
}
