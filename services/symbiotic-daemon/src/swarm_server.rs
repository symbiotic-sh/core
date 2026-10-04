//! Swarm server: manages the git server container and PR operations.
//!
//! This module wires `symbiotic-git-swarm` into the daemon, providing:
//! - Git server container lifecycle (start/stop at daemon init)
//! - RPC handlers for PR operations (called by agents via bridge)
//! - Push authorization endpoint (called by git pre-receive hook)

#![allow(
    clippy::enum_variant_names,
    clippy::needless_borrow,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::await_holding_lock
)]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{error, info, warn};
use uuid::Uuid;

use symbiotic_control_plane::{
    AgentAssignment, AssignmentMode, CollaborationScope, HeartbeatStatus, HeartbeatUpdate, Lease,
    ManagementStore, ReviewMode, ScopeClaim, ScopeClaimStatus, ScopeMode, ScopeRequirement,
    WorkItem, WorkItemKind, WorkItemStatus, WorkPriority, WorkUrgency,
};
use symbiotic_git_swarm::server::GitServerManager;
use symbiotic_git_swarm::types::{
    CheckStatus, GitSwarmConfig, PRStatus, PushAuthRequest, PushAuthResponse, PushSessionRequest,
    PushSessionResponse, SwarmPR, SwarmRepo,
};
use symbiotic_git_swarm::PRManager;
use symbiotic_trust::{now_unix, AccessBroker, AccessRequest, AgentTrustLevel, CapabilityToken};
use symbiotic_vm::types::{BindMount, NetworkPolicy, VmCreateRequest, VmResources};

use crate::goal_management::{goal_task_work_item_id, goal_work_item_id};

const PUSH_SESSION_TTL_SECS: u64 = 300;
const SWARM_REQUIRED_CHECKS_ENV: &str = "SYMBIOTIC_SWARM_REQUIRED_CHECKS";
const SWARM_AGENT_IMAGE_ENV: &str = "SYMBIOTIC_SWARM_AGENT_IMAGE";
const SWARM_AGENT_RUNNER_BINARY_ENV: &str = "SYMBIOTIC_AGENT_RUNNER_BINARY";
const SWARM_AGENT_VM_IMAGE: &str = "symbiotic-agent-v1";
const SWARM_RUNNER_PATH: &str = "/usr/local/bin/symbiotic-agent-runner";
const SWARM_WORKSPACE_PATH: &str = "/workspace";
const SWARM_DISPATCHER_AGENT: &str = "swarm-dispatcher";
const SWARM_LINTER_PATH: &str = "/usr/local/bin/symbiotic-linter";
const DISTILLERY_CHECK_NAME: &str = "distillery";
const DISTILLERY_BUNDLE_VM_PATH: &str = "/workspace/distillery-bundle.json";
const DISTILLERY_OUTPUT_VM_ROOT: &str = "/workspace/output";

/// System prompt for the distillery post-merge agent.
///
/// The distillery agent runs after a PR is auto-merged. Its job is to:
/// 1. Clone the repo from the git server
/// 2. Run `symbiotic-linter dir .` on the workspace
/// 3. If lint errors exist, use LLM to fix them and create a follow-up PR via `pr.create`
/// 4. If clean, extract a knowledge summary of the changes
const DISTILLERY_AGENT_SYSTEM_PROMPT: &str = r#"You are a Distillery agent in the Symbiotic git swarm.

Your job is post-merge quality assurance and knowledge extraction.

## Steps

1. Clone the repository:
   ```
   git clone $GIT_SERVER_URL workspace && cd workspace
   ```

2. Run the linter on the entire workspace:
   ```
   symbiotic-linter dir .
   ```

3. If the linter reports errors:
   a. Fix each error using your best judgment
   b. Create a new branch: `git checkout -b fix/distillery-lint-<PR_ID>`
   c. Commit the fixes
   d. Push the branch
   e. Create a follow-up PR using the `pr.create` tool with title "fix(distillery): lint fixes for PR <MERGED_PR_ID>"

4. If the linter reports no errors (or after creating the fix PR):
   a. Summarize what the merged PR changed
   b. Extract key architectural decisions, new patterns, or conventions
   c. Report the summary via the `check.report` tool

## Available tools
- `git_clone` — clone the swarm repo
- `git_push` — push a fix branch
- `pr.create` — create a follow-up lint-fix PR
- `check.report` — report distillery results

## Constraints
- You are best-effort. If something fails, log it and exit cleanly.
- Never force-push or modify the base branch directly.
- Keep fix commits small and focused on lint errors only.
- Before exiting, write a JSON manifest to `/workspace/distillery-bundle.json` with this schema:
  `{"version":1,"report":{"summary_markdown":"string","decisions":["string"],"patterns":["string"],"follow_up_pr_title":"string|null","lint_status":"clean|fix_pr_opened|skipped|failed"},"artifacts":[{"kind":"methodology_note|decision_note|pattern_note","title":"string","relative_path":"string"}]}`
- Optional extra markdown artifacts must live under `/workspace/output/`, and each one must be listed in `artifacts`.
"#;

#[derive(Debug, Clone)]
struct PushSession {
    token_id: String,
    agent_id: String,
    repo_id: String,
    thread_id: Option<String>,
    expires_at: u64,
}

/// Shared state for the swarm subsystem, accessible from RPC handlers.
pub struct SwarmServer {
    pub git_server: Arc<Mutex<GitServerManager>>,
    pub pr_manager: Arc<Mutex<PRManager>>,
    pub broker: Arc<StdMutex<AccessBroker>>,
    management_store: Arc<StdMutex<ManagementStore>>,
    push_sessions: Arc<StdMutex<HashMap<String, PushSession>>>,
    sandbox_manager: Option<Arc<StdMutex<symbiotic_vm::manager::VmManager>>>,
    llm_gateway_socket: Option<String>,
    archive_root: PathBuf,
    vm_output_root: PathBuf,
    required_checks: Vec<String>,
    pub config: GitSwarmConfig,
    /// Optional handle to the durable T126 `RepoRegistry`. When `Some`,
    /// `authorize_push` consults it for protected-branch enforcement on
    /// durable repos (defense-in-depth per `docs/design/repo-manifest.md`
    /// §Security). Tests may construct a `SwarmServer` with `None`.
    pub(crate) repo_registry: Option<crate::repo_registry::SharedRepoRegistry>,
}

