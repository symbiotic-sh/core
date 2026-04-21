//! Standalone agent runner for Symbiotic.
//!
//! Executes a ReAct loop in a sandboxed environment, proxying LLM completions
//! and remote tools to the Nucleus (daemon) via a Unix domain socket.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tracing::info;

use symbiotic_core::protocol::{format_tools_for_prompt, ChatMessage, LlmClient, Tool, ToolResult};

pub mod operator_protocol;
pub mod tools;
pub use operator_protocol::{ExecutionCheckpointArtifact, ExecutionContextPacket};
use tools::auth::RequestAuthSessionTool;
use tools::git::{GitCloneTool, GitPushTool};
use tools::metrics::{ToolAffinityTool, ToolStatsTool};
use tools::pr::{
    CheckReportTool, PRApproveTool, PRCloseTool, PRCommentTool, PRCreateTool, PRGetStatusTool,
    PRRequestChangesTool,
};
use tools::workspace::{FileEditTool, FileReadTool, FileWriteTool, ShellExecTool, WorkspaceConfig};

/// Maximum number of iterations before the loop terminates.
const MAX_ITERATIONS: usize = 15;
/// Maximum cumulative size of all messages in bytes.
const MAX_BUFFER_BYTES: usize = 2 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct RunnerArgs {
    /// Path to the Unix domain socket for communicating with the Nucleus.
    #[arg(long, env = "SYMBIOTIC_SOCKET")]
    socket: PathBuf,

    /// Capability token used to authenticate the bridge session.
    #[arg(long, env = "SYMBIOTIC_GATEWAY_TOKEN")]
    gateway_token: String,

    /// User goal to execute.
    #[arg(long, required_unless_present_any = ["ci_check", "review_pr"])]
    goal: Option<String>,

    /// Optional context to prepend to the goal.
    #[arg(long)]
    context: Option<String>,

    /// Optional agent identifier.
    #[arg(long)]
    agent_id: Option<String>,

    /// Workspace root directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,

    /// Role-specific system prompt.
    #[arg(long)]
    system_prompt: Option<String>,

    /// Optional agent role label for truthful runtime observability.
    #[arg(long)]
    role: Option<String>,

    /// Truthful sandbox/runtime type label for observability.
    #[arg(long)]
    sandbox_type: Option<String>,

    /// Truthful model route label currently known by the daemon.
    #[arg(long)]
    model_label: Option<String>,

    /// Optional attached thread identifier for truthful observability.
    #[arg(long)]
    thread_id: Option<String>,

    /// Maximum allowed iterations for the ReAct loop.
    #[arg(long, default_value_t = MAX_ITERATIONS)]
    max_iterations: usize,

    /// Deterministic CI mode: run the named check and report status back to the daemon.
    #[arg(long)]
    ci_check: Option<String>,

    /// Deterministic reviewer mode: inspect the PR diff and submit approve/request-changes.
    #[arg(long, default_value_t = false)]
    review_pr: bool,

    /// Swarm branch to clone/check out for CI or review modes.
    #[arg(long)]
    branch: Option<String>,

    /// Base branch for reviewer mode.
    #[arg(long, default_value = "main")]
    base_branch: String,
}

#[derive(Debug, Clone)]
struct SwarmEnv {
    git_server_url: String,
    repo_id: String,
    pr_id: Option<String>,
    check_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BridgeRuntimeProfile {
    pub role: Option<String>,
    pub sandbox_type: String,
    pub model_label: Option<String>,
    pub max_iterations: Option<u32>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunnerSessionConfig {
    pub goal: Option<String>,
    pub context: Option<String>,
    pub agent_id: Option<String>,
    pub workspace: PathBuf,
    pub system_prompt: Option<String>,
    pub role: Option<String>,
    pub sandbox_type: Option<String>,
    pub model_label: Option<String>,
    pub thread_id: Option<String>,
    pub max_iterations: usize,
    pub ci_check: Option<String>,
    pub review_pr: bool,
    pub branch: Option<String>,
    pub base_branch: String,
}

#[derive(Debug, Clone)]
pub struct CredentialAuthenticateRequest {
    pub target: String,
    pub scopes: Vec<String>,
    pub session_type: String,
    pub purpose: String,
    pub thread_id: Option<String>,
    pub auth_profile: Option<String>,
    pub prefer_existing_session: bool,
    pub require_human_approval: bool,
}

impl From<RunnerArgs> for RunnerSessionConfig {
    fn from(args: RunnerArgs) -> Self {
        Self {
            goal: args.goal,
            context: args.context,
            agent_id: args.agent_id,
            workspace: args.workspace,
            system_prompt: args.system_prompt,
            role: args.role,
            sandbox_type: args.sandbox_type,
            model_label: args.model_label,
            thread_id: args.thread_id,
            max_iterations: args.max_iterations,
            ci_check: args.ci_check,
            review_pr: args.review_pr,
            branch: args.branch,
            base_branch: args.base_branch,
        }
    }
}

pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
}

pub async fn run(args: RunnerArgs) -> Result<Option<ExecutionResult>> {
    let socket_path = args.socket.clone();
    let gateway_token = args.gateway_token.clone();
    let config: RunnerSessionConfig = args.into();
    let agent_id = config
        .agent_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());

