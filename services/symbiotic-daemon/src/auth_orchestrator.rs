use std::sync::Mutex;

use anyhow::{anyhow, Result};
use credential_gateway::auth_engine::AuthExecutionAttestation;
use symbiotic_core::protocol::{Kind, Status};
use symbiotic_matrix::events::MatrixEventEnvelope;

use crate::auth_approval_policies::AuthApprovalPolicy;
use crate::auth_job_coordinator::{
    AuthJobCoordinator, AuthJobCreateDisposition, AuthJobDispatch, AuthJobExecution,
    AuthJobExecutionOutcome,
};
use crate::auth_jobs::{
    auth_input_message, auth_required_message, require_request_id, AuthJobConfig, AuthJobRecord,
    AuthJobRequest, CredentialAuthenticateResponse,
};
use crate::bridge_interactions::{
    BridgeInteractionKind, BridgeInteractionLogStore, BridgeInteractionRecord, BridgeSessionStore,
};
use crate::events::{DaemonEvent, EventType, RoutedMatrixEnvelope};
use crate::goal_state::{upsert_goal_state, GoalState};
use crate::goals::{persist_auth_result, read_goal_text, WorkflowRunPayload};
use crate::SymbioticDaemon;

fn attach_auth_attestation_fields(
    detail: &mut serde_json::Value,
    attestation: Option<&AuthExecutionAttestation>,
) {
    let Some(attestation) = attestation else {
        return;
    };
    let Some(map) = detail.as_object_mut() else {
        return;
    };
    map.insert(
        "auth_requested_domain".to_string(),
        serde_json::json!(attestation.requested_domain),
    );
    map.insert(
        "auth_profile_id".to_string(),
        serde_json::json!(attestation.profile_id),
    );
    map.insert(
        "auth_profile_match".to_string(),
        serde_json::json!(attestation.match_kind),
    );
    map.insert(
        "auth_script_kind".to_string(),
        serde_json::json!(attestation.script_kind),
    );
    map.insert(
        "auth_profile_sha256".to_string(),
        serde_json::json!(attestation.script_sha256),
    );
}

fn attach_approval_policy_fields(
    detail: &mut serde_json::Value,
    approval_policy: Option<&AuthApprovalPolicy>,
) {
    let Some(policy) = approval_policy else {
        return;
    };
    let Some(map) = detail.as_object_mut() else {
        return;
    };
    map.insert(
        "approval_mode".to_string(),
        serde_json::json!("remembered_policy"),
    );
    map.insert(
        "approval_policy_id".to_string(),
        serde_json::json!(policy.policy_id),
    );
}

pub(crate) struct AuthOrchestrator<'a> {
    daemon: &'a SymbioticDaemon,
}

impl<'a> AuthOrchestrator<'a> {
    pub(crate) fn new(daemon: &'a SymbioticDaemon) -> Self {
        Self { daemon }
    }

    pub(crate) fn authenticate_room(
        &self,
        room_id: &str,
        sender: &str,
        domain: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let coordinator = self.coordinator();
        match coordinator.create(
            AuthJobRequest {
                target: domain.clone(),
                scopes: vec!["web.login".to_string()],
                session_type: credential_gateway::SessionType::Browser,
                purpose: format!("Manual authentication for {domain}"),
                room_id: room_id.to_string(),
                thread_id: None,
                auth_profile: None,
                requested_by: sender.to_string(),
                goal_scope: None,
                goal_room: None,
                goal_template: None,
                goal_id: None,
            },
            now,
        )? {
            AuthJobCreateDisposition::AwaitingApproval { record } => {
                let execution = coordinator.execute(record, now, None, None)?;
                self.render_auth_execution(room_id, execution, now)
            }
            AuthJobCreateDisposition::AutoApproved { record, policy } => {
                let execution = coordinator.execute(record, now, None, Some(policy))?;
                self.render_auth_execution(room_id, execution, now)
            }
        }
    }