#[derive(Debug, Clone)]
struct DispatchJob {
    request: VmCreateRequest,
    command: String,
    pr_id: String,
    check_name: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct DispatchPlan {
    ci_jobs: Vec<DispatchJob>,
    reviewer_job: Option<DispatchJob>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct AutoMergeOutcome {
    merged_sha: Option<String>,
    warning: Option<String>,
    distillery_vm_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DistilleryArchiveReport {
    summary_markdown: String,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    patterns: Vec<String>,
    follow_up_pr_title: Option<String>,
    lint_status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DistilleryArchiveBundle {
    #[serde(default = "default_distillery_bundle_version")]
    version: u32,
    report: DistilleryArchiveReport,
    #[serde(default)]
    artifacts: Vec<DistilleryArchiveArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DistilleryArchiveArtifact {
    kind: DistilleryArtifactKind,
    title: String,
    relative_path: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum DistilleryArtifactKind {
    #[serde(rename = "methodology_note", alias = "methodology")]
    Methodology,
    #[serde(rename = "decision_note", alias = "decision")]
    Decision,
    #[serde(rename = "pattern_note", alias = "pattern")]
    Pattern,
}

fn default_distillery_bundle_version() -> u32 {
    1
}

#[derive(Debug, Clone, Default)]
struct DispatchResult {
    ci_vm_ids: Vec<String>,
    reviewer_vm_id: Option<String>,
    warnings: Vec<String>,
}

impl SwarmServer {
    /// Initialize the swarm server from environment configuration.
    pub fn new(
        broker: Arc<StdMutex<AccessBroker>>,
        management_store: Arc<StdMutex<ManagementStore>>,
        sandbox_manager: Option<Arc<StdMutex<symbiotic_vm::manager::VmManager>>>,
        llm_gateway_socket: Option<String>,
        archive_root: PathBuf,
        data_dir: PathBuf,
        repo_registry: Option<crate::repo_registry::SharedRepoRegistry>,
    ) -> Result<Self> {
        let config = config_from_env();
        let git_server = GitServerManager::new(config.clone())?;

        Ok(Self {
            git_server: Arc::new(Mutex::new(git_server)),
            pr_manager: Arc::new(Mutex::new(PRManager::new())),
            broker,
            management_store,
            push_sessions: Arc::new(StdMutex::new(HashMap::new())),
            sandbox_manager,
            llm_gateway_socket,
            archive_root,
            vm_output_root: data_dir.join("runtime").join("vm-output"),
            required_checks: config.default_merge_rules.required_checks.clone(),
            config,
            repo_registry,
        })
    }

    /// Issue a short-lived push session for an agent branch push.
    pub async fn issue_push_session(
        &self,
        request: PushSessionRequest,
    ) -> Result<PushSessionResponse> {
        let now = now_unix();
        let repo = {
            let server = self.git_server.lock().await;
            server.get_repo(&request.repo_id).cloned()
        };

        let mut broker = self
            .broker
            .lock()
            .map_err(|_| anyhow!("capability broker lock poisoned"))?;
        let mut push_sessions = self
            .push_sessions
            .lock()
            .map_err(|_| anyhow!("push session store lock poisoned"))?;

        let response = issue_push_session_for_repo(
            repo.as_ref(),
            &mut broker,
            &mut push_sessions,
            &request,
            now,
        )?;

        self.sync_branch_ownership(&request, now)?;
        Ok(response)
    }

    /// Start the git server container. Call during daemon initialization.
    pub async fn start(&self) -> Result<()> {
        let mut server = self.git_server.lock().await;
        server.start().await?;
        info!("swarm: git server container started");
        Ok(())
    }

    /// Stop the git server container. Call during daemon shutdown.
    pub async fn stop(&self) -> Result<()> {
        let mut server = self.git_server.lock().await;
        server.stop().await?;
        info!("swarm: git server container stopped");
        Ok(())
    }

    /// Authorize a push from the git server's pre-receive hook.
    pub async fn authorize_push(&self, request: PushAuthRequest) -> PushAuthResponse {
        let repo = {
            let server = self.git_server.lock().await;
            server.get_repo(&request.repo_id).cloned()
        };
        let now = now_unix();

        // D2 defense-in-depth: snapshot the durable manifest (if registered)
        // before acquiring any sync locks. `SharedRepoRegistry` uses
        // `tokio::sync::Mutex`, so we await here — before any sync guard is
        // held, to avoid `await_holding_lock`.
        let durable_manifest = match &self.repo_registry {
            Some(registry_arc) => {
                let gate = registry_arc.lock().await;
                gate.get(&request.repo_id).cloned()
            }
            None => None,
        };

        let mut broker = match self.broker.lock() {
            Ok(broker) => broker,
            Err(_) => return deny_push("capability broker lock poisoned"),
        };
        let mut push_sessions = match self.push_sessions.lock() {
            Ok(push_sessions) => push_sessions,
            Err(_) => return deny_push("push session store lock poisoned"),
        };

        let response = authorize_push_for_repo(
            repo.as_ref(),
            &mut broker,
            &mut push_sessions,
            &request,
            now,
            durable_manifest.as_ref(),
        );
        let ownership_request =
            self.authorized_push_ownership_request(&request, &push_sessions, response.allowed);
        drop(push_sessions);
        drop(broker);

        if let Some(ownership_request) = ownership_request {
            if let Err(err) = self.sync_branch_ownership(&ownership_request, now) {
                warn!(
                    repo_id = %ownership_request.repo_id,
                    branch = %ownership_request.branch,
                    error = %err,
                    "swarm: denying push because branch ownership sync failed"
                );
                return deny_push(format!("branch ownership sync failed: {err}"));
            }
        }

        response
    }

    /// Handle an RPC call from an agent tool.
    ///
    /// Routes `pr.*` methods to the PRManager and `swarm.*` methods to
    /// the GitServerManager.
    pub async fn handle_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        match method {
            "pr.create" => self.rpc_pr_create(params).await,
            "pr.comment" | "pr.approve" | "pr.request_changes" => {
                self.rpc_pr_review(method, params).await
            }
            "pr.get_status" => self.rpc_pr_get_status(params).await,
            "pr.merge" => self.rpc_pr_merge(params).await,
            "pr.close" => self.rpc_pr_close(params).await,
            "check.report" => self.rpc_check_report(params).await,
            "swarm.issue_push_session" => self.rpc_issue_push_session(params).await,
            "swarm.create_repo" => self.rpc_create_repo(params).await,
            "swarm.destroy_repo" => self.rpc_destroy_repo(params).await,
            "distillery.status" => self.rpc_distillery_status(params).await,
            _ => Err(anyhow::anyhow!("unknown swarm method: {}", method)),
        }
    }

    fn sync_branch_ownership(&self, request: &PushSessionRequest, now: u64) -> Result<()> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|_| anyhow!("management store lock poisoned"))?;
        let parent_task_id = request
            .goal_scope
            .as_ref()
            .filter(|scope| !scope.is_empty())
            .map(|goal_scope| {
                self.ensure_goal_development_task_work_item(
                    &mut store,
                    goal_scope,
                    &request.repo_id,
                    request.thread_id.as_deref(),
                    now as i64,
                )
            })
            .transpose()?;

        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let mut work_item = store
            .get_work_item(&work_item_id)
            .cloned()
            .unwrap_or_else(|| WorkItem {
                id: work_item_id.clone(),
                project_id: request.repo_id.clone(),
                initiative_id: None,
                parent_work_item_id: None,
                kind: WorkItemKind::Execution,
                thread_id: request.thread_id.clone(),
                title: String::new(),
                summary: String::new(),
                status: WorkItemStatus::ClaimPending,
                priority: WorkPriority::P2,
                urgency: WorkUrgency::Normal,
                assignment_mode: AssignmentMode::SingleOwner,
                requested_scopes: vec![ScopeRequirement {
                    scope: CollaborationScope::RepoBranchNamespace {
                        repo_id: request.repo_id.clone(),
                        pattern: request.branch.clone(),
                    },
                    mode: ScopeMode::ExclusiveWrite,
                    reason: "swarm push session branch ownership".to_string(),
                }],
                accepted_claim_ids: Vec::new(),
                assignee: None,
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: ReviewMode::AutoReviewThenHumanIfNeeded,
                cancellation: None,
                created_at: now as i64,
                updated_at: now as i64,
            });
        work_item.title = format!("Implementation branch: {}", request.branch);
        work_item.summary = match request.goal_scope.as_deref() {
            Some(goal_scope) if !goal_scope.is_empty() => format!(
                "Tracks active branch ownership for repo '{}' branch '{}' under goal scope '{}'.",
                request.repo_id, request.branch, goal_scope
            ),
            _ => format!(
                "Tracks active branch ownership for repo '{}' branch '{}'.",
                request.repo_id, request.branch
            ),
        };
        work_item.assignee = Some(AgentAssignment {
            agent_id: request.agent_id.clone(),
            runner_id: None,
            assigned_at: now as i64,
        });
        if let Some(goal_scope) = request
            .goal_scope
            .as_ref()
            .filter(|scope| !scope.is_empty())
        {
            work_item.initiative_id = Some(goal_scope.clone());
            work_item.parent_work_item_id = parent_task_id.clone();
        }
        if let Some(thread_id) = request.thread_id.as_ref().filter(|value| !value.is_empty()) {
            work_item.thread_id = Some(thread_id.clone());
        }
        store.upsert_work_item(work_item)?;
        self.sync_branch_artifact_work_item(
            &mut store,
            request,
            &work_item_id,
            WorkItemStatus::Running,
            now as i64,
        )?;

        store.grant_claim(ScopeClaim {
            id: branch_claim_id(&request.repo_id, &request.branch, &request.agent_id),
            work_item_id: work_item_id.clone(),
            holder_agent_id: request.agent_id.clone(),
            scope: CollaborationScope::RepoBranchNamespace {
                repo_id: request.repo_id.clone(),
                pattern: request.branch.clone(),
            },
            mode: ScopeMode::ExclusiveWrite,
            status: ScopeClaimStatus::Active,
            lease: Lease::new(request.agent_id.clone(), now as i64, 30, 2),
            granted_at: now as i64,
            updated_at: now as i64,
        })?;

        store.record_heartbeat(HeartbeatUpdate {
            work_item_id,
            agent_id: request.agent_id.clone(),
            status: HeartbeatStatus::Alive,
            progress_summary: Some("push session issued".to_string()),
            progress_percent: None,
            needs_attention: false,
            observed_at: now as i64,
        })?;
        if let Some(parent_task_id) = parent_task_id.as_deref() {
            self.refresh_parent_task_status(&mut store, parent_task_id, now as i64)?;
        }

        Ok(())
    }

    fn ensure_goal_development_task_work_item(
        &self,
        store: &mut ManagementStore,
        goal_scope: &str,
        repo_id: &str,
        thread_id: Option<&str>,
        observed_at: i64,
    ) -> Result<String> {
        let task_slug = format!("development:{repo_id}");
        let task_id = goal_task_work_item_id(goal_scope, &task_slug);
        let mut task = store
            .get_work_item(&task_id)
            .cloned()
            .unwrap_or_else(|| WorkItem {
                id: task_id.clone(),
                project_id: "symbiotic".to_string(),
                initiative_id: Some(goal_scope.to_string()),
                parent_work_item_id: Some(goal_work_item_id(goal_scope)),
                kind: WorkItemKind::Task,
                thread_id: thread_id.map(str::to_string),
                title: format!("Development task: {repo_id}"),
                summary: String::new(),
                status: WorkItemStatus::Todo,
                priority: WorkPriority::P2,
                urgency: WorkUrgency::Normal,
                assignment_mode: AssignmentMode::ParallelChildren,
                requested_scopes: Vec::new(),
                accepted_claim_ids: Vec::new(),
                assignee: Some(AgentAssignment {
                    agent_id: SWARM_DISPATCHER_AGENT.to_string(),
                    runner_id: None,
                    assigned_at: observed_at,
                }),
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: ReviewMode::NoReview,
                cancellation: None,
                created_at: observed_at,
                updated_at: observed_at,
            });
        task.thread_id = thread_id.map(str::to_string).or(task.thread_id);
        task.summary =
            format!("Durable development task for repo '{repo_id}' under goal '{goal_scope}'.");
        task.touch(observed_at);
        store.upsert_work_item(task)?;
        Ok(task_id)
    }

    fn sync_branch_artifact_work_item(
        &self,
        store: &mut ManagementStore,
        request: &PushSessionRequest,
        parent_work_item_id: &str,
        status: WorkItemStatus,
        observed_at: i64,
    ) -> Result<String> {
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        let mut artifact = store
            .get_work_item(&artifact_id)
            .cloned()
            .unwrap_or_else(|| WorkItem {
                id: artifact_id.clone(),
                project_id: request.repo_id.clone(),
                initiative_id: request.goal_scope.clone(),
                parent_work_item_id: Some(parent_work_item_id.to_string()),
                kind: WorkItemKind::DevelopmentArtifact,
                thread_id: request.thread_id.clone(),
                title: String::new(),
                summary: String::new(),
                status,
                priority: WorkPriority::P2,
                urgency: WorkUrgency::Normal,
                assignment_mode: AssignmentMode::ReviewOnly,
                requested_scopes: vec![ScopeRequirement {
                    scope: CollaborationScope::RepoBranchNamespace {
                        repo_id: request.repo_id.clone(),
                        pattern: request.branch.clone(),
                    },
                    mode: ScopeMode::SharedRead,
                    reason: "development artifact projection".to_string(),
                }],
                accepted_claim_ids: Vec::new(),
                assignee: Some(AgentAssignment {
                    agent_id: request.agent_id.clone(),
                    runner_id: None,
                    assigned_at: observed_at,
                }),
                blocked_by: Vec::new(),
                depends_on: Vec::new(),
                review_mode: ReviewMode::AutoReviewThenHumanIfNeeded,
                cancellation: None,
                created_at: observed_at,
                updated_at: observed_at,
            });
        artifact.initiative_id = request.goal_scope.clone();
        artifact.parent_work_item_id = Some(parent_work_item_id.to_string());
        artifact.thread_id = request.thread_id.clone().or(artifact.thread_id);
        artifact.title = format!("Implementation branch: {}", request.branch);
        artifact.summary = match request.goal_scope.as_deref() {
            Some(goal_scope) if !goal_scope.is_empty() => format!(
                "Tracks visible branch/PR/review lifecycle for repo '{}' branch '{}' under goal '{}'.",
                request.repo_id, request.branch, goal_scope
            ),
            _ => format!(
                "Tracks visible branch/PR/review lifecycle for repo '{}' branch '{}'.",
                request.repo_id, request.branch
            ),
        };
        artifact.assignee = Some(AgentAssignment {
            agent_id: request.agent_id.clone(),
            runner_id: None,
            assigned_at: observed_at,
        });
        artifact.set_status(status, observed_at);
        store.upsert_work_item(artifact)?;
        Ok(artifact_id)
    }

    fn sync_branch_artifact_status(
        &self,
        store: &mut ManagementStore,
        repo_id: &str,
        branch: &str,
        status: WorkItemStatus,
        observed_at: i64,
    ) -> Result<()> {
        let artifact_id = branch_artifact_work_item_id(repo_id, branch);
        let Some(existing) = store.get_work_item(&artifact_id).cloned() else {
            return Ok(());
        };
        let mut updated = existing;
        updated.set_status(status, observed_at);
        store.upsert_work_item(updated)
    }

    fn refresh_parent_task_status(
        &self,
        store: &mut ManagementStore,
        parent_work_item_id: &str,
        observed_at: i64,
    ) -> Result<()> {
        let Some(mut parent) = store.get_work_item(parent_work_item_id).cloned() else {
            return Ok(());
        };
        let children = store.work_items_for_parent(parent_work_item_id);
        if children.is_empty() {
            return Ok(());
        }

        let next_status = if children
            .iter()
            .any(|item| matches!(item.status, WorkItemStatus::Blocked))
        {
            WorkItemStatus::Blocked
        } else if children
            .iter()
            .any(|item| matches!(item.status, WorkItemStatus::PendingReview))
        {
            WorkItemStatus::PendingReview
        } else if children.iter().any(|item| {
            matches!(
                item.status,
                WorkItemStatus::Running | WorkItemStatus::Claimed | WorkItemStatus::ClaimPending
            )
        }) {
            WorkItemStatus::Running
        } else if children
            .iter()
            .any(|item| matches!(item.status, WorkItemStatus::Done))
        {
            WorkItemStatus::Done
        } else if children
            .iter()
            .any(|item| matches!(item.status, WorkItemStatus::Cancelled))
        {
            WorkItemStatus::Cancelled
        } else if children.iter().any(|item| {
            matches!(
                item.status,
                WorkItemStatus::Expired | WorkItemStatus::Failed
            )
        }) {
            WorkItemStatus::Failed
        } else {
            WorkItemStatus::Todo
        };

        parent.set_status(next_status, observed_at);
        store.upsert_work_item(parent)
    }

    fn authorized_push_ownership_request(
        &self,
        request: &PushAuthRequest,
        push_sessions: &HashMap<String, PushSession>,
        push_allowed: bool,
    ) -> Option<PushSessionRequest> {
        if !push_allowed {
            return None;
        }

        push_sessions
            .get(&request.push_session)
            .map(|session| PushSessionRequest {
                agent_id: session.agent_id.clone(),
                repo_id: session.repo_id.clone(),
                branch: request.branch.clone(),
                goal_scope: None,
                thread_id: session.thread_id.clone(),
            })
    }

    fn update_branch_work_item_status(
        &self,
        repo_id: &str,
        branch: &str,
        status: WorkItemStatus,
        now_ts: i64,
    ) -> Result<()> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|_| anyhow!("management store lock poisoned"))?;
        let work_item_id = branch_execution_work_item_id(repo_id, branch);
        let Some(existing) = store.get_work_item(&work_item_id).cloned() else {
            return Ok(());
        };
        let parent_work_item_id = existing.parent_work_item_id.clone();

        let mut updated = existing;
        updated.set_status(status, now_ts);
        store.upsert_work_item(updated)?;
        self.sync_branch_artifact_status(&mut store, repo_id, branch, status, now_ts)?;
        if let Some(parent_work_item_id) = parent_work_item_id.as_deref() {
            self.refresh_parent_task_status(&mut store, parent_work_item_id, now_ts)?;
        }
        Ok(())
    }

    fn finalize_branch_ownership(
        &self,
        repo_id: &str,
        branch: &str,
        status: WorkItemStatus,
        now_ts: i64,
    ) -> Result<()> {
        let mut store = self
            .management_store
            .lock()
            .map_err(|_| anyhow!("management store lock poisoned"))?;
        let work_item_id = branch_execution_work_item_id(repo_id, branch);
        let Some(existing) = store.get_work_item(&work_item_id).cloned() else {
            return Ok(());
        };
        let parent_work_item_id = existing.parent_work_item_id.clone();

        let revoked_claims = store.revoke_claims_for_work_item(&work_item_id, now_ts)?;
        let mut updated = existing;
        updated.set_status(status, now_ts);
        if !revoked_claims.is_empty() {
            updated.touch(now_ts);
        }
        store.upsert_work_item(updated)?;
        self.sync_branch_artifact_status(&mut store, repo_id, branch, status, now_ts)?;
        if let Some(parent_work_item_id) = parent_work_item_id.as_deref() {
            self.refresh_parent_task_status(&mut store, parent_work_item_id, now_ts)?;
        }
        Ok(())
    }

    fn complete_branch_ownership(&self, repo_id: &str, branch: &str, now_ts: i64) -> Result<()> {
        self.finalize_branch_ownership(repo_id, branch, WorkItemStatus::Done, now_ts)
    }

    fn cancel_branch_ownership(&self, repo_id: &str, branch: &str, now_ts: i64) -> Result<()> {
        self.finalize_branch_ownership(repo_id, branch, WorkItemStatus::Cancelled, now_ts)
    }

    // -----------------------------------------------------------------------
    // PR RPC handlers
    // -----------------------------------------------------------------------

    async fn rpc_pr_create(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let repo_id = param_str(&params, "repo_id")?;
        let branch = param_str(&params, "branch")?;
        let base = params
            .get("base")
            .and_then(|v| v.as_str())
            .unwrap_or("main");
        let title = param_str(&params, "title")?;
        let description = params
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let author = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let goal_scope = params
            .get("goal_scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let thread_id = params
            .get("thread_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);

        let server = self.git_server.lock().await;
        let mut merge_rules = server
            .get_repo(repo_id)
            .map(|r| r.default_merge_rules.clone())
            .unwrap_or_default();
        drop(server);

        if merge_rules.required_checks.is_empty() && !self.required_checks.is_empty() {
            merge_rules.required_checks = self.required_checks.clone();
        }

        let pr = {
            let mut pr_mgr = self.pr_manager.lock().await;
            let pr = pr_mgr.create_pr(
                repo_id,
                &branch,
                base,
                &title,
                description,
                author,
                merge_rules,
            )?;

            for check_name in &pr.merge_rules.required_checks {
                pr_mgr.update_check(
                    &pr.id,
                    check_name,
                    SWARM_DISPATCHER_AGENT,
                    CheckStatus::Pending,
                    None,
                )?;
            }

            pr_mgr
                .get(&pr.id)
                .cloned()
                .ok_or_else(|| anyhow!("PR disappeared after creation: {}", pr.id))?
        };

        self.sync_branch_ownership(
            &PushSessionRequest {
                agent_id: author.to_string(),
                repo_id: repo_id.to_string(),
                branch: branch.to_string(),
                goal_scope,
                thread_id,
            },
            now_unix(),
        )?;
        self.update_branch_work_item_status(
            repo_id,
            &branch,
            WorkItemStatus::PendingReview,
            now_unix() as i64,
        )?;

        let dispatch_plan = self.prepare_dispatch_plan(&pr).await?;
        let dispatch_result = self.execute_dispatch_plan(dispatch_plan).await;
        let mut output = format!("PR created: {} — '{}'", pr.id, pr.title);
        if !pr.merge_rules.required_checks.is_empty() {
            output.push_str(&format!(
                "\nInitialized checks: {}",
                pr.merge_rules.required_checks.join(", ")
            ));
        }
        output.push_str(&format!(
            "\nDispatch plan: {} CI VM(s), reviewer {}",
            dispatch_result.ci_vm_ids.len(),
            if dispatch_result.reviewer_vm_id.is_some() {
                "launched"
            } else {
                "not launched"
            }
        ));
        if !dispatch_result.warnings.is_empty() {
            output.push_str(&format!(
                "\nDispatch warnings: {}",
                dispatch_result.warnings.join("; ")
            ));
        }

        Ok(serde_json::json!({
            "success": true,
            "output": output,
            "pr_id": pr.id,
            "required_checks": pr.merge_rules.required_checks,
            "planned_ci_agents": dispatch_result.ci_vm_ids.len(),
            "reviewer_planned": dispatch_result.reviewer_vm_id.is_some(),
            "dispatch_warnings": dispatch_result.warnings,
        }))
    }

    async fn rpc_pr_review(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let pr_id = param_str(&params, "pr_id")?;
        let agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("reviewer");

        let (output, auto_merge, branch_status_update) = {
            let mut pr_mgr = self.pr_manager.lock().await;

            match method {
                "pr.approve" => {
                    let comment = params.get("comment").and_then(|v| v.as_str());
                    let comments = if let Some(body) = comment {
                        vec![symbiotic_git_swarm::ReviewComment {
                            file: String::new(),
                            line: None,
                            body: body.to_string(),
                        }]
                    } else {
                        vec![]
                    };
                    let pr = pr_mgr.add_review(
                        &pr_id,
                        agent_id,
                        symbiotic_git_swarm::ReviewVerdict::Approved,
                        comments,
                    )?;
                    (
                        format!("PR {} approved by {}", pr_id, agent_id),
                        pr.status == PRStatus::Approved,
                        None,
                    )
                }
                "pr.request_changes" => {
                    let raw_comments = params
                        .get("comments")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .unwrap_or_default();

                    let comments: Vec<symbiotic_git_swarm::ReviewComment> = raw_comments
                        .into_iter()
                        .filter_map(|c| {
                            Some(symbiotic_git_swarm::ReviewComment {
                                file: c.get("file")?.as_str()?.to_string(),
                                line: c.get("line").and_then(|v| v.as_u64()).map(|n| n as u32),
                                body: c.get("body")?.as_str()?.to_string(),
                            })
                        })
                        .collect();

                    let pr = pr_mgr.add_review(
                        &pr_id,
                        agent_id,
                        symbiotic_git_swarm::ReviewVerdict::ChangesRequested,
                        comments,
                    )?;
                    (
                        format!("Changes requested on PR {} by {}", pr_id, agent_id),
                        pr.status == PRStatus::Approved,
                        Some((
                            pr.repo_id.clone(),
                            pr.branch.clone(),
                            WorkItemStatus::Running,
                        )),
                    )
                }
                "pr.comment" => {
                    let body = param_str(&params, "body")?;
                    let file = params
                        .get("file")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let line = params
                        .get("line")
                        .and_then(|v| v.as_u64())
                        .map(|n| n as u32);

                    let comments = vec![symbiotic_git_swarm::ReviewComment {
                        file,
                        line,
                        body: body.to_string(),
                    }];
                    let pr = pr_mgr.add_review(
                        &pr_id,
                        agent_id,
                        symbiotic_git_swarm::ReviewVerdict::Commented,
                        comments,
                    )?;
                    (
                        format!("Comment added to PR {}", pr_id),
                        pr.status == PRStatus::Approved,
                        None,
                    )
                }
                _ => return Err(anyhow::anyhow!("unknown review method: {}", method)),
            }
        };

        if let Some((repo_id, branch, status)) = branch_status_update {
            self.update_branch_work_item_status(&repo_id, &branch, status, now_unix() as i64)?;
        }

        let output = self
            .append_auto_merge_output(output, &pr_id, auto_merge)
            .await?;

        Ok(serde_json::json!({
            "success": true,
            "output": output,
        }))
    }

    async fn rpc_pr_get_status(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let pr_id = param_str(&params, "pr_id")?;
        let pr_mgr = self.pr_manager.lock().await;

        let pr = pr_mgr
            .get(&pr_id)
            .ok_or_else(|| anyhow::anyhow!("PR not found: {}", pr_id))?;

        let eval = pr.merge_rules.evaluate(pr);

        Ok(serde_json::json!({
            "success": true,
            "output": serde_json::to_string_pretty(&serde_json::json!({
                "id": pr.id,
                "status": pr.status,
                "branch": pr.branch,
                "base": pr.base,
                "reviews": pr.reviews.len(),
                "checks": pr.checks.len(),
                "can_merge": eval.can_merge,
                "rule_checks": eval.checks,
            }))?,
        }))
    }

    async fn rpc_pr_merge(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let pr_id = param_str(&params, "pr_id")?;

        // Get PR details
        let pr_mgr = self.pr_manager.lock().await;
        let pr = pr_mgr
            .get(&pr_id)
            .ok_or_else(|| anyhow::anyhow!("PR not found: {}", pr_id))?;

        let eval = pr.merge_rules.evaluate(pr);
        if !eval.can_merge {
            let failed: Vec<&str> = eval
                .checks
                .iter()
                .filter(|c| !c.satisfied)
                .map(|c| c.description.as_str())
                .collect();
            return Ok(serde_json::json!({
                "success": false,
                "output": format!("Cannot merge — unsatisfied rules: {}", failed.join(", ")),
            }));
        }

        let repo_id = pr.repo_id.clone();
        let branch = pr.branch.clone();
        let base = pr.base.clone();
        drop(pr_mgr);

        // Perform the merge via the git server container
        let server = self.git_server.lock().await;
        match server.fast_forward_merge(&repo_id, &branch, &base).await {
            Ok(sha) => {
                drop(server);
                let mut pr_mgr = self.pr_manager.lock().await;
                pr_mgr.mark_merged(&pr_id)?;
                drop(pr_mgr);
                self.complete_branch_ownership(&repo_id, &branch, now_unix() as i64)?;
                Ok(serde_json::json!({
                    "success": true,
                    "output": format!("PR {} merged into {} (sha: {})", pr_id, base, sha),
                }))
            }
            Err(e) => {
                error!(pr_id = %pr_id, error = %e, "merge failed");
                Ok(serde_json::json!({
                    "success": false,
                    "output": format!("Merge failed: {}", e),
                }))
            }
        }
    }

    async fn rpc_pr_close(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let pr_id = param_str(&params, "pr_id")?;

        let (repo_id, branch) = {
            let mut pr_mgr = self.pr_manager.lock().await;
            let pr = pr_mgr
                .get(&pr_id)
                .ok_or_else(|| anyhow::anyhow!("PR not found: {}", pr_id))?
                .clone();
            pr_mgr.close(&pr_id)?;
            (pr.repo_id, pr.branch)
        };

        self.cancel_branch_ownership(&repo_id, &branch, now_unix() as i64)?;

        Ok(serde_json::json!({
            "success": true,
            "output": format!("PR {} closed without merge", pr_id),
        }))
    }

    async fn rpc_check_report(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let pr_id = param_str(&params, "pr_id")?;
        let check_name = param_str(&params, "check_name")?;
        let status = parse_check_status(params.get("status"))?;
        let agent_id = params
            .get("agent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("ci-agent");
        let output = params
            .get("output")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        let (should_try_merge, branch_status_update) = {
            let mut pr_mgr = self.pr_manager.lock().await;
            let pr = pr_mgr.update_check(&pr_id, check_name, agent_id, status, output)?;
            (
                pr.status == PRStatus::Approved,
                if status == CheckStatus::Failure {
                    Some((
                        pr.repo_id.clone(),
                        pr.branch.clone(),
                        WorkItemStatus::Running,
                    ))
                } else {
                    None
                },
            )
        };

        if let Some((repo_id, branch, status)) = branch_status_update {
            self.update_branch_work_item_status(&repo_id, &branch, status, now_unix() as i64)?;
        }

        let mut response_output = format!(
            "Check '{}' updated to {} on PR {}",
            check_name,
            check_status_label(status),
            pr_id
        );
        if should_try_merge {
            response_output = self
                .append_auto_merge_output(response_output, &pr_id, true)
                .await?;
        }

        Ok(serde_json::json!({
            "success": true,
            "output": response_output,
            "pr_id": pr_id,
            "check_name": check_name,
            "status": check_status_label(status),
        }))
    }

    // -----------------------------------------------------------------------
    // Repo RPC handlers
    // -----------------------------------------------------------------------

    async fn rpc_create_repo(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = param_str(&params, "id")?;
        let mut server = self.git_server.lock().await;
        let repo = server.create_repo(&id, None, None).await?;
        Ok(serde_json::json!({
            "success": true,
            "output": format!("Repo created: {}", repo.id),
            "repo_url": server.repo_url(&id),
        }))
    }

    async fn rpc_destroy_repo(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = param_str(&params, "id")?;
        let mut server = self.git_server.lock().await;
        server.destroy_repo(&id).await?;
        Ok(serde_json::json!({
            "success": true,
            "output": format!("Repo destroyed: {}", id),
        }))
    }

    async fn rpc_issue_push_session(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let request: PushSessionRequest = serde_json::from_value(params)?;
        Ok(serde_json::to_value(
            self.issue_push_session(request).await?,
        )?)
    }

    async fn append_auto_merge_output(
        &self,
        mut output: String,
        pr_id: &str,
        should_try_merge: bool,
    ) -> Result<String> {
        if !should_try_merge {
            return Ok(output);
        }

        let auto_merge = self.try_auto_merge_if_ready(pr_id).await?;
        if let Some(sha) = auto_merge.merged_sha {
            output.push_str(&format!("\nAuto-merged at {}", sha));
        }
        if let Some(warning) = auto_merge.warning {
            output.push_str(&format!("\n{}", warning));
        }
        if let Some(vm_id) = auto_merge.distillery_vm_id {
            output.push_str(&format!("\nDistillery dispatched: {}", vm_id));
        }

        Ok(output)
    }

    async fn try_auto_merge_if_ready(&self, pr_id: &str) -> Result<AutoMergeOutcome> {
        let (repo_id, branch, base, should_merge) = {
            let pr_mgr = self.pr_manager.lock().await;
            let pr = pr_mgr
                .get(pr_id)
                .ok_or_else(|| anyhow!("PR not found: {}", pr_id))?;
            let eval = pr.merge_rules.evaluate(pr);
            (
                pr.repo_id.clone(),
                pr.branch.clone(),
                pr.base.clone(),
                pr.status == PRStatus::Approved && eval.can_merge,
            )
        };

        if !should_merge {
            return Ok(AutoMergeOutcome::default());
        }

        let sha = {
            let server = self.git_server.lock().await;
            match server.fast_forward_merge(&repo_id, &branch, &base).await {
                Ok(sha) => sha,
                Err(e) => {
                    error!(pr_id = %pr_id, error = %e, "auto-merge failed");
                    return Ok(AutoMergeOutcome {
                        merged_sha: None,
                        warning: Some(format!(
                            "Auto-merge attempted for PR {} but the git merge failed: {}",
                            pr_id, e
                        )),
                        distillery_vm_id: None,
                    });
                }
            }
        };

        let mut pr_mgr = self.pr_manager.lock().await;
        if let Err(e) = pr_mgr.mark_merged(pr_id) {
            error!(pr_id = %pr_id, error = %e, "auto-merge bookkeeping failed");
            return Ok(AutoMergeOutcome {
                merged_sha: Some(sha),
                warning: Some(format!(
                    "Merged PR {} in git, but failed to update daemon state: {}",
                    pr_id, e
                )),
                distillery_vm_id: None,
            });
        }
        drop(pr_mgr);

        if let Err(e) = self.complete_branch_ownership(&repo_id, &branch, now_unix() as i64) {
            error!(pr_id = %pr_id, error = %e, "failed to complete branch ownership after auto-merge");
            return Ok(AutoMergeOutcome {
                merged_sha: Some(sha),
                warning: Some(format!(
                    "Merged PR {} in git, but failed to release branch ownership: {}",
                    pr_id, e
                )),
                distillery_vm_id: None,
            });
        }

        // Best-effort: spawn a distillery VM for post-merge linting + knowledge extraction
        let distillery_vm_id = self.dispatch_distillery(pr_id, &sha).await;
        if let Some(ref vm_id) = distillery_vm_id {
            info!(pr_id = %pr_id, vm_id = %vm_id, "distillery dispatched after auto-merge");
        }

        Ok(AutoMergeOutcome {
            merged_sha: Some(sha),
            warning: None,
            distillery_vm_id,
        })
    }

    async fn prepare_dispatch_plan(&self, pr: &SwarmPR) -> Result<DispatchPlan> {
        let repo_url = {
            let server = self.git_server.lock().await;
            server.repo_url(&pr.repo_id)
        };

        let socket_path = self.llm_gateway_socket.as_deref();
        let runner_binary = find_runner_binary_path()?;
        let warnings =
            dispatch_blockers(self.sandbox_manager.is_some(), socket_path, &runner_binary);
        if !warnings.is_empty() {
            return Ok(DispatchPlan {
                ci_jobs: vec![],
                reviewer_job: None,
                warnings,
            });
        }

        let socket_path = socket_path.expect("checked above");
        let ci_jobs = pr
            .merge_rules
            .required_checks
            .iter()
            .map(|check_name| build_ci_job(&repo_url, pr, check_name, socket_path, &runner_binary))
            .collect();
        let reviewer_job = Some(build_reviewer_job(
            &repo_url,
            pr,
            socket_path,
            &runner_binary,
        ));

        Ok(DispatchPlan {
            ci_jobs,
            reviewer_job,
            warnings,
        })
    }

    async fn execute_dispatch_plan(&self, plan: DispatchPlan) -> DispatchResult {
        let mut result = DispatchResult {
            warnings: plan.warnings,
            ..Default::default()
        };
        let Some(manager) = self.sandbox_manager.clone() else {
            return result;
        };

        for job in plan.ci_jobs {
            match self
                .launch_dispatch_job(Arc::clone(&manager), job.clone())
                .await
            {
                Ok(vm_id) => result.ci_vm_ids.push(vm_id),
                Err(e) => {
                    let message = format!("failed to launch CI job: {}", e);
                    warn!(error = %e, "swarm CI launch failed");
                    let agent_id = job.request.requesting_agent.clone();
                    let pr_manager = Arc::clone(&self.pr_manager);
                    if let Some(check_name) = job.check_name.clone() {
                        mark_check_dispatch_failure(
                            pr_manager,
                            job.pr_id.clone(),
                            check_name,
                            agent_id,
                            message.clone(),
                        )
                        .await;
                    }
                    result.warnings.push(message);
                }
            }
        }

        if let Some(job) = plan.reviewer_job {
            match self.launch_dispatch_job(manager, job).await {
                Ok(vm_id) => result.reviewer_vm_id = Some(vm_id),
                Err(e) => {
                    let message = format!("failed to launch reviewer job: {}", e);
                    warn!(error = %e, "swarm reviewer launch failed");
                    result.warnings.push(message);
                }
            }
        }

        result
    }

    async fn launch_dispatch_job(
        &self,
        manager: Arc<StdMutex<symbiotic_vm::manager::VmManager>>,
        job: DispatchJob,
    ) -> Result<String> {
        let request = job.request.clone();
        let command =
            append_bridge_runner_command(&self.broker, &request.requesting_agent, &job.command)?;
        let vm_id = create_and_start_vm(Arc::clone(&manager), request.clone()).await?;
        let spawned_vm_id = vm_id.clone();
        let pr_manager = Arc::clone(&self.pr_manager);
        tokio::spawn(async move {
            if let Err(e) = exec_and_destroy_vm(
                Arc::clone(&manager),
                spawned_vm_id.clone(),
                request.requesting_agent.clone(),
                command,
            )
            .await
            {
                error!(
                    vm_id = %spawned_vm_id,
                    pr_id = %job.pr_id,
                    error = %e,
                    "swarm VM execution failed"
                );
                if let Some(check_name) = job.check_name {
                    mark_check_dispatch_failure(
                        pr_manager,
                        job.pr_id,
                        check_name,
                        request.requesting_agent,
                        format!("dispatch execution failed: {}", e),
                    )
                    .await;
                }
            }
        });

        Ok(vm_id)
    }

    // -----------------------------------------------------------------------
    // Distillery (post-merge pipeline)
    // -----------------------------------------------------------------------

    /// Dispatch a distillery VM as a best-effort post-merge pipeline.
    ///
    /// The distillery agent clones the repo, runs `symbiotic-linter`, fixes
    /// any lint errors (creating a follow-up PR), and extracts knowledge
    /// artifacts. Failures are logged but never block the merge.
    async fn dispatch_distillery(&self, pr_id: &str, merged_sha: &str) -> Option<String> {
        let repo_url = {
            let pr_mgr = self.pr_manager.lock().await;
            let pr = match pr_mgr.get(pr_id) {
                Some(pr) => pr,
                None => {
                    warn!(pr_id = %pr_id, "distillery: PR not found, skipping");
                    return None;
                }
            };
            let server = self.git_server.lock().await;
            server.repo_url(&pr.repo_id)
        };

        let (repo_id, base_branch) = {
            let pr_mgr = self.pr_manager.lock().await;
            let pr = pr_mgr.get(pr_id)?;
            (pr.repo_id.clone(), pr.base.clone())
        };

        let socket_path = match self.llm_gateway_socket.as_deref() {
            Some(path) => path,
            None => {
                warn!("distillery: no LLM gateway socket configured, skipping");
                return None;
            }
        };

        let runner_binary = match find_runner_binary_path() {
            Ok(path) => path,
            Err(e) => {
                warn!(error = %e, "distillery: runner binary not found, skipping");
                return None;
            }
        };

        let manager = match self.sandbox_manager.clone() {
            Some(m) => m,
            None => {
                warn!("distillery: sandbox manager unavailable, skipping");
                return None;
            }
        };

        let job = build_distillery_job(
            &repo_url,
            &repo_id,
            pr_id,
            merged_sha,
            &base_branch,
            socket_path,
            &runner_binary,
        );

        let vm_id = match create_and_start_vm(Arc::clone(&manager), job.request.clone()).await {
            Ok(id) => id,
            Err(e) => {
                warn!(pr_id = %pr_id, error = %e, "distillery: VM creation failed");
                return None;
            }
        };

        let spawned_vm_id = vm_id.clone();
        let pr_id_owned = pr_id.to_string();
        let merged_sha_owned = merged_sha.to_string();
        let command = match append_bridge_runner_command(
            &self.broker,
            &job.request.requesting_agent,
            &job.command,
        ) {
            Ok(command) => command,
            Err(e) => {
                warn!(pr_id = %pr_id, error = %e, "distillery: failed to issue bridge token");
                return None;
            }
        };
        let agent_id = job.request.requesting_agent.clone();
        let archive_root = self.archive_root.clone();
        let vm_output_root = self.vm_output_root.clone();
        tokio::spawn(async move {
            if let Err(e) = exec_extract_distillery_bundle_and_destroy_vm(
                manager,
                spawned_vm_id.clone(),
                agent_id,
                command,
                pr_id_owned.clone(),
                merged_sha_owned,
                archive_root,
                vm_output_root,
            )
            .await
            {
                warn!(
                    vm_id = %spawned_vm_id,
                    pr_id = %pr_id_owned,
                    error = %e,
                    "distillery VM execution failed (best-effort)"
                );
            } else {
                info!(
                    vm_id = %spawned_vm_id,
                    pr_id = %pr_id_owned,
                    "distillery pipeline completed"
                );
            }
        });

        Some(vm_id)
    }

    async fn rpc_distillery_status(&self, params: serde_json::Value) -> Result<serde_json::Value> {
        let vm_id = param_str(&params, "vm_id")?;
        let manager = self
            .sandbox_manager
            .as_ref()
            .ok_or_else(|| anyhow!("sandbox manager unavailable"))?;
        let guard = manager
            .lock()
            .map_err(|_| anyhow!("sandbox manager lock poisoned"))?;
        let vm_id_owned = vm_id.to_string();
        let running = guard
            .get_instance(&vm_id_owned)
            .map(|inst| inst.state == symbiotic_vm::types::VmState::Running)
            .unwrap_or(false);
        Ok(serde_json::json!({
            "success": true,
            "output": format!("Distillery VM {} is {}", vm_id, if running { "running" } else { "finished or unknown" }),
            "vm_id": vm_id,
            "running": running,
        }))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn param_str<'a>(params: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing required param: {}", key))
}

/// Build GitSwarmConfig from environment variables.
fn config_from_env() -> GitSwarmConfig {
    let mut config = GitSwarmConfig::default();

    if let Ok(image) = std::env::var("SYMBIOTIC_GIT_SERVER_IMAGE") {
        config.git_server_image = image;
    }
    if let Ok(port) = std::env::var("SYMBIOTIC_GIT_SERVER_PORT") {
        if let Ok(p) = port.parse() {
            config.git_server_port = p;
        }
    }
    if let Ok(bind) = std::env::var("SYMBIOTIC_GIT_BIND_ADDR") {
        config.git_server_bind = bind;
    }
    let required_checks = configured_checks_from_env();
    if !required_checks.is_empty() {
        config.default_merge_rules.required_checks = required_checks;
    }

    config
}

fn configured_checks_from_env() -> Vec<String> {
    std::env::var(SWARM_REQUIRED_CHECKS_ENV)
        .ok()
        .map(|value| parse_required_checks(&value))
        .unwrap_or_default()
}

fn parse_required_checks(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_check_status(status: Option<&serde_json::Value>) -> Result<CheckStatus> {
    match status.and_then(|value| value.as_str()) {
        Some("pending") => Ok(CheckStatus::Pending),
        Some("running") => Ok(CheckStatus::Running),
        Some("success") => Ok(CheckStatus::Success),
        Some("failure") => Ok(CheckStatus::Failure),
        Some(other) => Err(anyhow!("unsupported check status: {}", other)),
        None => Err(anyhow!("missing required param: status")),
    }
}

fn check_status_label(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Pending => "pending",
        CheckStatus::Running => "running",
        CheckStatus::Success => "success",
        CheckStatus::Failure => "failure",
    }
}

fn build_ci_job(
    repo_url: &str,
    pr: &SwarmPR,
    check_name: &str,
    socket_path: &str,
    runner_binary: &Path,
) -> DispatchJob {
    let agent_id = format!("swarm-ci-{}", sanitize_for_agent_id(check_name));
    let request = build_swarm_vm_request(
        swarm_agent_image(),
        agent_id.clone(),
        format!("Run required check '{}' for PR {}", check_name, pr.id),
        default_swarm_network_policy(repo_url),
        swarm_vm_env(
            repo_url,
            &pr.repo_id,
            socket_path,
            &pr.id,
            pr.base.as_str(),
            pr.branch.as_str(),
            Some(check_name),
            swarm_check_command(check_name),
        ),
        swarm_mounts(socket_path, runner_binary),
    );
    let command = build_runner_command(&[
        "--socket",
        socket_path,
        "--agent-id",
        &agent_id,
        "--workspace",
        SWARM_WORKSPACE_PATH,
        "--ci-check",
        check_name,
        "--branch",
        &pr.branch,
    ]);

    DispatchJob {
        request,
        command,
        pr_id: pr.id.clone(),
        check_name: Some(check_name.to_string()),
    }
}

fn build_reviewer_job(
    repo_url: &str,
    pr: &SwarmPR,
    socket_path: &str,
    runner_binary: &Path,
) -> DispatchJob {
    let agent_id = format!("swarm-reviewer-{}", sanitize_for_agent_id(&pr.id));
    let request = build_swarm_vm_request(
        swarm_agent_image(),
        agent_id.clone(),
        format!("Review PR {} from branch {}", pr.id, pr.branch),
        default_swarm_network_policy(repo_url),
        swarm_vm_env(
            repo_url,
            &pr.repo_id,
            socket_path,
            &pr.id,
            pr.base.as_str(),
            pr.branch.as_str(),
            None,
            None,
        ),
        swarm_mounts(socket_path, runner_binary),
    );
    let command = build_runner_command(&[
        "--socket",
        socket_path,
        "--agent-id",
        &agent_id,
        "--workspace",
        SWARM_WORKSPACE_PATH,
        "--review-pr",
        "--branch",
        &pr.branch,
        "--base-branch",
        &pr.base,
    ]);

    DispatchJob {
        request,
        command,
        pr_id: pr.id.clone(),
        check_name: None,
    }
}

fn build_distillery_job(
    repo_url: &str,
    repo_id: &str,
    pr_id: &str,
    merged_sha: &str,
    base_branch: &str,
    socket_path: &str,
    runner_binary: &Path,
) -> DispatchJob {
    let agent_id = format!("swarm-distillery-{}", sanitize_for_agent_id(pr_id));
    let linter_binary = find_linter_binary_path();
    let mut mounts = swarm_mounts(socket_path, runner_binary);
    if let Some(ref linter_path) = linter_binary {
        mounts.push(BindMount {
            host_path: linter_path.display().to_string(),
            vm_path: SWARM_LINTER_PATH.to_string(),
            read_only: true,
        });
    }

    let request = build_swarm_vm_request(
        swarm_agent_image(),
        agent_id.clone(),
        format!(
            "distillery-post-merge for PR {} (sha {})",
            pr_id, merged_sha
        ),
        default_swarm_network_policy(repo_url),
        distillery_vm_env(
            repo_url,
            repo_id,
            socket_path,
            pr_id,
            merged_sha,
            base_branch,
        ),
        mounts,
    );
    let mut command = build_runner_command(&[
        "--socket",
        socket_path,
        "--agent-id",
        &agent_id,
        "--workspace",
        SWARM_WORKSPACE_PATH,
        "--branch",
        base_branch,
    ]);
    command = append_runner_arg(
        &command,
        "--goal",
        &format!(
            "Run the post-merge distillery workflow for merged PR {} on branch {} at commit {}. Clone the swarm repository, run the linter when available, open a follow-up fix PR for lint issues, and otherwise report a concise architectural summary through check_report.",
            pr_id, base_branch, merged_sha
        ),
    );
    command = append_runner_arg(&command, "--system-prompt", DISTILLERY_AGENT_SYSTEM_PROMPT);

    DispatchJob {
        request,
        command,
        pr_id: pr_id.to_string(),
        check_name: None,
    }
}

fn distillery_vm_env(
    repo_url: &str,
    repo_id: &str,
    socket_path: &str,
    pr_id: &str,
    merged_sha: &str,
    base_branch: &str,
) -> Vec<String> {
    vec![
        format!("GIT_SERVER_URL={repo_url}"),
        format!("SWARM_REPO_ID={repo_id}"),
        format!("PR_ID={pr_id}"),
        format!("CHECK_NAME={DISTILLERY_CHECK_NAME}"),
        format!("SYMBIOTIC_SOCKET={socket_path}"),
        "DISTILLERY_MODE=post-merge".to_string(),
        format!("MERGED_PR_ID={pr_id}"),
        format!("MERGED_SHA={merged_sha}"),
        format!("SWARM_BASE_BRANCH={base_branch}"),
    ]
}

/// Locate the `symbiotic-linter` binary next to the agent runner, if it exists.
fn find_linter_binary_path() -> Option<PathBuf> {
    // Check next to the runner binary first
    if let Ok(runner_path) = find_runner_binary_path() {
        let linter_path = runner_path.with_file_name("symbiotic-linter");
        if linter_path.is_file() {
            return Some(linter_path);
        }
    }

    // Fall back to standard build locations
    if let Ok(repo_root) = std::env::current_dir() {
        let candidates = [
            repo_root.join("submodules/runtime/target/release/symbiotic-linter"),
            repo_root.join("submodules/runtime/target/debug/symbiotic-linter"),
        ];
        for candidate in &candidates {
            if candidate.is_file() {
                return Some(candidate.clone());
            }
        }
    }

    None
}

fn build_swarm_vm_request(
    image: String,
    requesting_agent: String,
    purpose: String,
    network: NetworkPolicy,
    env: Vec<String>,
    mounts: Vec<BindMount>,
) -> VmCreateRequest {
    VmCreateRequest {
        image,
        resources: VmResources::default(),
        network,
        inject_files: vec![],
        requesting_agent,
        purpose,
        env,
        mounts,
    }
}

fn swarm_vm_env(
    repo_url: &str,
    repo_id: &str,
    socket_path: &str,
    pr_id: &str,
    base_branch: &str,
    head_branch: &str,
    check_name: Option<&str>,
    check_command: Option<String>,
) -> Vec<String> {
    let mut env = vec![
        format!("GIT_SERVER_URL={repo_url}"),
        format!("SWARM_REPO_ID={repo_id}"),
        format!("PR_ID={pr_id}"),
        format!("SYMBIOTIC_SOCKET={socket_path}"),
        format!("SWARM_BASE_BRANCH={base_branch}"),
        format!("SWARM_HEAD_BRANCH={head_branch}"),
    ];
    if let Some(check_name) = check_name {
        env.push(format!("CHECK_NAME={check_name}"));
    }
    if let Some(check_command) = check_command {
        env.push(format!("CHECK_COMMAND={check_command}"));
    }
    env
}

fn swarm_mounts(socket_path: &str, runner_binary: &Path) -> Vec<BindMount> {
    vec![
        BindMount {
            host_path: socket_path.to_string(),
            vm_path: socket_path.to_string(),
            read_only: false,
        },
        BindMount {
            host_path: runner_binary.display().to_string(),
            vm_path: SWARM_RUNNER_PATH.to_string(),
            read_only: true,
        },
    ]
}

fn swarm_agent_image() -> String {
    std::env::var(SWARM_AGENT_IMAGE_ENV).unwrap_or_else(|_| SWARM_AGENT_VM_IMAGE.to_string())
}

fn default_swarm_network_policy(repo_url: &str) -> NetworkPolicy {
    let (allowed_domains, allowed_ports) = parse_repo_network_allowlist(repo_url);
    NetworkPolicy {
        deny_all: false,
        allowed_domains,
        allowed_ports,
        dns_servers: Vec::new(),
    }
}

fn parse_repo_network_allowlist(repo_url: &str) -> (Vec<String>, Vec<u16>) {
    let parsed = match url::Url::parse(repo_url) {
        Ok(parsed) => parsed,
        Err(_) => {
            return (Vec::new(), Vec::new());
        }
    };

    let allowed_domains = parsed
        .host_str()
        .map(|host| vec![host.to_string()])
        .unwrap_or_default();
    let allowed_ports = parsed.port().map(|port| vec![port]).unwrap_or_else(|| {
        vec![match parsed.scheme() {
            "https" => 443,
            _ => 80,
        }]
    });

    (allowed_domains, allowed_ports)
}

fn build_runner_command(args: &[&str]) -> String {
    let mut parts = vec![shell_quote(SWARM_RUNNER_PATH)];
    parts.extend(args.iter().map(|arg| shell_quote(arg)));
    parts.join(" ")
}

fn append_runner_arg(command: &str, flag: &str, value: &str) -> String {
    format!("{command} {} {}", shell_quote(flag), shell_quote(value))
}

fn issue_bridge_session_token(
    broker: &Arc<StdMutex<AccessBroker>>,
    agent_id: &str,
) -> Result<String> {
    let mut broker = broker
        .lock()
        .map_err(|_| anyhow!("access broker lock poisoned"))?;
    let now = now_unix();
    let token_id = format!("swarm-bridge-{}", Uuid::new_v4());
    broker.issue_token(CapabilityToken {
        token_id: token_id.clone(),
        subject: agent_id.to_string(),
        trust_level: AgentTrustLevel::ReadOnly,
        scopes: ["bridge.connect".to_string(), "llm.chat".to_string()]
            .into_iter()
            .collect(),
        expires_at: now + 3600,
        one_time: false,
        consumed: false,
        goal_scope: None,
    });
    Ok(token_id)
}

fn append_bridge_runner_command(
    broker: &Arc<StdMutex<AccessBroker>>,
    agent_id: &str,
    command: &str,
) -> Result<String> {
    let token = issue_bridge_session_token(broker, agent_id)?;
    Ok(append_runner_arg(command, "--gateway-token", &token))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn swarm_check_command(check_name: &str) -> Option<String> {
    match check_name {
        "cargo-test" => Some("cargo test".to_string()),
        "cargo-check" => Some("cargo check".to_string()),
        "cargo-fmt" => Some("cargo fmt --check".to_string()),
        "cargo-clippy" => {
            Some("cargo clippy --all-targets --all-features -- -D warnings".to_string())
        }
        _ => None,
    }
}

fn find_runner_binary_path() -> Result<PathBuf> {
    if let Ok(value) = std::env::var(SWARM_AGENT_RUNNER_BINARY_ENV) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Ok(path);
        }
        return Err(anyhow!(
            "{} points to a missing binary: {}",
            SWARM_AGENT_RUNNER_BINARY_ENV,
            path.display()
        ));
    }

    let repo_root = std::env::current_dir()?;
    let candidates = [
        repo_root.join("submodules/runtime/target/release/symbiotic-agent-runner"),
        repo_root.join("submodules/runtime/target/debug/symbiotic-agent-runner"),
    ];
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| {
            anyhow!(
                "symbiotic-agent-runner binary not found; build it first or set {}",
                SWARM_AGENT_RUNNER_BINARY_ENV
            )
        })
}

fn dispatch_blockers(
    has_sandbox_manager: bool,
    socket_path: Option<&str>,
    runner_binary: &Path,
) -> Vec<String> {
    let mut warnings = Vec::new();
    if !has_sandbox_manager {
        warnings.push("sandbox manager unavailable".to_string());
    }
    match socket_path {
        Some(path) => {
            if !Path::new(path).exists() {
                warnings.push(format!("LLM gateway socket does not exist at {}", path));
            }
        }
        None => {
            warnings.push("SYMBIOTIC_LLM_GATEWAY_SOCKET is not configured".to_string());
        }
    }
    if !runner_binary.is_file() {
        warnings.push(format!(
            "symbiotic-agent-runner binary missing at {}",
            runner_binary.display()
        ));
    }
    warnings
}

fn internal_vm_broker(agent_id: &str, now: u64) -> (AccessBroker, String) {
    let token_id = format!("swarm-vm-token-{}", Uuid::new_v4());
    let mut broker = AccessBroker::new();
    broker.issue_token(CapabilityToken {
        token_id: token_id.clone(),
        subject: agent_id.to_string(),
        trust_level: AgentTrustLevel::ExternalAct,
        scopes: [
            "vm.create".to_string(),
            "vm.exec".to_string(),
            "vm.file.inject".to_string(),
            "vm.file.extract".to_string(),
            "vm.destroy".to_string(),
        ]
        .into_iter()
        .collect(),
        expires_at: now + 3600,
        one_time: false,
        consumed: false,
        goal_scope: None,
    });
    (broker, token_id)
}

async fn create_and_start_vm(
    manager: Arc<StdMutex<symbiotic_vm::manager::VmManager>>,
    request: VmCreateRequest,
) -> Result<String> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let now = now_unix();
            let (mut broker, token_id) = internal_vm_broker(&request.requesting_agent, now);
            let mut guard = manager
                .lock()
                .map_err(|_| anyhow!("sandbox manager lock poisoned"))?;
            let vm_id = guard
                .create(request.clone(), &mut broker, &token_id, now)
                .await?;
            guard
                .start(
                    &vm_id,
                    &request.requesting_agent,
                    &mut broker,
                    &token_id,
                    now + 1,
                )
                .await?;
            Ok::<String, anyhow::Error>(vm_id)
        })
    })
    .await
    .map_err(|e| anyhow!("VM create/start task join failed: {}", e))?
}