    info!(
        agent_id = %agent_id,
        goal = %config.goal.as_deref().unwrap_or("<special-mode>"),
        "Starting agent runner"
    );

    let socket = UnixStream::connect(&socket_path).await.map_err(|e| {
        anyhow!(
            "Failed to connect to Nucleus socket {:?}: {}",
            socket_path,
            e
        )
    })?;
    let runtime_profile = BridgeRuntimeProfile {
        role: config.role.clone(),
        sandbox_type: config
            .sandbox_type
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        model_label: config.model_label.clone(),
        max_iterations: Some(config.max_iterations as u32),
        thread_id: config.thread_id.clone(),
    };
    let bridge = Arc::new(
        BridgeClient::connect(socket, agent_id, gateway_token, Some(runtime_profile)).await?,
    );

    run_with_bridge(config, bridge).await
}

pub async fn run_with_bridge(
    config: RunnerSessionConfig,
    bridge: Arc<BridgeClient>,
) -> Result<Option<ExecutionResult>> {
    let agent_id = config
        .agent_id
        .clone()
        .unwrap_or_else(|| "unknown".to_string());
    let workspace_cfg = Arc::new(WorkspaceConfig {
        root: config.workspace.clone(),
        ..Default::default()
    });
    let swarm_env = load_swarm_env();

    if let Some(check_name) = config.ci_check.as_deref() {
        let swarm = require_swarm_env(&swarm_env, "ci_check")?;
        let branch = config
            .branch
            .as_deref()
            .ok_or_else(|| anyhow!("--branch is required for --ci-check"))?;
        run_ci_check_mode(
            workspace_cfg.clone(),
            bridge.clone(),
            swarm,
            &agent_id,
            check_name,
            branch,
        )
        .await?;
        return Ok(None);
    }

    if config.review_pr {
        let swarm = require_swarm_env(&swarm_env, "review_pr")?;
        let branch = config
            .branch
            .as_deref()
            .ok_or_else(|| anyhow!("--branch is required for --review-pr"))?;
        run_pr_review_mode(
            workspace_cfg.clone(),
            bridge.clone(),
            swarm,
            &agent_id,
            branch,
            &config.base_branch,
        )
        .await?;
        return Ok(None);
    }

    let local_tools: Vec<Box<dyn Tool>> = vec![
        Box::new(ShellExecTool::new(workspace_cfg.clone())),
        Box::new(FileReadTool::new(workspace_cfg.clone())),
        Box::new(FileWriteTool::new(workspace_cfg.clone())),
        Box::new(FileEditTool::new(workspace_cfg.clone())),
    ];

    let goal = config
        .goal
        .as_deref()
        .ok_or_else(|| anyhow!("--goal is required unless using --ci-check or --review-pr"))?;
    let context_packet = bridge
        .load_context_packet(goal, config.context.as_deref())
        .await?;
    let llm = BridgeLlm::new(bridge.clone());

    // Remote tools that will be proxied to the daemon.
    // In a future version, these could be discovered via the bridge.
    let remote_tool_names = vec!["recall", "archive", "queue", "ask_user", "generate_plan"];
    let mut tools: Vec<Box<dyn Tool>> = local_tools;
    tools.push(Box::new(RequestAuthSessionTool::new(bridge.clone())));
    tools.push(Box::new(ToolStatsTool::new(bridge.clone())));
    tools.push(Box::new(ToolAffinityTool::new(bridge.clone())));
    for name in remote_tool_names {
        tools.push(Box::new(RemoteTool::new(name.to_string(), bridge.clone())));
    }

    // Swarm git tools — only registered when running inside a swarm container.
    if let Some(swarm) = swarm_env.as_ref() {
        tools.push(Box::new(GitCloneTool::new(
            workspace_cfg.clone(),
            swarm.git_server_url.clone(),
            swarm.repo_id.clone(),
        )));
        tools.push(Box::new(GitPushTool::new(
            workspace_cfg.clone(),
            swarm.git_server_url.clone(),
            swarm.repo_id.clone(),
            agent_id.clone(),
            context_packet.goal_scope.clone(),
            context_packet.thread_id.clone(),
            bridge.clone(),
        )));
        tools.push(Box::new(PRCreateTool::new(
            bridge.clone(),
            swarm.repo_id.clone(),
            agent_id.clone(),
            context_packet.goal_scope.clone(),
            context_packet.thread_id.clone(),
        )));
        tools.push(Box::new(PRCommentTool::new(
            bridge.clone(),
            agent_id.clone(),
        )));
        tools.push(Box::new(PRApproveTool::new(
            bridge.clone(),
            agent_id.clone(),
        )));
        tools.push(Box::new(PRRequestChangesTool::new(
            bridge.clone(),
            agent_id.clone(),
        )));
        tools.push(Box::new(PRGetStatusTool::new(bridge.clone())));
        tools.push(Box::new(PRCloseTool::new(bridge.clone())));
        if let (Some(pr_id), Some(check_name)) = (swarm.pr_id.clone(), swarm.check_name.clone()) {
            tools.push(Box::new(CheckReportTool::new(
                bridge.clone(),
                pr_id,
                check_name,
                agent_id.clone(),
            )));
        }

        info!(
            repo_id = %swarm.repo_id,
            goal_scope = ?context_packet.goal_scope,
            "Swarm git/PR tools registered"
        );
    }

    let result = run_react_loop(
        goal,
        config.context.as_deref().unwrap_or(""),
        &tools,
        &llm,
        config.system_prompt.as_deref(),
        bridge.clone(),
        &context_packet,
        config.max_iterations,
    )
    .await
    .inspect_err(|error| {
        let bridge = bridge.clone();
        let max_iterations = config.max_iterations;
        let error_text = error.to_string();
        tokio::spawn(async move {
            emit_agent_status(
                &bridge,
                "failed",
                Some(error_text),
                None,
                Some(max_iterations),
                None,
            )
            .await;
        });
    })?;
    bridge
        .create_checkpoint(ExecutionCheckpointArtifact::from_execution(
            &context_packet,
            result.iterations,
            &result.output,
        ))
        .await?;

    info!(iterations = result.iterations, "Agent execution complete");
    Ok(Some(result))
}