    pub(crate) fn approve_room(
        &self,
        room_id: &str,
        sender: &str,
        request_id: String,
        remember_for_secs: Option<u64>,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let request_id = require_request_id(Some(&request_id))?;
        let coordinator = self.coordinator();
        match coordinator.approve(request_id, now, None)? {
            AuthJobDispatch::Execute {
                record,
                continuation,
                ..
            } => {
                let approval_policy = match remember_for_secs {
                    Some(ttl_secs) => Some(coordinator.remember_approval(
                        &record.request_id,
                        sender,
                        ttl_secs,
                        now,
                    )?),
                    None => None,
                };
                let execution = coordinator.execute(record, now, continuation, approval_policy)?;
                self.render_auth_execution(room_id, execution, now)
            }
            AuthJobDispatch::TimedOut { record } => {
                self.render_timed_out_auth_job(room_id, record, now)
            }
        }
    }

    pub(crate) fn deny_room(
        &self,
        room_id: &str,
        request_id: String,
        reason: Option<String>,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let request_id = require_request_id(Some(&request_id))?;
        let record = self.coordinator().deny(request_id, now, reason.clone())?;

        if let (Some(goal_room), Some(template)) =
            (record.goal_room.as_deref(), record.goal_template.as_deref())
        {
            upsert_goal_state(
                &self.daemon.config.goal_state_file,
                GoalState {
                    goal_room: goal_room.to_string(),
                    thread_id: None,
                    project_id: crate::goals::default_unscoped_project_id(),
                    template: template.to_string(),
                    status: "failed".to_string(),
                    last_job_id: request_id.to_string(),
                    last_run_id: record.goal_id.clone(),
                    owner: self
                        .goal_owner_for_auth_request(&record)
                        .or_else(|| record.goal_scope.clone()),
                    updated_at: now,
                    complexity: None,
                    pipeline_stage: Some("auth_denied".to_string()),
                    audit_id: None,
                    plan_id: None,
                },
            )?;
        }

        let mut detail = serde_json::json!({
            "request_id": record.request_id,
            "target": record.target,
            "purpose": record.purpose,
            "phase": record.phase.as_str(),
            "reason": reason,
            "code": "denied",
            "status": "denied",
        });
        attach_auth_attestation_fields(&mut detail, record.attestation.as_ref());
        Ok(self.auth_event_routes(
            room_id,
            record.goal_room.as_deref(),
            EventType::AuthFailed,
            detail,
            now,
        ))
    }

    pub(crate) fn respond_room(
        &self,
        room_id: &str,
        request_id: String,
        value: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let request_id = require_request_id(Some(&request_id))?;
        let coordinator = self.coordinator();
        match coordinator.respond(request_id, value, now)? {
            AuthJobDispatch::Execute {
                record,
                continuation,
                approval_policy,
            } => {
                let execution = coordinator.execute(record, now, continuation, approval_policy)?;
                self.render_auth_execution(room_id, execution, now)
            }
            AuthJobDispatch::TimedOut { record } => {
                self.render_timed_out_auth_job(room_id, record, now)
            }
        }
    }

    pub(crate) fn list_policies(
        &self,
        room_id: &str,
        include_inactive: bool,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let policies = self.coordinator().list_policies(include_inactive, now)?;
        let event = MatrixEventEnvelope::new(
            Kind::Message,
            Status::Success,
            now,
            "Credential approval policies listed",
        )
        .with_detail_field("count", policies.len() as u64)
        .with_detail_field("include_inactive", include_inactive)
        .with_detail_field("policies", serde_json::to_value(&policies)?);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    pub(crate) fn revoke_policy(
        &self,
        room_id: &str,
        policy_id: String,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let policy = self.coordinator().revoke_policy(policy_id.trim(), now)?;
        let event = MatrixEventEnvelope::new(
            Kind::Message,
            Status::Success,
            now,
            "Credential approval policy revoked",
        )
        .with_detail_field("policy_id", policy.policy_id)
        .with_detail_field("target", policy.target)
        .with_detail_field("auth_profile_id", policy.auth_profile_id)
        .with_detail_field("auth_profile_sha256", policy.auth_profile_sha256);
        Ok(vec![RoutedMatrixEnvelope {
            room_id: room_id.to_string(),
            envelope: event,
        }])
    }

    fn coordinator(&self) -> AuthJobCoordinator<'_> {
        AuthJobCoordinator::new(
            self.daemon.auth_jobs.as_ref(),
            self.daemon.auth_approval_policies.as_ref(),
            self.daemon.auth_engine.as_ref(),
            self.daemon.credential_gateway.as_ref(),
            self.daemon.credential_vault.as_ref(),
            AuthJobConfig {
                approval_ttl_secs: self.daemon.config.auth_approval_ttl_secs,
                input_ttl_secs: self.daemon.config.auth_input_ttl_secs,
            },
        )
    }