async fn exec_and_destroy_vm(
    manager: Arc<StdMutex<symbiotic_vm::manager::VmManager>>,
    vm_id: String,
    agent_id: String,
    command: String,
) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let now = now_unix();
            let (mut broker, token_id) = internal_vm_broker(&agent_id, now);
            let mut guard = manager
                .lock()
                .map_err(|_| anyhow!("sandbox manager lock poisoned"))?;
            let exec_result = guard
                .exec(&vm_id, &command, &agent_id, &mut broker, &token_id, now + 2)
                .await;
            let destroy_result = guard
                .destroy(&vm_id, &agent_id, &mut broker, &token_id, now + 3)
                .await;

            let exec_result = exec_result?;
            destroy_result?;

            if exec_result.exit_code == 0 {
                Ok::<(), anyhow::Error>(())
            } else {
                Err(anyhow!(
                    "runner exited with status {}: stdout={} stderr={}",
                    exec_result.exit_code,
                    exec_result.stdout.trim(),
                    exec_result.stderr.trim()
                ))
            }
        })
    })
    .await
    .map_err(|e| anyhow!("VM exec/destroy task join failed: {}", e))?
}

async fn exec_extract_distillery_bundle_and_destroy_vm(
    manager: Arc<StdMutex<symbiotic_vm::manager::VmManager>>,
    vm_id: String,
    agent_id: String,
    command: String,
    pr_id: String,
    merged_sha: String,
    archive_root: PathBuf,
    vm_output_root: PathBuf,
) -> Result<()> {
    let handle = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            let now = now_unix();
            let (mut broker, token_id) = internal_vm_broker(&agent_id, now);
            let mut guard = manager
                .lock()
                .map_err(|_| anyhow!("sandbox manager lock poisoned"))?;

            let exec_result = guard
                .exec(&vm_id, &command, &agent_id, &mut broker, &token_id, now + 2)
                .await;
            let exec_result = exec_result?;
            let bundle_result: Result<()> = async {
                guard
                    .transfer_file(
                        &vm_id,
                        &symbiotic_vm::types::FileTransfer {
                            host_path: "distillery-bundle.json".to_string(),
                            vm_path: DISTILLERY_BUNDLE_VM_PATH.to_string(),
                            direction: symbiotic_vm::types::TransferDirection::VmToHost,
                        },
                        &agent_id,
                        &mut broker,
                        &token_id,
                        now + 3,
                    )
                    .await
                    .context("distillery bundle manifest extraction failed")?;

                extract_distillery_bundle_artifacts(
                    &vm_output_root,
                    &vm_id,
                    &pr_id,
                    &mut guard,
                    &agent_id,
                    &mut broker,
                    &token_id,
                    now + 4,
                )
                .await
                .context("distillery bundle artifact extraction failed")?;

                persist_distillery_archive_bundle(
                    &archive_root,
                    &vm_output_root,
                    &vm_id,
                    &pr_id,
                    &merged_sha,
                )
                .context("distillery archive persistence failed")?;

                Ok(())
            }
            .await;

            let destroy_result = guard
                .destroy(&vm_id, &agent_id, &mut broker, &token_id, now + 5)
                .await;
            drop(guard);

            destroy_result?;

            if exec_result.exit_code == 0 {
                bundle_result?;
                Ok::<(), anyhow::Error>(())
            } else {
                Err(anyhow!(
                    "runner exited with status {}: stdout={} stderr={}",
                    exec_result.exit_code,
                    exec_result.stdout.trim(),
                    exec_result.stderr.trim()
                ))
            }
        })
    })
    .await
    .map_err(|e| anyhow!("VM exec/extract/destroy task join failed: {}", e))?
}