fn load_swarm_env() -> Option<SwarmEnv> {
    let git_server_url = std::env::var("GIT_SERVER_URL").ok()?;
    let repo_id = std::env::var("SWARM_REPO_ID").ok()?;
    Some(SwarmEnv {
        git_server_url,
        repo_id,
        pr_id: std::env::var("PR_ID").ok(),
        check_name: std::env::var("CHECK_NAME").ok(),
    })
}

fn require_swarm_env<'a>(swarm_env: &'a Option<SwarmEnv>, mode: &str) -> Result<&'a SwarmEnv> {
    swarm_env
        .as_ref()
        .ok_or_else(|| anyhow!("{mode} requires GIT_SERVER_URL and SWARM_REPO_ID"))
}

async fn run_ci_check_mode(
    workspace_cfg: Arc<WorkspaceConfig>,
    bridge: Arc<BridgeClient>,
    swarm: &SwarmEnv,
    agent_id: &str,
    check_name: &str,
    branch: &str,
) -> Result<()> {
    let pr_id = swarm
        .pr_id
        .clone()
        .ok_or_else(|| anyhow!("--ci-check requires PR_ID in the environment"))?;
    let git_clone = GitCloneTool::new(
        workspace_cfg.clone(),
        swarm.git_server_url.clone(),
        swarm.repo_id.clone(),
    );
    let check_report =
        CheckReportTool::new(bridge, pr_id, check_name.to_string(), agent_id.to_string());

    report_check_status(
        &check_report,
        "running",
        Some(format!(
            "Starting check '{}' on branch {}",
            check_name, branch
        )),
    )
    .await;

    let clone_result = git_clone
        .execute(serde_json::json!({
            "branch": branch,
            "target_dir": "repo"
        }))
        .await?;
    if !clone_result.success {
        report_check_status(&check_report, "failure", Some(clone_result.output.clone())).await;
        return Err(anyhow!("git_clone failed: {}", clone_result.output));
    }

    let check_command = resolve_check_command(check_name)?;
    let repo_dir = workspace_cfg.root.join("repo");
    let output = tokio::process::Command::new("sh")
        .args(["-lc", &check_command])
        .current_dir(&repo_dir)
        .output()
        .await
        .with_context(|| format!("failed to execute CI command for {}", check_name))?;
    let summary = summarize_command_result(&check_command, &output);

    if output.status.success() {
        report_check_status(&check_report, "success", Some(summary)).await;
        Ok(())
    } else {
        report_check_status(&check_report, "failure", Some(summary.clone())).await;
        Err(anyhow!("CI check '{}' failed", check_name))
    }
}