    fn render_timed_out_auth_job(
        &self,
        room_id: &str,
        record: AuthJobRecord,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        if let (Some(goal_room), Some(template)) =
            (record.goal_room.as_deref(), record.goal_template.as_deref())
        {
            upsert_goal_state(
                &self.daemon.config.goal_state_file,
                GoalState {
                    goal_room: goal_room.to_string(),
                    thread_id: None,
                    project_id: crate::goals::default_unscoped_project_id(),
                    template: template.to_string(),
                    status: "failed".to_string(),
                    last_job_id: record.request_id.clone(),
                    last_run_id: record.goal_id.clone(),
                    owner: self.goal_owner_for_auth_request(&record),
                    updated_at: now,
                    complexity: None,
                    pipeline_stage: Some("auth_timed_out".to_string()),
                    audit_id: None,
                    plan_id: None,
                },
            )?;
        }

        Ok(self.auth_event_routes(
            room_id,
            record.goal_room.as_deref(),
            EventType::AuthFailed,
            {
                let mut detail = serde_json::json!({
                    "request_id": record.request_id,
                    "target": record.target,
                    "purpose": record.purpose,
                    "phase": record.phase.as_str(),
                    "code": "timed_out",
                    "status": "failed",
                    "reason": "Authentication request expired",
                });
                attach_auth_attestation_fields(&mut detail, record.attestation.as_ref());
                detail
            },
            now,
        ))
    }

    fn render_auth_execution(
        &self,
        room_id: &str,
        execution: AuthJobExecution,
        now: u64,
    ) -> Result<Vec<RoutedMatrixEnvelope>> {
        let mut routed = if let Some(started_record) = execution.started_record {
            self.auth_event_routes(
                room_id,
                started_record.goal_room.as_deref(),
                EventType::AuthStarted,
                serde_json::json!({
                    "request_id": started_record.request_id,
                    "target": started_record.target,
                    "purpose": started_record.purpose,
                    "phase": "execution",
                    "status": "started",
                }),
                now,
            )
        } else {
            Vec::new()
        };

        match execution.outcome {
            AuthJobExecutionOutcome::AwaitingInput {
                record,
                input,
                approval_policy,
            } => {
                if let (Some(goal_room), Some(template)) =
                    (record.goal_room.as_deref(), record.goal_template.as_deref())
                {
                    upsert_goal_state(
                        &self.daemon.config.goal_state_file,
                        GoalState {
                            goal_room: goal_room.to_string(),
                            thread_id: None,
                            project_id: crate::goals::default_unscoped_project_id(),
                            template: template.to_string(),
                            status: "awaiting_auth".to_string(),
                            last_job_id: record.request_id.clone(),
                            last_run_id: record.goal_id.clone(),
                            owner: self.goal_owner_for_auth_request(&record),
                            updated_at: now,
                            complexity: None,
                            pipeline_stage: Some("awaiting_auth_input".to_string()),
                            audit_id: None,
                            plan_id: None,
                        },
                    )?;
                }

                let mut detail = serde_json::json!({
                    "request_id": record.request_id,
                    "target": record.target,
                    "purpose": record.purpose,
                    "phase": "input",
                    "status": "awaiting_input",
                    "input": {
                        "kind": input.kind,
                        "prompt": input.prompt,
                        "masked_hint": input.masked_hint,
                    },
                    "expires_at": record.expires_at,
                    "message": auth_input_message(&input),
                });
                attach_auth_attestation_fields(&mut detail, record.attestation.as_ref());
                attach_approval_policy_fields(&mut detail, approval_policy.as_ref());
                routed.extend(self.auth_event_routes(
                    room_id,
                    record.goal_room.as_deref(),
                    EventType::AuthRequired,
                    detail,
                    now,
                ));
            }
            AuthJobExecutionOutcome::Completed {
                record,
                handle,
                approval_policy,
            } => {
                self.persist_auth_completion(&record, &handle.handle_id, now)?;

                let mut detail = serde_json::json!({
                    "request_id": record.request_id,
                    "target": record.target,
                    "purpose": record.purpose,
                    "phase": "completed",
                    "status": "completed",
                    "handle_id": handle.handle_id,
                    "expires_at": handle.expires_at,
                });
                attach_auth_attestation_fields(&mut detail, record.attestation.as_ref());
                attach_approval_policy_fields(&mut detail, approval_policy.as_ref());
                routed.extend(self.auth_event_routes(
                    room_id,
                    record.goal_room.as_deref(),
                    EventType::AuthCompleted,
                    detail,
                    now,
                ));
            }
            AuthJobExecutionOutcome::Failed {
                record,
                code,
                reason,
                approval_policy,
            } => {
                let mut detail = serde_json::json!({
                    "request_id": record.request_id,
                    "target": record.target,
                    "purpose": record.purpose,
                    "phase": "execution",
                    "status": "failed",
                    "code": code,
                    "reason": reason,
                });
                attach_auth_attestation_fields(&mut detail, record.attestation.as_ref());
                attach_approval_policy_fields(&mut detail, approval_policy.as_ref());
                routed.extend(self.auth_event_routes(
                    room_id,
                    record.goal_room.as_deref(),
                    EventType::AuthFailed,
                    detail,
                    now,
                ));
            }
        }

        Ok(routed)
    }