async fn extract_distillery_bundle_artifacts(
    vm_output_root: &Path,
    vm_id: &str,
    pr_id: &str,
    manager: &mut symbiotic_vm::manager::VmManager,
    agent_id: &str,
    broker: &mut AccessBroker,
    token_id: &str,
    now: u64,
) -> Result<()> {
    let bundle = load_distillery_archive_bundle(vm_output_root, vm_id)?;
    for artifact in &bundle.artifacts {
        let relative = validate_distillery_relative_path(&artifact.relative_path)
            .with_context(|| format!("invalid artifact path in distillery bundle for {pr_id}"))?;
        manager
            .transfer_file(
                &vm_id.to_string(),
                &symbiotic_vm::types::FileTransfer {
                    host_path: format!("artifacts/{}", relative.display()),
                    vm_path: format!("{DISTILLERY_OUTPUT_VM_ROOT}/{}", relative.display()),
                    direction: symbiotic_vm::types::TransferDirection::VmToHost,
                },
                agent_id,
                broker,
                token_id,
                now,
            )
            .await?;
    }
    Ok(())
}

fn persist_distillery_archive_bundle(
    archive_root: &Path,
    vm_output_root: &Path,
    vm_id: &str,
    pr_id: &str,
    merged_sha: &str,
) -> Result<PathBuf> {
    let bundle = load_distillery_archive_bundle(vm_output_root, vm_id)?;

    let notes_dir = archive_root.join("operations").join("swarm-distillery");
    std::fs::create_dir_all(&notes_dir)?;

    let note_path = notes_dir.join(format!("{}.md", sanitize_for_agent_id(pr_id)));
    std::fs::write(
        &note_path,
        render_distillery_archive_note(pr_id, merged_sha, &bundle.report),
    )?;

    persist_distillery_archive_artifacts(archive_root, vm_output_root, vm_id, pr_id, &bundle)?;

    Ok(note_path)
}