async fn run_pr_review_mode(
    workspace_cfg: Arc<WorkspaceConfig>,
    bridge: Arc<BridgeClient>,
    swarm: &SwarmEnv,
    agent_id: &str,
    branch: &str,
    base_branch: &str,
) -> Result<()> {
    let pr_id = swarm
        .pr_id
        .clone()
        .ok_or_else(|| anyhow!("--review-pr requires PR_ID in the environment"))?;
    let git_clone = GitCloneTool::new(
        workspace_cfg.clone(),
        swarm.git_server_url.clone(),
        swarm.repo_id.clone(),
    );
    let approve = PRApproveTool::new(bridge.clone(), agent_id.to_string());
    let request_changes = PRRequestChangesTool::new(bridge, agent_id.to_string());

    let clone_result = git_clone
        .execute(serde_json::json!({
            "branch": branch,
            "target_dir": "repo"
        }))
        .await?;
    if !clone_result.success {
        return Err(anyhow!("git_clone failed: {}", clone_result.output));
    }

    let repo_dir = workspace_cfg.root.join("repo");
    run_shell("git fetch origin", &repo_dir).await?;

    let diff_check_cmd = format!("git diff --check origin/{}...HEAD", base_branch);
    let diff_stat_cmd = format!("git diff --stat origin/{}...HEAD", base_branch);
    let diff_check = tokio::process::Command::new("sh")
        .args(["-lc", &diff_check_cmd])
        .current_dir(&repo_dir)
        .output()
        .await
        .with_context(|| "failed to run git diff --check")?;
    let diff_stat = tokio::process::Command::new("sh")
        .args(["-lc", &diff_stat_cmd])
        .current_dir(&repo_dir)
        .output()
        .await
        .with_context(|| "failed to run git diff --stat")?;
    let diff_summary = summarize_command_result(&diff_stat_cmd, &diff_stat);

    if diff_check.status.success() {
        approve
            .execute(serde_json::json!({
                "pr_id": pr_id,
                "comment": format!(
                    "Automated reviewer checked {} against {}.\n\n{}",
                    branch, base_branch, diff_summary
                )
            }))
            .await?;
        Ok(())
    } else {
        let review_output = summarize_command_result(&diff_check_cmd, &diff_check);
        request_changes
            .execute(serde_json::json!({
                "pr_id": pr_id,
                "comments": [{
                    "file": "",
                    "body": format!(
                        "Automated reviewer found patch issues while comparing {} against {}.\n\n{}",
                        branch, base_branch, review_output
                    )
                }]
            }))
            .await?;
        Err(anyhow!("review found issues for PR {}", pr_id))
    }
}

async fn run_shell(command: &str, cwd: &std::path::Path) -> Result<()> {
    let output = tokio::process::Command::new("sh")
        .args(["-lc", command])
        .current_dir(cwd)
        .output()
        .await
        .with_context(|| format!("failed to execute command: {}", command))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(summarize_command_result(command, &output)))
    }
}

async fn report_check_status(tool: &CheckReportTool, status: &str, output: Option<String>) {
    let _ = tool
        .execute(serde_json::json!({
            "status": status,
            "output": output,
        }))
        .await;
}