    fn persist_auth_completion(
        &self,
        record: &AuthJobRecord,
        handle_id: &str,
        now: u64,
    ) -> Result<()> {
        if let (Some(goal_room), Some(template)) =
            (record.goal_room.as_deref(), record.goal_template.as_deref())
        {
            persist_auth_result(
                &self.daemon.config.data_dir,
                goal_room,
                template,
                &record.request_id,
                &record.target,
                handle_id,
            );
            let owner = self.goal_owner_for_auth_request(record);
            let payload = WorkflowRunPayload {
                template: template.to_string(),
                goal_room: Some(goal_room.to_string()),
                goal_sender: owner.clone(),
                project_id: Some(crate::goals::default_unscoped_project_id()),
                user_answer: None,
                goal_id: record.goal_id.clone(),
                user_goal: read_goal_text(&self.daemon.config.data_dir, goal_room, template),
                replan_context: None,
            };
            let (job_id, _) = self.daemon.queue_workflow_run_with_payload(&payload)?;
            upsert_goal_state(
                &self.daemon.config.goal_state_file,
                GoalState {
                    goal_room: goal_room.to_string(),
                    thread_id: None,
                    project_id: crate::goals::default_unscoped_project_id(),
                    template: template.to_string(),
                    status: "running".to_string(),
                    last_job_id: job_id,
                    last_run_id: record.goal_id.clone(),
                    owner,
                    updated_at: now,
                    complexity: None,
                    pipeline_stage: Some("executing".to_string()),
                    audit_id: None,
                    plan_id: None,
                },
            )?;
        }
        Ok(())
    }

    fn goal_owner_for_auth_request(&self, record: &AuthJobRecord) -> Option<String> {
        let states = self.daemon.list_goal_states().ok()?;
        states
            .into_iter()
            .find(|state| {
                state.goal_room == record.goal_room.clone().unwrap_or_default()
                    && state.template == record.goal_template.clone().unwrap_or_default()
                    && match record.goal_id.as_deref() {
                        Some(goal_id) => state.last_run_id.as_deref() == Some(goal_id),
                        None => true,
                    }
            })
            .and_then(|state| state.owner)
    }

    fn auth_event_routes(
        &self,
        primary_room: &str,
        goal_room: Option<&str>,
        event_type: EventType,
        detail: serde_json::Value,
        now: u64,
    ) -> Vec<RoutedMatrixEnvelope> {
        let event = DaemonEvent {
            event_type,
            status: detail
                .get("status")
                .and_then(|value| value.as_str())
                .unwrap_or("working")
                .to_string(),
            job_id: detail
                .get("request_id")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            detail: detail.to_string(),
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
        };
        let envelope = event.to_envelope(now);
        let mut routed = vec![RoutedMatrixEnvelope {
            room_id: primary_room.to_string(),
            envelope: envelope.clone(),
        }];
        if let Some(goal_room) = goal_room {
            if goal_room != primary_room {
                routed.push(RoutedMatrixEnvelope {
                    room_id: goal_room.to_string(),
                    envelope,
                });
            }
        }
        routed
    }
}