fn load_distillery_archive_bundle(
    vm_output_root: &Path,
    vm_id: &str,
) -> Result<DistilleryArchiveBundle> {
    let bundle_path = vm_output_root.join(vm_id).join("distillery-bundle.json");
    if !bundle_path.is_file() {
        return Err(anyhow!(
            "distillery bundle missing at {}",
            bundle_path.display()
        ));
    }

    let raw = std::fs::read_to_string(&bundle_path)?;
    let bundle: DistilleryArchiveBundle =
        serde_json::from_str(&raw).context("invalid distillery bundle JSON")?;
    if bundle.version != 1 {
        return Err(anyhow!(
            "unsupported distillery bundle version {}",
            bundle.version
        ));
    }
    Ok(bundle)
}

fn persist_distillery_archive_artifacts(
    archive_root: &Path,
    vm_output_root: &Path,
    vm_id: &str,
    pr_id: &str,
    bundle: &DistilleryArchiveBundle,
) -> Result<()> {
    for (index, artifact) in bundle.artifacts.iter().enumerate() {
        let relative = validate_distillery_relative_path(&artifact.relative_path)?;
        let extracted_path = vm_output_root.join(vm_id).join("artifacts").join(relative);
        if !extracted_path.is_file() {
            return Err(anyhow!(
                "distillery artifact missing at {}",
                extracted_path.display()
            ));
        }
        let body = std::fs::read_to_string(&extracted_path)?;
        let artifact_dir = archive_root
            .join("operations")
            .join("swarm-distillery")
            .join("artifacts")
            .join(artifact_kind_dir(artifact.kind));
        std::fs::create_dir_all(&artifact_dir)?;
        let file_name = format!(
            "{}-{}-{}.md",
            sanitize_for_agent_id(pr_id),
            index + 1,
            sanitize_for_agent_id(&artifact.title)
        );
        let artifact_path = artifact_dir.join(file_name);
        std::fs::write(
            &artifact_path,
            render_distillery_artifact_note(pr_id, artifact, &body),
        )?;
    }
    Ok(())
}

fn validate_distillery_relative_path(relative_path: &str) -> Result<&Path> {
    let path = Path::new(relative_path);
    if path.is_absolute() {
        return Err(anyhow!("artifact path must be relative"));
    }
    if path
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(anyhow!(
            "artifact path must not contain traversal components"
        ));
    }
    if path.extension().and_then(|value| value.to_str()) != Some("md") {
        return Err(anyhow!("artifact path must end with .md"));
    }
    Ok(path)
}

fn artifact_kind_dir(kind: DistilleryArtifactKind) -> &'static str {
    match kind {
        DistilleryArtifactKind::Methodology => "methodology",
        DistilleryArtifactKind::Decision => "decisions",
        DistilleryArtifactKind::Pattern => "patterns",
    }
}

fn render_distillery_artifact_note(
    pr_id: &str,
    artifact: &DistilleryArchiveArtifact,
    body: &str,
) -> String {
    let mut doc = String::new();
    writeln!(doc, "---").unwrap();
    writeln!(doc, "type: methodology").unwrap();
    writeln!(doc, "source: swarm-distillery").unwrap();
    writeln!(doc, "pr_id: {pr_id}").unwrap();
    writeln!(doc, "artifact_kind: {}", artifact_kind_dir(artifact.kind)).unwrap();
    writeln!(doc, "title: {}", artifact.title.trim()).unwrap();
    writeln!(doc, "---").unwrap();
    writeln!(doc).unwrap();
    writeln!(doc, "# {}", artifact.title.trim()).unwrap();
    writeln!(doc).unwrap();
    writeln!(doc, "{}", body.trim()).unwrap();
    writeln!(doc).unwrap();
    doc
}

fn render_distillery_archive_note(
    pr_id: &str,
    merged_sha: &str,
    report: &DistilleryArchiveReport,
) -> String {
    let mut doc = String::new();
    writeln!(doc, "---").unwrap();
    writeln!(doc, "type: methodology").unwrap();
    writeln!(doc, "source: swarm-distillery").unwrap();
    writeln!(doc, "pr_id: {pr_id}").unwrap();
    writeln!(doc, "merged_sha: {merged_sha}").unwrap();
    if let Some(status) = report.lint_status.as_deref() {
        writeln!(doc, "lint_status: {status}").unwrap();
    }
    writeln!(doc, "---").unwrap();
    writeln!(doc).unwrap();
    writeln!(doc, "# Swarm Distillery {pr_id}").unwrap();
    writeln!(doc).unwrap();
    writeln!(doc, "{}", report.summary_markdown.trim()).unwrap();
    writeln!(doc).unwrap();

    if !report.decisions.is_empty() {
        writeln!(doc, "## Decisions").unwrap();
        writeln!(doc).unwrap();
        for item in &report.decisions {
            writeln!(doc, "- {}", item.trim()).unwrap();
        }
        writeln!(doc).unwrap();
    }

    if !report.patterns.is_empty() {
        writeln!(doc, "## Patterns").unwrap();
        writeln!(doc).unwrap();
        for item in &report.patterns {
            writeln!(doc, "- {}", item.trim()).unwrap();
        }
        writeln!(doc).unwrap();
    }

    if let Some(title) = report.follow_up_pr_title.as_deref() {
        writeln!(doc, "## Follow-up").unwrap();
        writeln!(doc).unwrap();
        writeln!(doc, "- Follow-up PR opened: {}", title.trim()).unwrap();
    }

    doc
}

async fn mark_check_dispatch_failure(
    pr_manager: Arc<Mutex<PRManager>>,
    pr_id: String,
    check_name: String,
    agent_id: String,
    message: String,
) {
    let mut pr_manager = pr_manager.lock().await;
    let _ = pr_manager.update_check(
        &pr_id,
        &check_name,
        &agent_id,
        CheckStatus::Failure,
        Some(message),
    );
}

fn sanitize_for_agent_id(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    sanitized.trim_matches('-').to_string()
}

fn authorize_push_for_repo(
    repo: Option<&SwarmRepo>,
    broker: &mut AccessBroker,
    push_sessions: &mut HashMap<String, PushSession>,
    request: &PushAuthRequest,
    now: u64,
    durable_manifest: Option<&symbiotic_control_plane::RepoManifest>,
) -> PushAuthResponse {
    let repo = match repo {
        Some(repo) => repo,
        None => return deny_push(format!("unknown repo: {}", request.repo_id)),
    };

    let rule = match repo.branch_rule_for(&request.branch) {
        Some(rule) => rule,
        None => {
            return deny_push(format!(
                "no branch rule matched branch '{}'",
                request.branch
            ));
        }
    };

    cleanup_expired_sessions(push_sessions, now);

    let session = match push_sessions.get(&request.push_session) {
        Some(session) => session,
        None => return deny_push("invalid or expired push session"),
    };

    if session.repo_id != request.repo_id {
        return deny_push("push session repo mismatch");
    }

    let has_scope = rule.allowed_push_scopes.iter().any(|scope| {
        broker
            .evaluate(
                &session.token_id,
                &AccessRequest {
                    subject: session.agent_id.clone(),
                    required_level: required_trust_for_scope(scope),
                    scope: scope.to_string(),
                    goal_scope: None,
                },
                now,
            )
            .is_ok()
    });
    if !has_scope {
        return deny_push(format!(
            "agent '{}' lacks push scope for branch '{}'",
            session.agent_id, request.branch
        ));
    }

    if rule.require_pr {
        return deny_push(format!(
            "branch '{}' is protected and requires a PR merge",
            request.branch
        ));
    }

    // D2 defense-in-depth: consult durable RepoManifest if one exists.
    // Ephemeral-repo rules cover the per-goal feature-branch case; this
    // gate covers the durable mirror case where an agent pushes to a
    // branch that the operator has flagged as protected at the manifest
    // level (typically main/preview of an attached source repo).
    if let Some(manifest) = durable_manifest {
        if manifest
            .source
            .protected_branches
            .iter()
            .any(|b| b == &request.branch)
        {
            let has_external_push_scope = broker
                .evaluate(
                    &session.token_id,
                    &AccessRequest {
                        subject: session.agent_id.clone(),
                        required_level: AgentTrustLevel::ExternalAct,
                        scope: "git.push_external".to_string(),
                        goal_scope: None,
                    },
                    now,
                )
                .is_ok();
            if !has_external_push_scope {
                return deny_push(format!(
                    "branch '{}' is protected on durable manifest '{}'; \
                     push requires an approved git.push_external token",
                    request.branch, manifest.id,
                ));
            }
        }
    }

    PushAuthResponse {
        allowed: true,
        reason: None,
    }
}

fn issue_push_session_for_repo(
    repo: Option<&SwarmRepo>,
    broker: &mut AccessBroker,
    push_sessions: &mut HashMap<String, PushSession>,
    request: &PushSessionRequest,
    now: u64,
) -> Result<PushSessionResponse> {
    let repo = repo.ok_or_else(|| anyhow!("unknown repo: {}", request.repo_id))?;
    let rule = repo
        .branch_rule_for(&request.branch)
        .ok_or_else(|| anyhow!("no branch rule matched branch '{}'", request.branch))?;

    if rule.require_pr {
        return Err(anyhow!(
            "branch '{}' is protected and requires a PR merge",
            request.branch
        ));
    }

    let token_id =
        find_token_for_allowed_scope(broker, &request.agent_id, &rule.allowed_push_scopes, now)
            .ok_or_else(|| {
                anyhow!(
                    "agent '{}' lacks push scope for branch '{}'",
                    request.agent_id,
                    request.branch
                )
            })?;

    cleanup_expired_sessions(push_sessions, now);

    let push_session = Uuid::new_v4().to_string();
    let expires_at = now + PUSH_SESSION_TTL_SECS;
    push_sessions.insert(
        push_session.clone(),
        PushSession {
            token_id,
            agent_id: request.agent_id.clone(),
            repo_id: request.repo_id.clone(),
            thread_id: request.thread_id.clone(),
            expires_at,
        },
    );

    Ok(PushSessionResponse {
        push_session,
        expires_at,
    })
}

fn branch_execution_work_item_id(repo_id: &str, branch: &str) -> String {
    stable_swarm_id("swarm-branch-execution-work-item", &[repo_id, branch])
}

fn branch_artifact_work_item_id(repo_id: &str, branch: &str) -> String {
    stable_swarm_id("swarm-branch-artifact-work-item", &[repo_id, branch])
}

fn branch_claim_id(repo_id: &str, branch: &str, agent_id: &str) -> String {
    stable_swarm_id("swarm-branch-claim", &[repo_id, branch, agent_id])
}

fn stable_swarm_id(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    let digest = hex::encode(hasher.finalize());
    format!("{prefix}-{}", &digest[..16])
}