fn resolve_check_command(check_name: &str) -> Result<String> {
    if let Ok(command) = std::env::var("CHECK_COMMAND") {
        if !command.trim().is_empty() {
            return Ok(command);
        }
    }

    let command = match check_name {
        "cargo-test" => "cargo test",
        "cargo-check" => "cargo check",
        "cargo-fmt" => "cargo fmt --check",
        "cargo-clippy" => "cargo clippy --all-targets --all-features -- -D warnings",
        other => {
            return Err(anyhow!(
                "unsupported check '{}'; set CHECK_COMMAND in the environment",
                other
            ));
        }
    };

    Ok(command.to_string())
}

fn summarize_command_result(command: &str, output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!(
        "$ {}\nexit={}\nstdout:\n{}\nstderr:\n{}",
        command,
        output.status.code().unwrap_or(-1),
        trim_large_output(&stdout),
        trim_large_output(&stderr)
    )
}

fn trim_large_output(value: &str) -> String {
    const LIMIT: usize = 4000;
    if value.len() <= LIMIT {
        value.to_string()
    } else {
        format!("{}...\n[truncated {} bytes]", &value[..LIMIT], value.len())
    }
}

// ---------------------------------------------------------------------------
// JSON-RPC Bridge Client
// ---------------------------------------------------------------------------

pub struct BridgeClient {
    stream: tokio::sync::Mutex<UnixStream>,
    agent_id: String,
}

impl BridgeClient {
    pub async fn connect(
        stream: UnixStream,
        agent_id: String,
        token_id: String,
        runtime_profile: Option<BridgeRuntimeProfile>,
    ) -> Result<Self> {
        let client = Self {
            stream: tokio::sync::Mutex::new(stream),
            agent_id,
        };
        client.handshake(token_id, runtime_profile).await?;
        Ok(client)
    }

    pub async fn connect_socket(
        socket_path: &std::path::Path,
        agent_id: String,
        token_id: String,
        runtime_profile: Option<BridgeRuntimeProfile>,
    ) -> Result<Arc<Self>> {
        let socket = UnixStream::connect(socket_path).await.map_err(|e| {
            anyhow!(
                "Failed to connect to Nucleus socket {:?}: {}",
                socket_path,
                e
            )
        })?;
        Ok(Arc::new(
            Self::connect(socket, agent_id, token_id, runtime_profile).await?,
        ))
    }