pub(crate) struct BridgeAuthOrchestrator<'a> {
    coordinator: AuthJobCoordinator<'a>,
    session_store: &'a Mutex<BridgeSessionStore>,
    interaction_log_store: &'a Mutex<BridgeInteractionLogStore>,
    credentials_room_id: &'a str,
}

impl<'a> BridgeAuthOrchestrator<'a> {
    pub(crate) fn new(
        coordinator: AuthJobCoordinator<'a>,
        session_store: &'a Mutex<BridgeSessionStore>,
        interaction_log_store: &'a Mutex<BridgeInteractionLogStore>,
        credentials_room_id: &'a str,
    ) -> Self {
        Self {
            coordinator,
            session_store,
            interaction_log_store,
            credentials_room_id,
        }
    }

    pub(crate) async fn authenticate(
        &self,
        bridge_token_id: &str,
        request: AuthJobRequest,
        now: u64,
    ) -> Result<CredentialAuthenticateResponse> {
        match self.coordinator.create(request, now)? {
            AuthJobCreateDisposition::AwaitingApproval { record } => {
                let pending = record.pending_request();
                let mut session_store = self
                    .session_store
                    .lock()
                    .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
                session_store.record_pending_auth_request(bridge_token_id, pending.clone());
                drop(session_store);
                self.record_pending_auth_request(bridge_token_id, &pending, now)?;
                Ok(CredentialAuthenticateResponse::AwaitingApproval {
                    request_id: record.request_id.clone(),
                    target: record.target.clone(),
                    room_id: self.credentials_room_id.to_string(),
                    expires_at: record.expires_at,
                    message: auth_required_message(&record.target),
                })
            }
            AuthJobCreateDisposition::AutoApproved { record, policy } => {
                let execution = self
                    .coordinator
                    .execute_async(record, now, None, Some(policy))
                    .await?;
                match execution.outcome {
                    AuthJobExecutionOutcome::AwaitingInput { record, input, .. } => {
                        let pending = record.pending_request();
                        let mut session_store = self
                            .session_store
                            .lock()
                            .map_err(|_| anyhow!("bridge session store lock poisoned"))?;
                        session_store.record_pending_auth_request(bridge_token_id, pending.clone());
                        drop(session_store);
                        self.record_pending_auth_request(bridge_token_id, &pending, now)?;
                        Ok(CredentialAuthenticateResponse::AwaitingInput {
                            request_id: record.request_id.clone(),
                            target: record.target.clone(),
                            room_id: self.credentials_room_id.to_string(),
                            input,
                            expires_at: record.expires_at,
                            message: auth_required_message(&record.target),
                        })
                    }
                    AuthJobExecutionOutcome::Completed { record, handle, .. } => {
                        Ok(CredentialAuthenticateResponse::Completed {
                            request_id: record.request_id,
                            message: "Authentication completed via remembered approval policy"
                                .to_string(),
                            handle,
                        })
                    }
                    AuthJobExecutionOutcome::Failed {
                        record,
                        code,
                        reason,
                        ..
                    } => Ok(CredentialAuthenticateResponse::Failed {
                        request_id: record.request_id,
                        code,
                        message: reason,
                        retryable: !matches!(
                            code,
                            crate::auth_jobs::AuthFailureCode::Denied
                                | crate::auth_jobs::AuthFailureCode::TargetBlocked
                        ),
                    }),
                }
            }
        }
    }

    fn record_pending_auth_request(
        &self,
        bridge_token_id: &str,
        pending: &crate::auth_jobs::PendingAuthRequest,
        now: u64,
    ) -> Result<()> {
        let mut interaction_store = self
            .interaction_log_store
            .lock()
            .map_err(|_| anyhow!("bridge interaction log store lock poisoned"))?;
        interaction_store.append(BridgeInteractionRecord {
            event_id: format!("{bridge_token_id}:{now}:auth"),
            token_id: bridge_token_id.to_string(),
            agent_id: pending.requested_by.clone(),
            goal_scope: pending.goal_scope.clone(),
            thread_id: pending.thread_id.clone(),
            kind: BridgeInteractionKind::PendingAuthRequest,
            summary: format!("Needs {} authentication", pending.target),
            detail: pending.message.clone(),
            created_at: now,
            raw_payload: serde_json::to_value(pending)?,
        })
    }
}