fn find_token_for_allowed_scope(
    broker: &mut AccessBroker,
    agent_id: &str,
    allowed_scopes: &[String],
    now: u64,
) -> Option<String> {
    let token_ids: Vec<String> = broker
        .tokens()
        .into_iter()
        .filter(|token| token.subject == agent_id)
        .map(|token| token.token_id)
        .collect();

    for token_id in token_ids {
        for scope in allowed_scopes {
            if broker
                .evaluate(
                    &token_id,
                    &AccessRequest {
                        subject: agent_id.to_string(),
                        required_level: required_trust_for_scope(scope),
                        scope: scope.to_string(),
                        goal_scope: None,
                    },
                    now,
                )
                .is_ok()
            {
                return Some(token_id.clone());
            }
        }
    }

    None
}

fn cleanup_expired_sessions(push_sessions: &mut HashMap<String, PushSession>, now: u64) {
    push_sessions.retain(|_, session| session.expires_at > now);
}

fn required_trust_for_scope(scope: &str) -> AgentTrustLevel {
    match scope {
        "git.read" => AgentTrustLevel::ReadOnly,
        "git.push" | "pr.create" | "pr.review" | "check.report" => AgentTrustLevel::ArchiveWrite,
        "git.push:protected" | "pr.merge" => AgentTrustLevel::ExternalAct,
        _ => AgentTrustLevel::ExternalAct,
    }
}