    async fn handshake(
        &self,
        token_id: String,
        runtime_profile: Option<BridgeRuntimeProfile>,
    ) -> Result<()> {
        let mut params = serde_json::json!({
            "agent_id": self.agent_id,
            "token_id": token_id,
        });
        if let Some(profile) = runtime_profile {
            params["runtime_profile"] = serde_json::json!({
                "role": profile.role,
                "sandbox_type": profile.sandbox_type,
                "model_label": profile.model_label,
                "max_iterations": profile.max_iterations,
                "thread_id": profile.thread_id,
            });
        }
        let _ = self.call("bridge.handshake", params).await?;
        Ok(())
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": 1 // In a more robust implementation, use atomic incrementing IDs
        });

        let mut msg = serde_json::to_vec(&request)?;
        msg.push(b'\n');

        let mut stream = self.stream.lock().await;
        stream.write_all(&msg).await?;

        let mut response_buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            stream.read_exact(&mut byte).await?;
            if byte[0] == b'\n' {
                break;
            }
            response_buf.push(byte[0]);
        }

        let response: serde_json::Value = serde_json::from_slice(&response_buf)?;
        if let Some(error) = response.get("error") {
            return Err(anyhow!("Nucleus returned error: {}", error));
        }

        response
            .get("result")
            .cloned()
            .ok_or_else(|| anyhow!("Nucleus response missing result"))
    }

    pub async fn notify(&self, method: &str, params: serde_json::Value) -> Result<()> {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        let mut msg = serde_json::to_vec(&request)?;
        msg.push(b'\n');

        let mut stream = self.stream.lock().await;
        stream.write_all(&msg).await?;
        Ok(())
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub async fn request_credential(
        &self,
        service: &str,
        scopes: &[&str],
        session_type: &str,
    ) -> Result<serde_json::Value> {
        self.call(
            "credential.request",
            serde_json::json!({
                "service": service,
                "scopes": scopes,
                "session_type": session_type,
            }),
        )
        .await
    }

    pub async fn authenticate_credential(
        &self,
        request: CredentialAuthenticateRequest,
    ) -> Result<serde_json::Value> {
        self.call(
            "credential.authenticate",
            serde_json::json!({
                "agent_id": self.agent_id(),
                "target": request.target,
                "scopes": request.scopes,
                "session_type": request.session_type,
                "purpose": request.purpose,
                "thread_id": request.thread_id,
                "auth_profile": request.auth_profile,
                "prefer_existing_session": request.prefer_existing_session,
                "require_human_approval": request.require_human_approval,
            }),
        )
        .await
    }

    pub async fn tool_stats(
        &self,
        tool_name: &str,
        window_size: usize,
    ) -> Result<serde_json::Value> {
        self.call(
            "tool.stats",
            serde_json::json!({
                "tool_name": tool_name,
                "window_size": window_size,
            }),
        )
        .await
    }

    pub async fn tool_affinity(
        &self,
        agent_id: Option<&str>,
        window_size: usize,
    ) -> Result<serde_json::Value> {
        let mut params = serde_json::Map::new();
        if let Some(agent_id) = agent_id {
            params.insert("agent_id".to_string(), serde_json::json!(agent_id));
        }
        params.insert("window_size".to_string(), serde_json::json!(window_size));
        self.call("tool.affinity", serde_json::Value::Object(params))
            .await
    }

    pub async fn load_context_packet(
        &self,
        goal: &str,
        context: Option<&str>,
    ) -> Result<ExecutionContextPacket> {
        let value = self
            .call(
                "context.packet.load",
                serde_json::json!({
                    "goal": goal,
                    "context": context,
                }),
            )
            .await?;
        serde_json::from_value(value).map_err(|e| anyhow!("Invalid context packet: {e}"))
    }

    pub async fn create_checkpoint(&self, artifact: ExecutionCheckpointArtifact) -> Result<()> {
        let _ = self
            .call("checkpoint.create", serde_json::to_value(artifact)?)
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bridge LLM Client
// ---------------------------------------------------------------------------

pub struct BridgeLlm {
    bridge: Arc<BridgeClient>,
}

impl BridgeLlm {
    pub fn new(bridge: Arc<BridgeClient>) -> Self {
        Self { bridge }
    }
}

#[async_trait::async_trait]
impl LlmClient for BridgeLlm {
    async fn chat(&self, messages: &[ChatMessage], json_mode: bool) -> Result<String> {
        let result = self
            .bridge
            .call(
                "llm.chat",
                serde_json::json!({
                    "messages": messages,
                    "json_mode": json_mode,
                    "agent_id": self.bridge.agent_id(),
                }),
            )
            .await?;

        result
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("LLM response missing content"))
    }
}

// ---------------------------------------------------------------------------
// Remote Tool Proxy
// ---------------------------------------------------------------------------

pub struct RemoteTool {
    name: String,
    bridge: Arc<BridgeClient>,
}

impl RemoteTool {
    pub fn new(name: String, bridge: Arc<BridgeClient>) -> Self {
        Self { name, bridge }
    }
}

#[async_trait::async_trait]
impl Tool for RemoteTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Proxied remote tool"
    }

    fn parameters_schema(&self) -> serde_json::Value {
        // In a more robust implementation, the daemon should provide the schema.
        // For now, we assume the LLM knows the schema from the system prompt.
        serde_json::json!({ "type": "object" })
    }

    async fn execute(&self, params: serde_json::Value) -> Result<ToolResult> {
        let result = self
            .bridge
            .call(
                "tool.execute",
                serde_json::json!({
                    "name": self.name,
                    "params": params,
                    "agent_id": self.bridge.agent_id(),
                }),
            )
            .await?;

        serde_json::from_value(result)
            .map_err(|e| anyhow!("Invalid tool result from bridge: {}", e))
    }
}

// ---------------------------------------------------------------------------
// ReAct Loop Implementation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub output: String,
    pub iterations: usize,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LlmAction {
    Done {
        done: bool,
        result: String,
    },
    ToolCall {
        tool: String,
        params: serde_json::Value,
    },
}

async fn emit_agent_log(
    bridge: &BridgeClient,
    entry_type: &str,
    content: String,
    tool_name: Option<&str>,
    tool_params: Option<Value>,
) {
    let mut payload = serde_json::json!({
        "event_type": "agent.log",
        "entry_type": entry_type,
        "content": content,
        "agent_id": bridge.agent_id(),
    });
    if let Some(name) = tool_name {
        payload["tool_name"] = Value::String(name.to_string());
    }
    if let Some(params) = tool_params {
        payload["tool_params"] = params;
    }
    let _ = bridge.notify("goal.event", payload).await;
}