fn deny_push(reason: impl Into<String>) -> PushAuthResponse {
    PushAuthResponse {
        allowed: false,
        reason: Some(reason.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap as StdHashMap;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;

    use super::*;
    use symbiotic_control_plane::{ManagementStore, WorkItemStatus};
    use symbiotic_git_swarm::types::{BranchRule, MergeRuleSet, SwarmRepoStatus};
    use symbiotic_trust::CapabilityToken;
    use symbiotic_vm::backend::VmBackend;
    use symbiotic_vm::file_bridge::FileBridge;
    use symbiotic_vm::manager::VmManager;
    use symbiotic_vm::types::{ExecResult, FileTransfer, VmInstance, VmState};

    type DistilleryFileMap = StdHashMap<(String, String), Vec<u8>>;

    #[derive(Default, Clone)]
    struct DistilleryArtifactBackend {
        states: Arc<Mutex<StdHashMap<String, VmState>>>,
        instances: Arc<Mutex<StdHashMap<String, VmInstance>>>,
        files: Arc<Mutex<DistilleryFileMap>>,
    }

    #[async_trait]
    impl VmBackend for DistilleryArtifactBackend {
        async fn create(
            &self,
            id: &str,
            request: &symbiotic_vm::types::VmCreateRequest,
        ) -> Result<VmInstance> {
            let instance = VmInstance {
                id: id.to_string(),
                image: request.image.clone(),
                state: VmState::Creating,
                resources: request.resources.clone(),
                network: request.network.clone(),
                requesting_agent: request.requesting_agent.clone(),
                purpose: request.purpose.clone(),
                created_at: now_unix(),
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
            self.states
                .lock()
                .expect("lock")
                .insert(id.to_string(), VmState::Running);
            Ok(())
        }

        async fn exec(&self, id: &str, command: &str) -> Result<ExecResult> {
            let state = self
                .states
                .lock()
                .expect("lock")
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("VM not found: {id}"))?;
            if state != VmState::Running {
                return Err(anyhow!("VM {id} is not running"));
            }

            let mut files = self.files.lock().expect("lock");
            files.insert(
                (
                    id.to_string(),
                    format!("{DISTILLERY_OUTPUT_VM_ROOT}/patterns/typed-artifact.md"),
                ),
                "# Typed Distillery Artifact\n\nEmit a typed distillery bundle artifact.\n"
                    .as_bytes()
                    .to_vec(),
            );
            files.insert(
                (id.to_string(), DISTILLERY_BUNDLE_VM_PATH.to_string()),
                serde_json::json!({
                    "version": 1,
                    "report": {
                        "summary_markdown": format!("Integrated distillery summary from {id}."),
                        "decisions": ["Keep the daemon as the Archive writer."],
                        "patterns": ["Emit a typed distillery bundle artifact."],
                        "lint_status": "clean"
                    },
                    "artifacts": [{
                        "kind": "pattern_note",
                        "title": "Typed Distillery Artifact",
                        "relative_path": "patterns/typed-artifact.md"
                    }]
                })
                .to_string()
                .into_bytes(),
            );

            Ok(ExecResult {
                exit_code: 0,
                stdout: format!("[distillery-backend] executed: {command}"),
                stderr: String::new(),
            })
        }

        async fn stop(&self, id: &str) -> Result<()> {
            self.states
                .lock()
                .expect("lock")
                .insert(id.to_string(), VmState::Stopped);
            Ok(())
        }

        async fn destroy(&self, id: &str) -> Result<()> {
            self.states.lock().expect("lock").remove(id);
            self.instances.lock().expect("lock").remove(id);
            self.files
                .lock()
                .expect("lock")
                .retain(|(vm_id, _), _| vm_id != id);
            Ok(())
        }

        async fn transfer(&self, id: &str, transfer: &FileTransfer) -> Result<()> {
            let state = self
                .states
                .lock()
                .expect("lock")
                .get(id)
                .cloned()
                .ok_or_else(|| anyhow!("VM not found: {id}"))?;
            if state != VmState::Running {
                return Err(anyhow!("VM {id} must be running for file transfer"));
            }

            match transfer.direction {
                symbiotic_vm::types::TransferDirection::HostToVm => {
                    let bytes = std::fs::read(&transfer.host_path)?;
                    self.files
                        .lock()
                        .expect("lock")
                        .insert((id.to_string(), transfer.vm_path.clone()), bytes);
                }
                symbiotic_vm::types::TransferDirection::VmToHost => {
                    let bytes = self
                        .files
                        .lock()
                        .expect("lock")
                        .get(&(id.to_string(), transfer.vm_path.clone()))
                        .cloned()
                        .ok_or_else(|| anyhow!("VM file missing: {}", transfer.vm_path))?;
                    if let Some(parent) = std::path::Path::new(&transfer.host_path).parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&transfer.host_path, bytes)?;
                }
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

    fn repo_with_rules(branch_rules: Vec<BranchRule>) -> SwarmRepo {
        SwarmRepo {
            id: "repo-1".to_string(),
            container_path: "/repos/repo-1.git".to_string(),
            created_at: Utc::now(),
            status: SwarmRepoStatus::Active,
            branch_rules,
            default_merge_rules: MergeRuleSet::default(),
        }
    }

    fn token(
        agent_id: &str,
        token_id: &str,
        scope: &str,
        trust: AgentTrustLevel,
    ) -> CapabilityToken {
        CapabilityToken {
            token_id: token_id.to_string(),
            subject: agent_id.to_string(),
            trust_level: trust,
            scopes: HashSet::from([scope.to_string()]),
            expires_at: 10_000,
            one_time: false,
            consumed: false,
            goal_scope: None,
        }
    }

    fn push_session_request(branch: &str) -> PushSessionRequest {
        PushSessionRequest {
            agent_id: "agent-1".to_string(),
            repo_id: "repo-1".to_string(),
            branch: branch.to_string(),
            goal_scope: None,
            thread_id: None,
        }
    }

    fn push_auth_request(branch: &str, push_session: &str) -> PushAuthRequest {
        PushAuthRequest {
            repo_id: "repo-1".to_string(),
            branch: branch.to_string(),
            old_sha: "0000000000000000000000000000000000000000".to_string(),
            new_sha: "1111111111111111111111111111111111111111".to_string(),
            push_session: push_session.to_string(),
        }
    }

    fn test_swarm_server(tmp: &tempfile::TempDir) -> SwarmServer {
        SwarmServer::new(
            Arc::new(StdMutex::new(AccessBroker::new())),
            Arc::new(StdMutex::new(ManagementStore::new(
                tmp.path().join("control-plane"),
            ))),
            None,
            None,
            tmp.path().join("archive"),
            tmp.path().join("data"),
            None,
        )
        .expect("swarm server should initialize")
    }

    fn test_pr(required_checks: Vec<String>) -> SwarmPR {
        SwarmPR {
            id: "pr-1".to_string(),
            repo_id: "repo-1".to_string(),
            branch: "feature/agent-1".to_string(),
            base: "main".to_string(),
            title: "Test PR".to_string(),
            description: "description".to_string(),
            author_agent: "agent-1".to_string(),
            status: PRStatus::Open,
            reviews: vec![],
            checks: vec![],
            merge_rules: MergeRuleSet {
                required_approvals: 1,
                required_checks,
                dismiss_stale_reviews: true,
                allowed_merge_agents: vec!["*".to_string()],
            },
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn authorize_push_allows_feature_branch_with_git_push_scope() {
        let repo = repo_with_rules(vec![
            BranchRule::protected_main(),
            BranchRule::standard_push("*"),
        ]);
        let mut broker = AccessBroker::new();
        let mut push_sessions = HashMap::new();
        broker.issue_token(token(
            "agent-1",
            "token-1",
            "git.push",
            AgentTrustLevel::ArchiveWrite,
        ));

        let session = issue_push_session_for_repo(
            Some(&repo),
            &mut broker,
            &mut push_sessions,
            &push_session_request("feature/agent-1"),
            1_000,
        )
        .expect("push session should issue");

        let response = authorize_push_for_repo(
            Some(&repo),
            &mut broker,
            &mut push_sessions,
            &push_auth_request("feature/agent-1", &session.push_session),
            1_000,
            None,
        );

        assert!(response.allowed);
        assert_eq!(response.reason, None);
    }

    #[test]
    fn sync_branch_ownership_creates_running_management_claim() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        let claim_id = branch_claim_id(&request.repo_id, &request.branch, &request.agent_id);

        assert_eq!(store.work_item_count(), 2);
        assert_eq!(store.claim_count(), 1);
        assert_eq!(store.active_claim_count(), 1);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::Running
        );
        assert_eq!(
            store
                .get_claim(&claim_id)
                .expect("claim")
                .lease
                .last_heartbeat_at,
            1_000
        );
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .status,
            WorkItemStatus::Running
        );
    }

    #[test]
    fn sync_branch_ownership_persists_explicit_thread_attachment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let mut request = push_session_request("feature/agent-1");
        request.thread_id = Some("thread-build-runtime".to_string());

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .thread_id
                .as_deref(),
            Some("thread-build-runtime")
        );
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .thread_id
                .as_deref(),
            Some("thread-build-runtime")
        );
    }

    #[test]
    fn goal_scoped_branch_ownership_creates_development_task_parent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let mut request = push_session_request("feature/agent-1");
        request.goal_scope = Some("build-runtime".to_string());
        request.thread_id = Some("thread-build-runtime".to_string());

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");

        let store = server.management_store.lock().expect("lock");
        let task_id = goal_task_work_item_id("build-runtime", "development:repo-1");
        let execution_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);

        assert_eq!(store.work_item_count(), 3);
        assert_eq!(
            store
                .get_work_item(&task_id)
                .expect("task work item")
                .parent_work_item_id
                .as_deref(),
            Some("goal:build-runtime")
        );
        assert_eq!(
            store
                .get_work_item(&execution_id)
                .expect("execution work item")
                .parent_work_item_id
                .as_deref(),
            Some(task_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .parent_work_item_id
                .as_deref(),
            Some(execution_id.as_str())
        );
        assert_eq!(
            store
                .get_work_item(&task_id)
                .expect("task work item")
                .status,
            WorkItemStatus::Running
        );
    }

    #[test]
    fn update_branch_work_item_status_transitions_existing_work_item() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .update_branch_work_item_status(
                &request.repo_id,
                &request.branch,
                WorkItemStatus::PendingReview,
                1_010,
            )
            .expect("status update should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::PendingReview
        );
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .status,
            WorkItemStatus::PendingReview
        );
    }

    #[test]
    fn complete_branch_ownership_revokes_claims_and_marks_done() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .complete_branch_ownership(&request.repo_id, &request.branch, 1_020)
            .expect("completion should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        let claim_id = branch_claim_id(&request.repo_id, &request.branch, &request.agent_id);

        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::Done
        );
        assert_eq!(
            store.get_claim(&claim_id).expect("claim").status,
            ScopeClaimStatus::Revoked
        );
        assert_eq!(store.active_claim_count(), 0);
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .status,
            WorkItemStatus::Done
        );
    }

    #[test]
    fn parent_development_task_tracks_branch_review_and_completion() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let mut request = push_session_request("feature/agent-1");
        request.goal_scope = Some("build-runtime".to_string());

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .update_branch_work_item_status(
                &request.repo_id,
                &request.branch,
                WorkItemStatus::PendingReview,
                1_010,
            )
            .expect("status update should succeed");

        let task_id = goal_task_work_item_id("build-runtime", "development:repo-1");
        {
            let store = server.management_store.lock().expect("lock");
            assert_eq!(
                store
                    .get_work_item(&task_id)
                    .expect("task work item")
                    .status,
                WorkItemStatus::PendingReview
            );
        }

        server
            .complete_branch_ownership(&request.repo_id, &request.branch, 1_020)
            .expect("completion should succeed");

        let store = server.management_store.lock().expect("lock");
        assert_eq!(
            store
                .get_work_item(&task_id)
                .expect("task work item")
                .status,
            WorkItemStatus::Done
        );
    }

    #[test]
    fn authorize_push_derives_branch_ownership_request_from_session() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let mut push_sessions = HashMap::new();
        push_sessions.insert(
            "push-session".to_string(),
            PushSession {
                token_id: "token-1".to_string(),
                agent_id: "agent-1".to_string(),
                repo_id: "repo-1".to_string(),
                thread_id: Some("thread-1".to_string()),
                expires_at: 10_000,
            },
        );

        let ownership_request = server
            .authorized_push_ownership_request(
                &push_auth_request("feature/agent-1", "push-session"),
                &push_sessions,
                true,
            )
            .expect("ownership request");

        assert_eq!(ownership_request.agent_id, "agent-1");
        assert_eq!(ownership_request.repo_id, "repo-1");
        assert_eq!(ownership_request.branch, "feature/agent-1");
        assert_eq!(ownership_request.thread_id.as_deref(), Some("thread-1"));
    }

    #[tokio::test]
    async fn request_changes_returns_branch_work_item_to_running() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .update_branch_work_item_status(
                &request.repo_id,
                &request.branch,
                WorkItemStatus::PendingReview,
                1_010,
            )
            .expect("status update should succeed");

        let pr_id = {
            let mut pr_mgr = server.pr_manager.lock().await;
            pr_mgr
                .create_pr(
                    &request.repo_id,
                    &request.branch,
                    "main",
                    "Review me",
                    "",
                    &request.agent_id,
                    MergeRuleSet::default(),
                )
                .expect("pr should create")
                .id
        };

        server
            .rpc_pr_review(
                "pr.request_changes",
                serde_json::json!({
                    "pr_id": pr_id,
                    "agent_id": "reviewer-1",
                    "comments": [{
                        "file": "src/lib.rs",
                        "line": 1,
                        "body": "please revise"
                    }]
                }),
            )
            .await
            .expect("request changes should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::Running
        );
    }

    #[tokio::test]
    async fn failed_check_returns_branch_work_item_to_running() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .update_branch_work_item_status(
                &request.repo_id,
                &request.branch,
                WorkItemStatus::PendingReview,
                1_010,
            )
            .expect("status update should succeed");

        let pr_id = {
            let mut pr_mgr = server.pr_manager.lock().await;
            pr_mgr
                .create_pr(
                    &request.repo_id,
                    &request.branch,
                    "main",
                    "Checks pending",
                    "",
                    &request.agent_id,
                    MergeRuleSet {
                        required_checks: vec!["ci".to_string()],
                        ..MergeRuleSet::default()
                    },
                )
                .expect("pr should create")
                .id
        };

        server
            .rpc_check_report(serde_json::json!({
                "pr_id": pr_id,
                "check_name": "ci",
                "status": "failure",
                "agent_id": "ci-agent",
                "output": "tests failed"
            }))
            .await
            .expect("check report should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::Running
        );
    }

    #[tokio::test]
    async fn close_pr_revokes_claims_and_marks_branch_cancelled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let server = test_swarm_server(&tmp);
        let request = push_session_request("feature/agent-1");

        server
            .sync_branch_ownership(&request, 1_000)
            .expect("branch ownership sync should succeed");
        server
            .update_branch_work_item_status(
                &request.repo_id,
                &request.branch,
                WorkItemStatus::PendingReview,
                1_010,
            )
            .expect("status update should succeed");

        let pr_id = {
            let mut pr_mgr = server.pr_manager.lock().await;
            pr_mgr
                .create_pr(
                    &request.repo_id,
                    &request.branch,
                    "main",
                    "Will close",
                    "",
                    &request.agent_id,
                    MergeRuleSet::default(),
                )
                .expect("pr should create")
                .id
        };

        server
            .rpc_pr_close(serde_json::json!({ "pr_id": pr_id }))
            .await
            .expect("close should succeed");

        let store = server.management_store.lock().expect("lock");
        let work_item_id = branch_execution_work_item_id(&request.repo_id, &request.branch);
        let artifact_id = branch_artifact_work_item_id(&request.repo_id, &request.branch);
        let claim_id = branch_claim_id(&request.repo_id, &request.branch, &request.agent_id);
        assert_eq!(
            store
                .get_work_item(&work_item_id)
                .expect("work item")
                .status,
            WorkItemStatus::Cancelled
        );
        assert_eq!(
            store.get_claim(&claim_id).expect("claim").status,
            ScopeClaimStatus::Revoked
        );
        assert_eq!(store.active_claim_count(), 0);
        assert_eq!(
            store
                .get_work_item(&artifact_id)
                .expect("artifact work item")
                .status,
            WorkItemStatus::Cancelled
        );
    }

    #[test]
    fn authorize_push_denies_protected_branch_even_with_scope() {
        let repo = repo_with_rules(vec![
            BranchRule::protected_main(),
            BranchRule::standard_push("*"),
        ]);
        let mut broker = AccessBroker::new();
        let mut push_sessions = HashMap::new();
        broker.issue_token(token(
            "agent-1",
            "token-1",
            "git.push:protected",
            AgentTrustLevel::ExternalAct,
        ));

        let err = issue_push_session_for_repo(
            Some(&repo),
            &mut broker,
            &mut push_sessions,
            &push_session_request("main"),
            1_000,
        )
        .expect_err("protected branch push session should be denied");

        assert_eq!(
            err.to_string(),
            "branch 'main' is protected and requires a PR merge"
        );
    }

    #[test]
    fn authorize_push_denies_when_agent_lacks_matching_scope() {
        let repo = repo_with_rules(vec![BranchRule::standard_push("*")]);
        let mut broker = AccessBroker::new();
        let mut push_sessions = HashMap::new();
        broker.issue_token(token(
            "agent-1",
            "token-1",
            "git.read",
            AgentTrustLevel::ReadOnly,
        ));

        let err = issue_push_session_for_repo(
            Some(&repo),
            &mut broker,
            &mut push_sessions,
            &push_session_request("feature/x"),
            1_000,
        )
        .expect_err("push session should be denied");
        assert_eq!(
            err.to_string(),
            "agent 'agent-1' lacks push scope for branch 'feature/x'"
        );
    }

    #[test]
    fn authorize_push_denies_unknown_repo() {
        let mut broker = AccessBroker::new();
        let mut push_sessions = HashMap::new();
        let response = authorize_push_for_repo(
            None,
            &mut broker,
            &mut push_sessions,
            &push_auth_request("feature/x", "push-session"),
            1_000,
            None,
        );
        assert!(!response.allowed);
        assert_eq!(response.reason.as_deref(), Some("unknown repo: repo-1"));
    }

    #[test]
    fn authorize_push_denies_invalid_push_session() {
        let repo = repo_with_rules(vec![BranchRule::standard_push("*")]);
        let mut broker = AccessBroker::new();
        let mut push_sessions = HashMap::new();

        let response = authorize_push_for_repo(
            Some(&repo),
            &mut broker,
            &mut push_sessions,
            &push_auth_request("feature/x", "missing-session"),
            1_000,
            None,
        );

        assert!(!response.allowed);
        assert_eq!(
            response.reason.as_deref(),
            Some("invalid or expired push session")
        );
    }

    #[test]
    fn parse_required_checks_trims_and_drops_empty_entries() {
        assert_eq!(
            parse_required_checks(" cargo-test, ,vault-linter ,"),
            vec!["cargo-test".to_string(), "vault-linter".to_string()]
        );
    }

    #[test]
    fn append_bridge_runner_command_issues_authenticated_bridge_token() {
        let broker = Arc::new(StdMutex::new(AccessBroker::new()));

        let command = append_bridge_runner_command(
            &broker,
            "swarm-distillery-pr-42",
            "/runner '--goal' 'distill'",
        )
        .expect("command should be built");

        assert!(command.contains("'--gateway-token'"));

        let broker = broker.lock().expect("lock");
        let tokens = broker.tokens();
        assert_eq!(tokens.len(), 1);
        let token = &tokens[0];
        assert_eq!(token.subject, "swarm-distillery-pr-42");
        assert!(token.token_id.starts_with("swarm-bridge-"));
        assert!(token.scopes.contains("bridge.connect"));
        assert!(token.scopes.contains("llm.chat"));
    }

    #[test]
    fn build_ci_job_includes_swarm_env_and_mounts() {
        let runner_binary = std::env::temp_dir().join(format!("swarm-runner-{}", Uuid::new_v4()));
        std::fs::write(&runner_binary, "runner").expect("runner binary fixture");

        let job = build_ci_job(
            "http://172.17.0.1:80/repo-1.git",
            &test_pr(vec!["cargo-test".to_string()]),
            "cargo-test",
            "/tmp/nucleus.sock",
            &runner_binary,
        );
        let request = job.request;

        assert_eq!(request.image, SWARM_AGENT_VM_IMAGE);
        assert_eq!(request.requesting_agent, "swarm-ci-cargo-test");
        assert!(request
            .env
            .contains(&"GIT_SERVER_URL=http://172.17.0.1:80/repo-1.git".to_string()));
        assert!(request.env.contains(&"SWARM_REPO_ID=repo-1".to_string()));
        assert!(request.env.contains(&"PR_ID=pr-1".to_string()));
        assert!(request.env.contains(&"CHECK_NAME=cargo-test".to_string()));
        assert!(request
            .env
            .contains(&"SYMBIOTIC_SOCKET=/tmp/nucleus.sock".to_string()));
        assert!(request
            .env
            .contains(&"CHECK_COMMAND=cargo test".to_string()));
        assert_eq!(request.mounts.len(), 2);
        assert_eq!(request.mounts[0].vm_path, "/tmp/nucleus.sock");
        assert_eq!(request.mounts[1].vm_path, SWARM_RUNNER_PATH);
        assert_eq!(
            request.network.allowed_domains,
            vec!["172.17.0.1".to_string()]
        );
        assert_eq!(request.network.allowed_ports, vec![80]);
    }

    #[test]
    fn dispatch_blockers_require_sandbox_socket_and_runner_binary() {
        let runner_binary = std::env::temp_dir().join(format!("swarm-runner-{}", Uuid::new_v4()));
        let blockers = dispatch_blockers(false, None, &runner_binary);

        assert!(blockers
            .iter()
            .any(|item| item.contains("sandbox manager unavailable")));
        assert!(blockers
            .iter()
            .any(|item| item.contains("SYMBIOTIC_LLM_GATEWAY_SOCKET")));
        assert!(blockers
            .iter()
            .any(|item| item.contains("symbiotic-agent-runner binary missing")));
    }

    #[test]
    fn build_distillery_job_includes_post_merge_env_and_mounts() {
        let runner_binary = std::env::temp_dir().join(format!("swarm-runner-{}", Uuid::new_v4()));
        std::fs::write(&runner_binary, "runner").expect("runner binary fixture");

        let job = build_distillery_job(
            "http://172.17.0.1:80/repo-1.git",
            "repo-1",
            "pr-42",
            "abc123def",
            "main",
            "/tmp/nucleus.sock",
            &runner_binary,
        );
        let request = job.request;

        assert_eq!(request.image, SWARM_AGENT_VM_IMAGE);
        assert!(request.requesting_agent.starts_with("swarm-distillery-"));
        assert!(request.purpose.contains("distillery-post-merge"));
        assert!(request
            .env
            .contains(&"DISTILLERY_MODE=post-merge".to_string()));
        assert!(request.env.contains(&"MERGED_PR_ID=pr-42".to_string()));
        assert!(request.env.contains(&"MERGED_SHA=abc123def".to_string()));
        assert!(request.env.contains(&"SWARM_BASE_BRANCH=main".to_string()));
        assert!(request
            .env
            .contains(&"GIT_SERVER_URL=http://172.17.0.1:80/repo-1.git".to_string()));
        assert!(request.env.contains(&"SWARM_REPO_ID=repo-1".to_string()));
        assert!(request.env.contains(&"PR_ID=pr-42".to_string()));
        assert!(request
            .env
            .contains(&format!("CHECK_NAME={DISTILLERY_CHECK_NAME}")));
        assert!(request
            .env
            .contains(&"SYMBIOTIC_SOCKET=/tmp/nucleus.sock".to_string()));
        assert!(job.command.contains("'--goal'"));
        assert!(job.command.contains("'--system-prompt'"));
        assert!(!job.command.contains("'--distillery'"));

        // At minimum, socket + runner mounts (linter may or may not exist)
        assert!(request.mounts.len() >= 2);
        assert_eq!(request.mounts[0].vm_path, "/tmp/nucleus.sock");
        assert_eq!(request.mounts[1].vm_path, SWARM_RUNNER_PATH);

        // Check_name should be None for distillery
        assert!(job.check_name.is_none());
        assert_eq!(job.pr_id, "pr-42");
    }

    #[test]
    fn distillery_vm_env_contains_required_vars() {
        let env = distillery_vm_env(
            "http://172.17.0.1:80/repo-1.git",
            "repo-1",
            "/tmp/nucleus.sock",
            "pr-42",
            "abc123",
            "main",
        );

        assert!(env.contains(&"DISTILLERY_MODE=post-merge".to_string()));
        assert!(env.contains(&"PR_ID=pr-42".to_string()));
        assert!(env.contains(&format!("CHECK_NAME={DISTILLERY_CHECK_NAME}")));
        assert!(env.contains(&"MERGED_PR_ID=pr-42".to_string()));
        assert!(env.contains(&"MERGED_SHA=abc123".to_string()));
        assert!(env.contains(&"SWARM_BASE_BRANCH=main".to_string()));
        assert_eq!(env.len(), 9);
    }

    #[test]
    fn render_distillery_archive_note_includes_structured_sections() {
        let note = render_distillery_archive_note(
            "pr-42",
            "abc123",
            &DistilleryArchiveReport {
                summary_markdown: "Summary body.".to_string(),
                decisions: vec!["Use the real runner path".to_string()],
                patterns: vec!["Emit a typed bundle manifest".to_string()],
                follow_up_pr_title: Some("fix(distillery): lint follow-up".to_string()),
                lint_status: Some("fix_pr_opened".to_string()),
            },
        );

        assert!(note.contains("source: swarm-distillery"));
        assert!(note.contains("pr_id: pr-42"));
        assert!(note.contains("# Swarm Distillery pr-42"));
        assert!(note.contains("## Decisions"));
        assert!(note.contains("## Patterns"));
        assert!(note.contains("fix(distillery): lint follow-up"));
    }

    #[test]
    fn persist_distillery_archive_bundle_writes_canonical_note_and_artifacts() {
        let temp = tempfile::tempdir().expect("tempdir");
        let archive_root = temp.path().join("archive");
        let vm_output_root = temp.path().join("data/runtime/vm-output");
        let vm_dir = vm_output_root.join("vm-test");
        let artifact_dir = vm_dir.join("artifacts/patterns");
        std::fs::create_dir_all(&vm_dir).expect("vm output dir");
        std::fs::create_dir_all(&artifact_dir).expect("artifact dir");
        std::fs::write(
            vm_dir.join("distillery-bundle.json"),
            serde_json::json!({
                "version": 1,
                "report": {
                    "summary_markdown": "Summary from VM.",
                    "decisions": ["Capture durable knowledge."],
                    "patterns": ["Use a typed artifact bundle."],
                    "lint_status": "clean"
                },
                "artifacts": [{
                    "kind": "pattern_note",
                    "title": "Typed Artifact Pattern",
                    "relative_path": "patterns/typed-artifact.md"
                }]
            })
            .to_string(),
        )
        .expect("write bundle");
        std::fs::write(
            artifact_dir.join("typed-artifact.md"),
            "# Typed Artifact Pattern\n\nPersist extra notes through a manifest.\n",
        )
        .expect("write artifact");

        let path = persist_distillery_archive_bundle(
            &archive_root,
            &vm_output_root,
            "vm-test",
            "pr-42",
            "abc123",
        )
        .expect("persist bundle");

        let written = std::fs::read_to_string(path).expect("read note");
        assert!(written.contains("Summary from VM."));
        assert!(written.contains("Capture durable knowledge."));
        assert!(written.contains("Use a typed artifact bundle."));
        let artifact = std::fs::read_to_string(
            archive_root
                .join("operations")
                .join("swarm-distillery")
                .join("artifacts")
                .join("patterns")
                .join("pr-42-1-Typed-Artifact-Pattern.md"),
        )
        .expect("read persisted artifact");
        assert!(artifact.contains("Persist extra notes through a manifest."));
    }

    #[tokio::test]
    async fn distillery_job_exec_extract_and_persist_flow_is_integrated() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let data_dir = project_root.join("data");
        let archive_root = temp.path().join("archive");
        std::fs::create_dir_all(&project_root).expect("project root");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::create_dir_all(&archive_root).expect("archive root");

        let runner_binary = temp.path().join("symbiotic-agent-runner");
        std::fs::write(&runner_binary, "runner").expect("runner fixture");
        let socket_path = temp.path().join("nucleus.sock");
        std::fs::write(&socket_path, "").expect("socket fixture");

        let backend = Box::new(DistilleryArtifactBackend::default());
        let bridge = FileBridge::new(&project_root, &data_dir);
        let audit_path = data_dir.join("runtime/vm-audit.jsonl");
        let manager = Arc::new(StdMutex::new(VmManager::new(backend, &audit_path, bridge)));

        let job = build_distillery_job(
            "http://172.17.0.1:80/repo-1.git",
            "repo-1",
            "pr-42",
            "abc123def",
            "main",
            &socket_path.to_string_lossy(),
            &runner_binary,
        );
        let vm_id = create_and_start_vm(Arc::clone(&manager), job.request.clone())
            .await
            .expect("vm should start");

        exec_extract_distillery_bundle_and_destroy_vm(
            Arc::clone(&manager),
            vm_id.clone(),
            job.request.requesting_agent.clone(),
            job.command.clone(),
            "pr-42".to_string(),
            "abc123def".to_string(),
            archive_root.clone(),
            data_dir.join("runtime").join("vm-output"),
        )
        .await
        .expect("distillery flow should succeed");

        let note = std::fs::read_to_string(
            archive_root
                .join("operations")
                .join("swarm-distillery")
                .join("pr-42.md"),
        )
        .expect("archive note should exist");
        assert!(note.contains("Integrated distillery summary"));
        assert!(note.contains("Keep the daemon as the Archive writer."));
        assert!(note.contains("Emit a typed distillery bundle artifact."));
        let artifact = std::fs::read_to_string(
            archive_root
                .join("operations")
                .join("swarm-distillery")
                .join("artifacts")
                .join("patterns")
                .join("pr-42-1-Typed-Distillery-Artifact.md"),
        )
        .expect("artifact note should exist");
        assert!(artifact.contains("Emit a typed distillery bundle artifact."));
        assert!(
            manager.lock().expect("lock").get_instance(&vm_id).is_none(),
            "vm should be destroyed after flow"
        );
    }

    #[tokio::test]
    #[ignore = "requires local Docker runtime and symbiotic-agent-v1:latest"]
    async fn distillery_job_exec_extract_and_persist_flow_runs_on_docker_backend() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let data_dir = project_root.join("data");
        let archive_root = temp.path().join("archive");
        std::fs::create_dir_all(&project_root).expect("project root");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::create_dir_all(&archive_root).expect("archive root");

        let runner_binary = temp.path().join("symbiotic-agent-runner");
        std::fs::write(
            &runner_binary,
            "#!/bin/sh\nwhile true; do sleep 3600; done\n",
        )
        .expect("runner fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runner_binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod runner fixture");
        }
        let socket_path = temp.path().join("nucleus.sock");
        std::fs::write(&socket_path, "").expect("socket fixture");

        let backend = Box::new(
            symbiotic_vm::backends::sysbox::SysboxBackend::with_runtime(false)
                .expect("docker backend"),
        );
        let bridge = FileBridge::new(&project_root, &data_dir);
        let audit_path = data_dir.join("runtime/vm-audit.jsonl");
        let manager = Arc::new(StdMutex::new(VmManager::new(backend, &audit_path, bridge)));

        let job = build_distillery_job(
            "http://172.17.0.1:80/repo-1.git",
            "repo-1",
            "pr-42",
            "abc123def",
            "main",
            &socket_path.to_string_lossy(),
            &runner_binary,
        );
        let vm_id = create_and_start_vm(Arc::clone(&manager), job.request.clone())
            .await
            .expect("docker vm should start");

        let command = r#"mkdir -p /workspace/output/patterns && cat > /workspace/output/patterns/docker-artifact.md <<'EOF1'
# Docker Artifact

Persist Archive notes on the trusted side.
EOF1
cat > /workspace/distillery-bundle.json <<'EOF2'
{"version":1,"report":{"summary_markdown":"Docker distillery summary.","decisions":["Use Docker-backed extract verification."],"patterns":["Persist Archive notes on the trusted side."],"lint_status":"clean"},"artifacts":[{"kind":"pattern_note","title":"Docker Artifact","relative_path":"patterns/docker-artifact.md"}]}
EOF2"#;

        exec_extract_distillery_bundle_and_destroy_vm(
            Arc::clone(&manager),
            vm_id.clone(),
            job.request.requesting_agent.clone(),
            command.to_string(),
            "pr-42".to_string(),
            "abc123def".to_string(),
            archive_root.clone(),
            data_dir.join("runtime").join("vm-output"),
        )
        .await
        .expect("docker distillery flow should succeed");

        let note = std::fs::read_to_string(
            archive_root
                .join("operations")
                .join("swarm-distillery")
                .join("pr-42.md"),
        )
        .expect("archive note should exist");
        assert!(note.contains("Docker distillery summary."));
        assert!(note.contains("Use Docker-backed extract verification."));
        assert!(note.contains("Persist Archive notes on the trusted side."));
        assert!(
            manager.lock().expect("lock").get_instance(&vm_id).is_none(),
            "vm should be destroyed after docker-backed flow"
        );
    }

    #[tokio::test]
    #[ignore = "requires local sysbox-runc runtime and symbiotic-agent-v1:latest"]
    async fn distillery_job_exec_extract_and_persist_flow_runs_on_sysbox_backend() {
        if !symbiotic_vm::backends::sysbox::SysboxBackend::sysbox_runtime_available()
            .await
            .expect("probe sysbox runtime")
        {
            eprintln!("skipping: sysbox-runc runtime is not configured on this Docker host");
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let project_root = temp.path().join("project");
        let data_dir = project_root.join("data");
        let archive_root = temp.path().join("archive");
        std::fs::create_dir_all(&project_root).expect("project root");
        std::fs::create_dir_all(&data_dir).expect("data dir");
        std::fs::create_dir_all(&archive_root).expect("archive root");

        let runner_binary = temp.path().join("symbiotic-agent-runner");
        std::fs::write(
            &runner_binary,
            "#!/bin/sh\nwhile true; do sleep 3600; done\n",
        )
        .expect("runner fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&runner_binary, std::fs::Permissions::from_mode(0o755))
                .expect("chmod runner fixture");
        }
        let socket_path = temp.path().join("nucleus.sock");
        std::fs::write(&socket_path, "").expect("socket fixture");

        let backend =
            Box::new(symbiotic_vm::backends::sysbox::SysboxBackend::new().expect("sysbox backend"));
        let bridge = FileBridge::new(&project_root, &data_dir);
        let audit_path = data_dir.join("runtime/vm-audit.jsonl");
        let manager = Arc::new(StdMutex::new(VmManager::new(backend, &audit_path, bridge)));

        let job = build_distillery_job(
            "http://172.17.0.1:80/repo-1.git",
            "repo-1",
            "pr-42",
            "abc123def",
            "main",
            &socket_path.to_string_lossy(),
            &runner_binary,
        );
        let vm_id = create_and_start_vm(Arc::clone(&manager), job.request.clone())
            .await
            .expect("sysbox vm should start");

        let command = r#"mkdir -p /workspace/output/patterns && cat > /workspace/output/patterns/sysbox-artifact.md <<'EOF1'
# Sysbox Artifact

Persist Archive notes on the trusted side with the configured runtime.
EOF1
cat > /workspace/distillery-bundle.json <<'EOF2'
{"version":1,"report":{"summary_markdown":"Sysbox distillery summary.","decisions":["Require an actual Sysbox runtime for the final VM proof."],"patterns":["Persist Archive notes on the trusted side with the configured runtime."],"lint_status":"clean"},"artifacts":[{"kind":"pattern_note","title":"Sysbox Artifact","relative_path":"patterns/sysbox-artifact.md"}]}
EOF2"#;

        exec_extract_distillery_bundle_and_destroy_vm(
            Arc::clone(&manager),
            vm_id.clone(),
            job.request.requesting_agent.clone(),
            command.to_string(),
            "pr-42".to_string(),
            "abc123def".to_string(),
            archive_root.clone(),
            data_dir.join("runtime").join("vm-output"),
        )
        .await
        .expect("sysbox distillery flow should succeed");

        let note = std::fs::read_to_string(
            archive_root
                .join("operations")
                .join("swarm-distillery")
                .join("pr-42.md"),
        )
        .expect("archive note should exist");
        assert!(note.contains("Sysbox distillery summary."));
        assert!(note.contains("Require an actual Sysbox runtime for the final VM proof."));
        assert!(
            note.contains("Persist Archive notes on the trusted side with the configured runtime.")
        );
        assert!(
            manager.lock().expect("lock").get_instance(&vm_id).is_none(),
            "vm should be destroyed after sysbox-backed flow"
        );
    }

    #[test]
    fn auto_merge_outcome_default_has_no_distillery_vm_id() {
        let outcome = AutoMergeOutcome::default();
        assert!(outcome.distillery_vm_id.is_none());
        assert!(outcome.merged_sha.is_none());
        assert!(outcome.warning.is_none());
    }

    // ── T126 §08.a: durable-repo protected-branch gate ─────────────────
    //
    // `authorize_push_for_repo` now accepts an optional `RepoManifest` and
    // enforces the `source.protected_branches` list as a second gate on top
    // of the ephemeral per-repo rules. These tests cover the four corners:
    // (a) no registry, (b) protected + scope allowed, (c) protected + no
    // scope denied, (d) same-repo unprotected branch still allowed.
    mod durable_branch_protection_tests {
        use super::*;
        use std::path::PathBuf;
        use symbiotic_control_plane::{
            CredentialScope, MirrorDirection, RepoAgentScopes, RepoCheckoutPolicy,
            RepoCredentialBinding, RepoHooks, RepoManifest, RepoMetadata, RepoMirrorPolicy,
            RepoProvider, RepoRole, RepoSource, RepoState,
        };

        /// Build a minimal source-role `RepoManifest` whose `repo_id` matches
        /// the id used by the rest of the swarm_server test fixtures (the
        /// existing `repo_with_rules` helper uses the plain id `repo-1`,
        /// which doesn't carry the `repo:` prefix that real manifests would
        /// have — but `RepoRegistry::get` keys purely by the request's
        /// `repo_id`, so using it directly here exercises the same path).
        /// `protected_branches` is threaded directly so each test can pick
        /// its own enforcement surface.
        fn build_test_manifest(
            id: &str,
            project_id: &str,
            protected_branches: Vec<String>,
        ) -> RepoManifest {
            let slug = id.strip_prefix("repo:").unwrap_or(id);
            RepoManifest {
                id: id.to_string(),
                project_id: project_id.to_string(),
                slug: slug.to_string(),
                title: slug.to_string(),
                state: RepoState::Active,
                repo_role: RepoRole::Source,
                source: RepoSource {
                    url: format!("git@github.com:example/{slug}.git"),
                    provider: RepoProvider::Github,
                    default_branch: "main".to_string(),
                    protected_branches,
                    pinned_head: None,
                },
                credential: RepoCredentialBinding {
                    id: format!("credential:example-{slug}-push"),
                    scope: CredentialScope::Push,
                    trust_floor: AgentTrustLevel::CredentialAccess,
                },
                mirror: RepoMirrorPolicy {
                    internal_bare_path: PathBuf::from(format!("data/git-server/repos/{slug}.git")),
                    direction: MirrorDirection::Bidirectional,
                    sync_interval_secs: 300,
                    last_pulled_at: None,
                    last_pushed_at: None,
                },
                checkout: RepoCheckoutPolicy {
                    worktree_root: PathBuf::from(format!("data/worktrees/{slug}/")),
                    agent_branch_prefix: "agent/".to_string(),
                    max_concurrent_worktrees: 4,
                    cleanup_on_goal_close: true,
                },
                agent_scopes: RepoAgentScopes {
                    read: vec!["archive.read".to_string(), "file.read".to_string()],
                    write: vec![
                        "archive.read".to_string(),
                        "archive.write".to_string(),
                        "file.read".to_string(),
                        "file.write".to_string(),
                    ],
                    push_external: false,
                    requires_operator_approval_for: vec![],
                },
                hooks: RepoHooks {
                    on_attach: None,
                    on_drift_detected: None,
                    on_detach: None,
                },
                indexing: None,
                metadata: RepoMetadata {
                    attached_at: "2026-04-17T00:00:00Z".to_string(),
                    attached_by: "operator".to_string(),
                    notes: String::new(),
                },
                archeology_policy: None,
                body_markdown: String::new(),
            }
        }

        /// Build a `CapabilityToken` carrying one or more scopes. The in-file
        /// `token` helper only supports a single scope, which isn't enough to
        /// both satisfy the ephemeral `standard_push` rule (`git.push`) AND
        /// trip the durable gate's `git.push_external` evaluation on the same
        /// session token.
        fn token_with_scopes(
            agent_id: &str,
            token_id: &str,
            scopes: &[&str],
            trust: AgentTrustLevel,
        ) -> CapabilityToken {
            CapabilityToken {
                token_id: token_id.to_string(),
                subject: agent_id.to_string(),
                trust_level: trust,
                scopes: scopes.iter().map(|s| s.to_string()).collect(),
                expires_at: 10_000,
                one_time: false,
                consumed: false,
                goal_scope: None,
            }
        }

        /// Bootstrap a swarm repo, broker, and push session for `branch`
        /// using a token that carries the given `scopes` set. The session is
        /// pinned to this token, so any later `broker.evaluate` call against
        /// the session's token id will hit exactly this scope set.
        fn fixture_for_branch(
            branch: &str,
            token_id: &str,
            scopes: &[&str],
            trust: AgentTrustLevel,
        ) -> (
            SwarmRepo,
            AccessBroker,
            HashMap<String, PushSession>,
            String,
        ) {
            let repo = repo_with_rules(vec![
                BranchRule::protected_main(),
                BranchRule::standard_push("*"),
            ]);
            let mut broker = AccessBroker::new();
            let mut push_sessions = HashMap::new();
            broker.issue_token(token_with_scopes("agent-1", token_id, scopes, trust));

            let session = issue_push_session_for_repo(
                Some(&repo),
                &mut broker,
                &mut push_sessions,
                &push_session_request(branch),
                1_000,
            )
            .expect("push session should issue");

            (repo, broker, push_sessions, session.push_session)
        }

        // (1) No registry → new gate is a no-op; existing-behavior allowed.
        #[test]
        fn authorize_push_allows_unprotected_branch_without_registry() {
            let (repo, mut broker, mut push_sessions, push_session) = fixture_for_branch(
                "agent/feature",
                "token-1",
                &["git.push"],
                AgentTrustLevel::ArchiveWrite,
            );

            let response = authorize_push_for_repo(
                Some(&repo),
                &mut broker,
                &mut push_sessions,
                &push_auth_request("agent/feature", &push_session),
                1_000,
                None,
            );

            assert!(response.allowed, "no registry → existing behavior");
            assert_eq!(response.reason, None);
        }

        // (2) Protected branch + session token carries both `git.push`
        //     (needed by the ephemeral rule) AND `git.push_external`
        //     (needed by the durable gate) with `ExternalAct` trust →
        //     new gate allows.
        //
        // In production, T126 §05's `issue_repo_token` composes both
        // scopes into a single post-approval token; here we mimic that
        // composition so a single session token satisfies both gates.
        #[test]
        fn authorize_push_allows_protected_branch_with_external_push_scope() {
            let protected_branch = "agent/feature-protected";
            let (repo, mut broker, mut push_sessions, push_session) = fixture_for_branch(
                protected_branch,
                "token-composed",
                &["git.push", "git.push_external"],
                AgentTrustLevel::ExternalAct,
            );

            let manifest =
                build_test_manifest("repo-1", "project:flux", vec![protected_branch.to_string()]);

            let response = authorize_push_for_repo(
                Some(&repo),
                &mut broker,
                &mut push_sessions,
                &push_auth_request(protected_branch, &push_session),
                1_000,
                Some(&manifest),
            );

            assert!(
                response.allowed,
                "protected branch with external push scope → allowed (got {:?})",
                response.reason
            );
            assert_eq!(response.reason, None);
        }

        // (3) Protected branch + session token lacks `git.push_external`
        //     (only carries `git.push`) → durable gate denies with a
        //     reason that cites the branch, the word "protected", and
        //     the manifest id.
        #[test]
        fn authorize_push_denies_protected_branch_without_external_push_scope() {
            let protected_branch = "agent/feature-protected";
            let (repo, mut broker, mut push_sessions, push_session) = fixture_for_branch(
                protected_branch,
                "token-push",
                &["git.push"],
                AgentTrustLevel::ArchiveWrite,
            );

            let manifest =
                build_test_manifest("repo-1", "project:flux", vec![protected_branch.to_string()]);

            let response = authorize_push_for_repo(
                Some(&repo),
                &mut broker,
                &mut push_sessions,
                &push_auth_request(protected_branch, &push_session),
                1_000,
                Some(&manifest),
            );

            assert!(!response.allowed, "no external-push scope → denied");
            let reason = response.reason.expect("deny reason");
            assert!(
                reason.contains("protected"),
                "reason should mention 'protected': {reason}"
            );
            assert!(
                reason.contains(protected_branch),
                "reason should mention branch name: {reason}"
            );
            assert!(
                reason.contains("repo-1"),
                "reason should mention manifest id: {reason}"
            );
        }

        // (4) Manifest protects `main` but request targets an unprotected
        //     `agent/feature-x` branch → new gate is inactive, session's
        //     existing scope is sufficient.
        #[test]
        fn authorize_push_allows_unprotected_branch_on_same_repo_without_scope() {
            let (repo, mut broker, mut push_sessions, push_session) = fixture_for_branch(
                "agent/feature-x",
                "token-1",
                &["git.push"],
                AgentTrustLevel::ArchiveWrite,
            );

            let manifest = build_test_manifest("repo-1", "project:flux", vec!["main".to_string()]);

            let response = authorize_push_for_repo(
                Some(&repo),
                &mut broker,
                &mut push_sessions,
                &push_auth_request("agent/feature-x", &push_session),
                1_000,
                Some(&manifest),
            );

            assert!(
                response.allowed,
                "unprotected branch on same repo → allowed even without external scope (got {:?})",
                response.reason
            );
            assert_eq!(response.reason, None);
        }
    }
}