async fn emit_agent_status(
    bridge: &BridgeClient,
    status: &str,
    detail: Option<String>,
    current_iteration: Option<usize>,
    max_iterations: Option<usize>,
    active_tool_name: Option<&str>,
) {
    let mut payload = serde_json::json!({
        "event_type": "agent.status",
        "status": status,
        "agent_id": bridge.agent_id(),
    });
    if let Some(detail) = detail {
        payload["detail"] = Value::String(detail);
    }
    if let Some(current_iteration) = current_iteration {
        payload["current_iteration"] = Value::from(current_iteration as u64);
    }
    if let Some(max_iterations) = max_iterations {
        payload["max_iterations"] = Value::from(max_iterations as u64);
    }
    if let Some(active_tool_name) = active_tool_name {
        payload["active_tool_name"] = Value::String(active_tool_name.to_string());
    }
    let _ = bridge.notify("goal.event", payload).await;
}

#[allow(clippy::too_many_arguments)]
pub async fn run_react_loop(
    goal: &str,
    context: &str,
    tools: &[Box<dyn Tool>],
    llm: &dyn LlmClient,
    custom_system_prompt: Option<&str>,
    bridge: Arc<BridgeClient>,
    context_packet: &ExecutionContextPacket,
    max_iterations: usize,
) -> Result<ExecutionResult> {
    let tool_refs: Vec<&dyn Tool> = tools.iter().map(|t| t.as_ref()).collect();
    let tools_prompt = format_tools_for_prompt(&tool_refs);

    let base_prompt = custom_system_prompt.unwrap_or(
        "You are an AI agent. Complete the user's goal. \
Use tools when you need to access external data or interact with the system. \
Respond ONLY with JSON. No markdown, no explanation outside JSON.",
    );

    let system_prompt = format!(
        "{base_prompt}\n\n{}\n{tools_prompt}",
        context_packet.prompt_prelude()
    );

    let mut messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt,
        },
        ChatMessage {
            role: "user".to_string(),
            content: if context.is_empty() {
                goal.to_string()
            } else {
                format!("Context:\n{context}\n\nGoal: {goal}")
            },
        },
    ];

    for iteration in 1..=max_iterations {
        let buffer_size: usize = messages.iter().map(|m| m.content.len()).sum();
        if buffer_size > MAX_BUFFER_BYTES {
            emit_agent_status(
                &bridge,
                "failed",
                Some("Agent message buffer exceeded limit".to_string()),
                Some(iteration),
                Some(max_iterations),
                None,
            )
            .await;
            return Err(anyhow!("Agent message buffer exceeded limit"));
        }

        let iteration_detail = format!("Iteration {}/{}", iteration, max_iterations);
        let _ = bridge
            .notify(
                "goal.event",
                serde_json::json!({
                    "event_type": "agent.step",
                    "status": "working",
                    "detail": iteration_detail,
                    "agent_id": bridge.agent_id(),
                }),
            )
            .await;
        emit_agent_status(
            &bridge,
            "running",
            Some(format!("Iteration {}/{}", iteration, max_iterations)),
            Some(iteration),
            Some(max_iterations),
            None,
        )
        .await;

        let response = llm.chat(&messages, true).await?;
        let json_str = extract_json_object(&response);
        let action = match json_str.and_then(|s| serde_json::from_str::<LlmAction>(s).ok()) {
            Some(action) => action,
            None => {
                // If we can't parse it, treat the raw text as the final answer
                emit_agent_log(&bridge, "result", response.clone(), None, None).await;
                emit_agent_status(
                    &bridge,
                    "completed",
                    Some("Completed with direct model response".to_string()),
                    Some(iteration),
                    Some(max_iterations),
                    None,
                )
                .await;
                return Ok(ExecutionResult {
                    output: response,
                    iterations: iteration,
                });
            }
        };

        match action {
            LlmAction::Done { done: true, result } => {
                emit_agent_log(&bridge, "result", result.clone(), None, None).await;
                emit_agent_status(
                    &bridge,
                    "completed",
                    Some("Execution completed".to_string()),
                    Some(iteration),
                    Some(max_iterations),
                    None,
                )
                .await;
                return Ok(ExecutionResult {
                    output: result,
                    iterations: iteration,
                });
            }
            LlmAction::Done {
                done: false,
                result,
            } => {
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response,
                });
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: format!("Continue. Previous partial result: {result}"),
                });
            }
            LlmAction::ToolCall { tool, params } => {
                let tool_impl = tools.iter().find(|t| t.name() == tool);

                // Notify daemon of tool call
                let _ = bridge
                    .notify(
                        "goal.event",
                        serde_json::json!({
                            "event_type": "agent.tool_call",
                            "status": "working",
                            "detail": format!("Calling tool: {}", tool),
                            "agent_id": bridge.agent_id(),
                        }),
                    )
                    .await;
                emit_agent_status(
                    &bridge,
                    "running",
                    Some(format!("Calling tool: {tool}")),
                    Some(iteration),
                    Some(max_iterations),
                    Some(&tool),
                )
                .await;
                emit_agent_log(
                    &bridge,
                    "tool",
                    format!("Calling tool: {tool}"),
                    Some(&tool),
                    Some(params.clone()),
                )
                .await;

                let (success, output) = match tool_impl {
                    Some(t) => match t.execute(params.clone()).await {
                        Ok(result) => (result.success, result.output),
                        Err(e) => (false, format!("Tool error: {e}")),
                    },
                    None => (false, format!("Unknown tool: {tool}")),
                };
                emit_agent_log(
                    &bridge,
                    if success { "result" } else { "blocked" },
                    output.clone(),
                    Some(&tool),
                    Some(params.clone()),
                )
                .await;
                emit_agent_status(
                    &bridge,
                    if success { "running" } else { "blocked" },
                    Some(if success {
                        format!("Tool {tool} completed")
                    } else {
                        output.clone()
                    }),
                    Some(iteration),
                    Some(max_iterations),
                    if success { None } else { Some(&tool) },
                )
                .await;

                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: response,
                });
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: format!(
                        "Tool result (success={success}):\n{output}\n\nContinue with the task."
                    ),
                });
            }
        }
    }

    emit_agent_log(
        &bridge,
        "blocked",
        "Agent exceeded maximum iterations".to_string(),
        None,
        None,
    )
    .await;
    emit_agent_status(
        &bridge,
        "failed",
        Some("Agent exceeded maximum iterations".to_string()),
        Some(max_iterations),
        Some(max_iterations),
        None,
    )
    .await;
    Err(anyhow!("Agent exceeded maximum iterations"))
}

fn extract_json_object(text: &str) -> Option<&str> {
    if serde_json::from_str::<serde_json::Value>(text).is_ok() {
        return Some(text);
    }

    let stripped = text.trim();
    if let Some(rest) = stripped.strip_prefix("```json") {
        if let Some(inner) = rest.strip_suffix("```") {
            let inner = inner.trim();
            if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
                return Some(inner);
            }
        }
    }
    if let Some(rest) = stripped.strip_prefix("```") {
        if let Some(inner) = rest.strip_suffix("```") {
            let inner = inner.trim();
            if serde_json::from_str::<serde_json::Value>(inner).is_ok() {
                return Some(inner);
            }
        }
    }

    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, &b) in bytes.iter().enumerate() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if b == b'\\' && in_string {
            escape_next = true;
            continue;
        }
        if b == b'"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        if b == b'{' {
            if depth == 0 {
                start = Some(i);
            }
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                if let Some(s) = start {
                    let candidate = &text[s..=i];
                    if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                        return Some(candidate);
                    }
                }
                start = None;
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_json_object_accepts_raw_json() {
        let input = r#"{"done":true,"result":"ok"}"#;
        assert_eq!(extract_json_object(input), Some(input));
    }

    #[test]
    fn extract_json_object_extracts_fenced_json() {
        let input = "```json\n{\"tool\":\"ask_user\",\"params\":{\"question\":\"What?\"}}\n```";
        let extracted = extract_json_object(input).expect("should extract JSON");
        assert!(extracted.contains("\"ask_user\""));
    }

    #[test]
    fn extract_json_object_finds_embedded_json_object() {
        let input = "Here is my response:\n{\"tool\": \"ask_user\", \"params\": {\"question\": \"What?\"}}\nLet me know.";
        let extracted = extract_json_object(input).expect("should extract embedded JSON");
        assert!(extracted.contains("\"ask_user\""));
    }
}
