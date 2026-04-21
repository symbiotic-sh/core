use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{Duration, Utc};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;
use symbiotic_agents::monitoring::{
    self, AgentMonitor, ExecutionFilter, ExecutionStatus, MonitorConfig, SqliteAgentMonitor,
};
use symbiotic_archive::{ArchiveSensitivity, FileArchiveStore};
use symbiotic_core::intake::{
    normalize_url, IntakeBatchResult, IntakeKind, IntakeRequest, IntakeSource,
};
use symbiotic_core::now_unix;
use symbiotic_daemon::{DaemonConfig, SymbioticDaemon};
use symbiotic_domains::{DomainQueueStore, DomainTask, QueueKind};
use symbiotic_metrics::{MetricStore, ProposalConfig, ProposalEngine, ProposalStatus, TimeWindow};
use symbiotic_vault_store::keys::{Identity as AgeIdentity, Recipient as AgeRecipient};
use symbiotic_vault_store::{BlobCategory, BlobMetadata, BlobStore};

#[derive(Debug, Parser)]
#[command(name = "symbiotic")]
#[command(about = "Symbiotic CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Intake(IntakeArgs),
    Ingest(IngestArgs),
    Bookmarks(BookmarksArgs),
    Domain(DomainArgs),
    Metrics(MetricsArgs),
    /// Monitor and inspect agent executions
    Agents(AgentsArgs),
    /// Run a one-shot agent goal against the local Archive (researcher, planner, etc.)
    Agent(AgentArgs),
    /// Migrate existing Private archive entries to age-encrypted blob store
    MigratePrivate(MigratePrivateArgs),
    /// Blob store management commands
    BlobStore(BlobStoreArgs),
}

#[derive(Debug, Args)]
struct AgentArgs {
    #[command(subcommand)]
    command: AgentCommand,
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Dispatch a goal to a registered agent role (e.g. `researcher`) and print
    /// the final answer. Grounds the agent in the local Archive via Recall.
    Run {
        /// Agent role name as declared in `config/agents/<role>.toml` or built-in
        /// defaults (researcher, planner, coder, reviewer, inquisitor, ...).
        #[arg(long, default_value = "researcher")]
        role: String,
        /// Goal / question to hand to the agent.
        #[arg(long)]
        goal: String,
    },
}

#[derive(Debug, Args)]
struct IntakeArgs {
    #[arg(value_name = "URL")]
    url: Option<String>,
    #[arg(long, value_name = "FILE")]
    file: Option<PathBuf>,
    #[arg(long)]
    stdin: bool,
    #[arg(long, value_name = "TEXT")]
    note: Option<String>,
    #[arg(short, long, value_delimiter = ',')]
    tags: Vec<String>,
    /// Explicit title for the ingested entry. When set for `--note`, skips
    /// LLM title generation and lands directly on the stored record.
    #[arg(long, value_name = "TEXT")]
    title: Option<String>,
}

#[derive(Debug, Args)]
struct IngestArgs {
    url: String,
    #[arg(short, long, value_delimiter = ',')]
    tags: Vec<String>,
}

#[derive(Debug, Args)]
struct BookmarksArgs {
    #[command(subcommand)]
    command: BookmarksCommand,
}

#[derive(Debug, Subcommand)]
enum BookmarksCommand {
    Sync {
        #[arg(value_parser = ["api", "browser"])]
        source: String,
        #[arg(default_value_t = 25)]
        limit: u32,
    },
}

#[derive(Debug, Args)]
struct DomainArgs {
    #[command(subcommand)]
    command: DomainCommand,
}

#[derive(Debug, Subcommand)]
enum DomainCommand {
    Queue(DomainQueueArgs),
}

#[derive(Debug, Args)]
struct DomainQueueArgs {
    #[command(subcommand)]
    command: DomainQueueCommand,
}

#[derive(Debug, Subcommand)]
enum DomainQueueCommand {
    List {
        domain: String,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long)]
        assignee: Option<String>,
        #[arg(long)]
        backlog: bool,
        #[arg(long)]
        active: bool,
    },
    Add {
        domain: String,
        title: String,
        #[arg(long)]
        goal: Option<String>,
        #[arg(long)]
        stream: Option<String>,
        #[arg(long, default_value_t = 3)]
        priority: u8,
        #[arg(long)]
        backlog: bool,
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        context: Option<String>,
    },
    Status {
        domain: String,
        task_id: String,
        status: String,
    },
    Assign {
        domain: String,
        task_id: String,
        assignee: String,
    },
    Promote {
        domain: String,
        task_id: String,
    },
}

#[derive(Debug, Args)]
struct MetricsArgs {
    #[command(subcommand)]
    command: MetricsCommand,
}

#[derive(Debug, Subcommand)]
enum MetricsCommand {
    /// Show metrics summary for a time window
    Summary {
        /// Agent to filter by
        #[arg(long)]
        agent: Option<String>,
        /// Time window: 1h, 24h, 7d
        #[arg(long, default_value = "24h")]
        window: String,
    },
    /// Show or manage improvement proposals
    Proposals(MetricsProposalsArgs),
    /// Show cost breakdown
    Cost {
        /// Time window: 1h, 24h, 7d
        #[arg(long, default_value = "7d")]
        window: String,
    },
    /// Run proposal engine evaluation
    Evaluate,
}

#[derive(Debug, Args)]
struct MetricsProposalsArgs {
    #[command(subcommand)]
    command: Option<MetricsProposalsCommand>,
}

#[derive(Debug, Subcommand)]
enum MetricsProposalsCommand {
    /// Approve a proposal
    Approve { proposal_id: String },
    /// Reject a proposal
    Reject { proposal_id: String },
}

#[derive(Debug, Args)]
struct AgentsArgs {
    #[command(subcommand)]
    command: AgentsCommand,
}

#[derive(Debug, Subcommand)]
enum AgentsCommand {
    /// Show execution summary (success rate, avg duration, active count)
    Status {
        /// Time window: 1h, 24h, 7d
        #[arg(long, default_value = "24h")]
        window: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List recent agent executions
    List {
        /// Filter by agent ID
        #[arg(long)]
        agent: Option<String>,
        /// Filter by task ID
        #[arg(long)]
        task: Option<String>,
        /// Filter by status: Running, Success, Failed, Cancelled, Handoff
        #[arg(long)]
        status: Option<String>,
        /// Time window: 1h, 24h, 7d
        #[arg(long, default_value = "24h")]
        window: String,
        /// Maximum number of results
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Show execution history with full details
    History {
        /// Execution ID to inspect
        execution_id: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
}

// ---------------------------------------------------------------------------
// migrate-private command
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
struct MigratePrivateArgs {
    /// Archive directory (default: knowledge-base/)
    #[arg(long, default_value = "knowledge-base/")]
    archive_root: PathBuf,

    /// Blob store directory (default: data/blob-store/)
    #[arg(long, default_value = "data/blob-store/")]
    blob_store_root: PathBuf,

    /// Age identity key file (required)
    #[arg(long)]
    key_file: PathBuf,

    /// Show what would be migrated without making changes
    #[arg(long)]
    dry_run: bool,

    /// Skip confirmation prompt
    #[arg(long)]
    yes: bool,
}

// ---------------------------------------------------------------------------
// blob-store command
// ---------------------------------------------------------------------------

#[derive(Debug, Args)]
struct BlobStoreArgs {
    #[command(subcommand)]
    command: BlobStoreCommand,
}

#[derive(Debug, Subcommand)]
enum BlobStoreCommand {
    /// Pre-flight check: verify blob store is working correctly
    Test {
        /// Age identity key file (required)
        #[arg(long)]
        key_file: PathBuf,

        /// Blob store directory (default: data/blob-store/)
        #[arg(long, default_value = "data/blob-store/")]
        blob_store_root: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// Migration report
// ---------------------------------------------------------------------------

/// Summary of a migration run.
pub struct MigrationReport {
    pub scanned: u32,
    pub migrated: u32,
    pub skipped: u32,
    pub errors: Vec<(String, String)>,
}

impl MigrationReport {
    fn new() -> Self {
        Self {
            scanned: 0,
            migrated: 0,
            skipped: 0,
            errors: Vec::new(),
        }
    }
}

fn main() -> Result<()> {
    // Honor `RUST_LOG`; default to `warn` so silent daemon errors become visible
    // without drowning the user in routine info-level chatter.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let cli = Cli::parse();

    match cli.command {
        // Commands that do NOT need the full daemon.
        Command::Domain(args) => run_domain(args),
        Command::Metrics(args) => run_metrics(args),
        Command::Agents(args) => run_agents(args),
        Command::MigratePrivate(args) => run_migrate_private(args),
        Command::BlobStore(args) => run_blob_store(args),

        // Commands that need the daemon.
        command => {
            // Honor the same LLM-provider env vars the daemon binary reads so
            // `symbiotic agent run` (and future provider-dependent commands)
            // pick up `SYMBIOTIC_OLLAMA_URL`, `ANTHROPIC_API_KEY`, etc. without
            // extra plumbing.
            let config = daemon_config_from_env();
            let (daemon, _matrix_outbound_rx, _conflict_goal_rx) = SymbioticDaemon::open(config)?;
            match command {
                Command::Ingest(args) => {
                    let result = run_ingest_alias(args, &daemon)?;
                    print_summary(&result);
                    Ok(())
                }
                Command::Intake(args) => {
                    let result = run_intake(args, &daemon)?;
                    print_summary(&result);
                    Ok(())
                }
                Command::Bookmarks(args) => run_bookmarks(args, &daemon),
                Command::Agent(args) => run_agent(args, &daemon),
                // Already handled above; unreachable.
                _ => unreachable!(),
            }
        }
    }
}

fn run_agent(args: AgentArgs, daemon: &SymbioticDaemon) -> Result<()> {
    match args.command {
        AgentCommand::Run { role, goal } => {
            // Agent backends (React in-process loop, or the sandboxed Runner)
            // both need a tokio runtime handle on the calling thread: React
            // because the ReAct loop's tools are async, Runner because the LLM
            // Gateway bridge runs on tokio. We create one here and block on
            // the synchronous daemon method from inside it so `Handle::try_current()`
            // sees the runtime. The daemon itself remains non-async; tokio is
            // scoped to this invocation only.
            let rt = tokio::runtime::Runtime::new()
                .context("failed to create tokio runtime for agent run")?;
            let output = rt.block_on(async { daemon.run_agent_goal(&role, &goal) })?;
            // The ReAct loop wraps its terminal answer in `{"done": true, "result": "..."}`.
            // Strip that envelope so users see the answer directly; fall back to raw
            // output if the format ever changes or the agent returns plain text.
            let pretty = serde_json::from_str::<serde_json::Value>(output.trim())
                .ok()
                .and_then(|v| {
                    v.get("result")
                        .and_then(|r| r.as_str())
                        .map(|s| s.to_string())
                })
                .unwrap_or(output);
            println!("{pretty}");
            Ok(())
        }
    }
}

/// Build a `DaemonConfig` from `DaemonConfig::default()` plus the LLM-relevant
/// environment variables the daemon binary also reads. This lets CLI commands
/// that need a provider (e.g. `symbiotic agent run`) honor the same
/// `SYMBIOTIC_OLLAMA_URL` / `ANTHROPIC_API_KEY` / ... vars users already set
/// when running the daemon directly.
fn daemon_config_from_env() -> DaemonConfig {
    let mut cfg = DaemonConfig::default();
    let take = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    // Let the caller point at a disk roles dir (TOML overrides the built-in
    // defaults). Without this, DaemonConfig::default() uses the relative path
    // `config/agents` which only resolves when you run from the runtime root.
    if let Some(dir) = take("SYMBIOTIC_AGENT_ROLES_DIR") {
        cfg.role_dir = std::path::PathBuf::from(dir);
    }
    cfg.default_provider = take("SYMBIOTIC_DEFAULT_PROVIDER");
    cfg.ollama_url = take("SYMBIOTIC_OLLAMA_URL");
    cfg.ollama_chat_model = take("SYMBIOTIC_OLLAMA_CHAT_MODEL");
    cfg.anthropic_api_key = take("ANTHROPIC_API_KEY");
    cfg.openai_api_key = take("OPENAI_API_KEY");
    cfg.openrouter_api_key = take("OPENROUTER_API_KEY");
    cfg.openrouter_model = take("SYMBIOTIC_OPENROUTER_MODEL");
    cfg.gemini_api_key = take("GEMINI_API_KEY");
    cfg.gemini_model = take("SYMBIOTIC_GEMINI_MODEL");
    if let Ok(val) = std::env::var("SYMBIOTIC_AGENT_BACKEND") {
        match val.trim().to_lowercase().as_str() {
            "cli" => cfg.agent_backend = symbiotic_daemon::AgentBackend::Cli,
            "runner" => cfg.agent_backend = symbiotic_daemon::AgentBackend::Runner,
            "react" | "" => cfg.agent_backend = symbiotic_daemon::AgentBackend::React,
            _ => {}
        }
    }
    cfg
}

fn run_bookmarks(args: BookmarksArgs, daemon: &SymbioticDaemon) -> Result<()> {
    match args.command {
        BookmarksCommand::Sync { source, limit } => {
            let job_id = daemon.queue_bookmarks_sync(&source, limit)?;
            println!("queued bookmarks sync job_id={job_id}");
            Ok(())
        }
    }
}

fn run_domain(args: DomainArgs) -> Result<()> {
    match args.command {
        DomainCommand::Queue(queue_args) => run_domain_queue(queue_args),
    }
}

fn run_domain_queue(args: DomainQueueArgs) -> Result<()> {
    let store = DomainQueueStore::open("domains")?;
    match args.command {
        DomainQueueCommand::List {
            domain,
            status,
            goal,
            assignee,
            backlog,
            active,
        } => {
            let kind = if backlog && !active {
                Some(QueueKind::Backlog)
            } else if active && !backlog {
                Some(QueueKind::Active)
            } else {
                None
            };
            let tasks = if domain == "all" {
                store
                    .list_all_tasks(kind.clone())?
                    .into_iter()
                    .map(|(domain, task)| (Some(domain), task))
                    .collect::<Vec<_>>()
            } else {
                store
                    .list_tasks(&domain, kind.clone())?
                    .into_iter()
                    .map(|task| (None, task))
                    .collect::<Vec<_>>()
            };

            for (domain_label, task) in tasks {
                if let Some(filter) = status.as_deref() {
                    if task.status != filter {
                        continue;
                    }
                }
                if let Some(filter) = goal.as_deref() {
                    if task.goal.as_deref() != Some(filter) {
                        continue;
                    }
                }
                if let Some(filter) = assignee.as_deref() {
                    if task.assignee.as_deref() != Some(filter) {
                        continue;
                    }
                }
                let domain_prefix = domain_label
                    .as_ref()
                    .map(|value| format!("[{value}] "))
                    .unwrap_or_default();
                println!(
                    "{}{} | {} | status={} priority={} assignee={}",
                    domain_prefix,
                    task.id,
                    task.title,
                    task.status,
                    task.priority,
                    task.assignee
                        .clone()
                        .unwrap_or_else(|| "unassigned".to_string())
                );
            }
        }
        DomainQueueCommand::Add {
            domain,
            title,
            goal,
            stream,
            priority,
            backlog,
            id,
            context,
        } => {
            let context_value = match context {
                Some(raw) => serde_json::from_str::<Value>(&raw)
                    .context("context must be valid JSON object")?,
                None => Value::Object(Default::default()),
            };
            let task_id = id.unwrap_or_else(|| store.generate_task_id(&domain));
            let task = DomainTask {
                id: task_id.clone(),
                title,
                goal,
                stream,
                status: "pending".to_string(),
                priority,
                assignee: None,
                created_at: now_unix(),
                due_at: None,
                context: context_value,
            };
            store.add_task(
                &domain,
                task.clone(),
                if backlog {
                    QueueKind::Backlog
                } else {
                    QueueKind::Active
                },
            )?;
            println!("added {} to {}", task.id, domain);
        }
        DomainQueueCommand::Status {
            domain,
            task_id,
            status,
        } => {
            let task = store.update_status(&domain, &task_id, &status)?;
            println!("updated {} status={}", task.id, task.status);
        }
        DomainQueueCommand::Assign {
            domain,
            task_id,
            assignee,
        } => {
            let task = store.assign_task(&domain, &task_id, &assignee)?;
            println!(
                "assigned {} to {}",
                task.id,
                task.assignee.unwrap_or_default()
            );
        }
        DomainQueueCommand::Promote { domain, task_id } => {
            let task = store.promote_task(&domain, &task_id)?;
            println!("promoted {} to active queue", task.id);
        }
    }
    Ok(())
}

fn metrics_db_path() -> PathBuf {
    PathBuf::from("data/metrics/metrics.db")
}

fn run_metrics(args: MetricsArgs) -> Result<()> {
    let db_path = metrics_db_path();
    let store =
        MetricStore::open(&db_path).map_err(|e| anyhow!("failed to open metrics store: {e}"))?;

    match args.command {
        MetricsCommand::Summary { agent, window } => {
            let tw: TimeWindow = window.parse().map_err(|e: String| anyhow!(e))?;
            let output = symbiotic_metrics::format_summary(&store, tw, agent.as_deref())
                .map_err(|e| anyhow!("metrics error: {e}"))?;
            print!("{output}");
        }
        MetricsCommand::Proposals(proposals_args) => match proposals_args.command {
            None => {
                let proposals = store
                    .list_proposals(Some("Pending"))
                    .map_err(|e| anyhow!("metrics error: {e}"))?;
                let output = symbiotic_metrics::format_proposals(&proposals);
                print!("{output}");
            }
            Some(MetricsProposalsCommand::Approve { proposal_id }) => {
                let id = uuid::Uuid::parse_str(&proposal_id).context("invalid proposal ID")?;
                store
                    .update_proposal_status(&id, &ProposalStatus::Approved)
                    .map_err(|e| anyhow!("metrics error: {e}"))?;
                println!("Proposal {proposal_id} approved.");
            }
            Some(MetricsProposalsCommand::Reject { proposal_id }) => {
                let id = uuid::Uuid::parse_str(&proposal_id).context("invalid proposal ID")?;
                store
                    .update_proposal_status(&id, &ProposalStatus::Rejected)
                    .map_err(|e| anyhow!("metrics error: {e}"))?;
                println!("Proposal {proposal_id} rejected.");
            }
        },
        MetricsCommand::Cost { window } => {
            let tw: TimeWindow = window.parse().map_err(|e: String| anyhow!(e))?;
            let output = symbiotic_metrics::format_summary(&store, tw, None)
                .map_err(|e| anyhow!("metrics error: {e}"))?;
            print!("{output}");
        }
        MetricsCommand::Evaluate => {
            let config = ProposalConfig::default();
            let engine = ProposalEngine::new(&store, config);
            let proposals = engine
                .evaluate()
                .map_err(|e| anyhow!("evaluation error: {e}"))?;
            if proposals.is_empty() {
                println!("No new proposals generated.");
            } else {
                println!("Generated {} new proposal(s):", proposals.len());
                for p in &proposals {
                    println!("  [{}] {}", p.priority, p.suggestion);
                }
            }
        }
    }
    Ok(())
}

fn agent_monitor_db_path() -> PathBuf {
    PathBuf::from("data/agent-monitor/monitor.db")
}

fn parse_window_duration(window: &str) -> Result<Duration> {
    match window {
        "1h" => Ok(Duration::hours(1)),
        "24h" => Ok(Duration::hours(24)),
        "7d" => Ok(Duration::days(7)),
        "30d" => Ok(Duration::days(30)),
        other => Err(anyhow!(
            "unknown time window: {other} (expected 1h, 24h, 7d, 30d)"
        )),
    }
}

fn run_agents(args: AgentsArgs) -> Result<()> {
    let db_path = agent_monitor_db_path();
    let monitor = SqliteAgentMonitor::open(&db_path, &MonitorConfig::default())
        .map_err(|e| anyhow!("failed to open agent monitor: {e}"))?;

    match args.command {
        AgentsCommand::Status { window, json } => {
            let duration = parse_window_duration(&window)?;
            let since = Utc::now() - duration;
            let summary = monitor
                .summary(since)
                .map_err(|e| anyhow!("monitor error: {e}"))?;

            if json {
                let output = monitoring::format_execution_summary_json(&summary)
                    .map_err(|e| anyhow!("json error: {e}"))?;
                println!("{output}");
            } else {
                let output = monitoring::format_execution_summary(&summary, &window);
                print!("{output}");
            }
        }
        AgentsCommand::List {
            agent,
            task,
            status,
            window,
            limit,
            json,
        } => {
            let duration = parse_window_duration(&window)?;
            let since = Utc::now() - duration;

            let status_filter = match status {
                Some(ref s) => {
                    let parsed: ExecutionStatus = s
                        .parse()
                        .map_err(|e: String| anyhow!("invalid status: {e}"))?;
                    Some(parsed)
                }
                None => None,
            };

            let filter = ExecutionFilter {
                agent_id: agent,
                task_id: task,
                status: status_filter,
                limit: Some(limit),
                ..Default::default()
            };

            let executions = monitor
                .query_executions(&filter, since)
                .map_err(|e| anyhow!("monitor error: {e}"))?;

            if json {
                let output = monitoring::format_execution_list_json(&executions)
                    .map_err(|e| anyhow!("json error: {e}"))?;
                println!("{output}");
            } else {
                let output = monitoring::format_execution_list(&executions);
                print!("{output}");
            }
        }
        AgentsCommand::History { execution_id, json } => {
            let exec = monitor
                .get_execution(&execution_id)
                .map_err(|e| anyhow!("monitor error: {e}"))?;

            match exec {
                Some(exec) => {
                    if json {
                        let output = serde_json::to_string_pretty(&exec)
                            .context("json serialization failed")?;
                        println!("{output}");
                    } else {
                        println!("Execution: {}", exec.execution_id);
                        println!("Agent:     {}", exec.agent_id);
                        println!("Type:      {}", exec.agent_type);
                        println!("Status:    {}", exec.status);
                        if let Some(ref task) = exec.task_id {
                            println!("Task:      {task}");
                        }
                        if let Some(ref parent) = exec.parent_id {
                            println!("Parent:    {parent}");
                        }
                        if let Some(ref model) = exec.model {
                            println!("Model:     {model}");
                        }
                        println!(
                            "Started:   {}",
                            exec.started_at.format("%Y-%m-%d %H:%M:%S UTC")
                        );
                        if let Some(finished) = exec.finished_at {
                            println!("Finished:  {}", finished.format("%Y-%m-%d %H:%M:%S UTC"));
                            let dur = finished - exec.started_at;
                            println!("Duration:  {:.1}s", dur.num_milliseconds() as f64 / 1000.0);
                        }
                        println!("Iterations: {}", exec.iterations);
                        println!("Tool calls: {}", exec.tool_call_count);
                        if let Some(tokens) = exec.tokens_in {
                            println!("Tokens in:  {tokens}");
                        }
                        if let Some(tokens) = exec.tokens_out {
                            println!("Tokens out: {tokens}");
                        }
                        if let Some(ref err) = exec.error_message {
                            println!("Error:     {err}");
                        }
                    }
                }
                None => {
                    return Err(anyhow!("execution {execution_id} not found"));
                }
            }
        }
    }
    Ok(())
}

fn run_ingest_alias(args: IngestArgs, daemon: &SymbioticDaemon) -> Result<IntakeBatchResult> {
    let normalized = normalize_url(&args.url)?;
    let request = IntakeRequest {
        source: IntakeSource::Cli,
        kind: IntakeKind::Url,
        urls: vec![normalized],
        note: None,
        tags: args.tags,
        file_path: None,
        title: None,
    };
    daemon.submit_intake_request(request)
}

fn run_intake(args: IntakeArgs, daemon: &SymbioticDaemon) -> Result<IntakeBatchResult> {
    if let Some(note) = args.note {
        let request = IntakeRequest {
            source: IntakeSource::Cli,
            kind: IntakeKind::Note,
            urls: vec![],
            note: Some(note),
            tags: args.tags,
            file_path: None,
            title: args.title,
        };
        return daemon.submit_intake_request(request);
    }

    let raw_urls = collect_url_inputs(args.url, args.file, args.stdin)?;
    if raw_urls.is_empty() {
        return Err(anyhow!(
            "no intake input provided; use URL argument, --file, --stdin, or --note"
        ));
    }

    daemon.submit_intake_urls_raw(raw_urls, args.tags, IntakeSource::Cli)
}

fn collect_url_inputs(
    url: Option<String>,
    file: Option<PathBuf>,
    use_stdin: bool,
) -> Result<Vec<String>> {
    let mut sources_used = 0usize;
    if url.is_some() {
        sources_used += 1;
    }
    if file.is_some() {
        sources_used += 1;
    }
    if use_stdin {
        sources_used += 1;
    }
    if sources_used > 1 {
        return Err(anyhow!(
            "choose exactly one URL input source: positional URL, --file, or --stdin"
        ));
    }

    if let Some(single_url) = url {
        return Ok(vec![single_url]);
    }

    if let Some(path) = file {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read input file {}", path.display()))?;
        return Ok(split_lines(content));
    }

    if use_stdin {
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .context("failed to read URLs from stdin")?;
        return Ok(split_lines(buffer));
    }

    Ok(Vec::new())
}

fn split_lines(content: String) -> Vec<String> {
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

fn print_summary(result: &IntakeBatchResult) {
    println!("run_id={}", result.run_id);
    println!(
        "summary total={} ingested={} duplicate={} blocked={} invalid={} failed={} secure_routed={} review_queued={}",
        result.summary.total,
        result.summary.ingested,
        result.summary.duplicates,
        result.summary.blocked,
        result.summary.invalid,
        result.summary.failed,
        result.summary.secure_routed,
        result.summary.review_queued
    );
    for item in &result.items {
        println!(
            "item status={:?} route={:?} input={} review_queued={} error={}",
            item.status,
            item.route,
            item.input,
            item.review_queued,
            item.error.clone().unwrap_or_default()
        );
    }
}

// ---------------------------------------------------------------------------
// blob-store test command
// ---------------------------------------------------------------------------

fn run_blob_store(args: BlobStoreArgs) -> Result<()> {
    match args.command {
        BlobStoreCommand::Test {
            key_file,
            blob_store_root,
        } => run_blob_store_test(&key_file, &blob_store_root),
    }
}

/// Pre-flight check: verify that the blob store is operational.
///
/// 1. Verifies key file exists and contains a valid age identity.
/// 2. Creates a test blob, reads it back, verifies content matches.
/// 3. Deletes the test blob.
/// 4. Reports success or failure.
fn run_blob_store_test(key_file: &PathBuf, blob_store_root: &Path) -> Result<()> {
    println!("Blob store pre-flight check");
    println!("===========================");
    println!();

    // Step 1: Validate key file.
    print!("  [1/4] Checking key file... ");
    let (identity, recipient) = load_age_keypair(key_file)?;
    println!("OK (valid age identity)");

    // Step 2: Open/create blob store.
    print!(
        "  [2/4] Opening blob store at {}... ",
        blob_store_root.display()
    );
    let store = BlobStore::new(blob_store_root.to_path_buf())
        .map_err(|e| anyhow!("failed to open blob store: {e}"))?;
    println!("OK");

    // Step 3: Store, read back, and verify.
    print!("  [3/4] Store/read/verify round-trip... ");
    let test_id = format!("__preflight_test_{}", now_unix());
    let test_content = b"symbiotic blob store pre-flight check";
    let test_metadata = BlobMetadata {
        title: "Pre-flight test".to_string(),
        tags: vec!["test".to_string()],
        size_bytes: test_content.len() as u64,
        content_type: "text/plain".to_string(),
    };

    store
        .store(
            &test_id,
            BlobCategory::Custom("test".to_string()),
            test_metadata,
            test_content,
            &[&recipient],
        )
        .map_err(|e| anyhow!("failed to store test blob: {e}"))?;

    let readback = store
        .read(&test_id, &identity)
        .map_err(|e| anyhow!("failed to read test blob: {e}"))?;

    if readback != test_content {
        // Clean up before failing.
        let _ = store.delete(&test_id);
        return Err(anyhow!(
            "content mismatch: stored {} bytes, read back {} bytes",
            test_content.len(),
            readback.len()
        ));
    }
    println!("OK (content verified)");

    // Step 4: Delete test blob.
    print!("  [4/4] Cleaning up test blob... ");
    store
        .delete(&test_id)
        .map_err(|e| anyhow!("failed to delete test blob: {e}"))?;
    println!("OK");

    println!();
    println!("All checks passed. Blob store is operational.");
    Ok(())
}

// ---------------------------------------------------------------------------
// migrate-private command
// ---------------------------------------------------------------------------

fn run_migrate_private(args: MigratePrivateArgs) -> Result<()> {
    println!("Symbiotic Private Entry Migration");
    println!("=================================");
    println!();

    // Step 1: Load age keypair.
    let (identity, recipient) = load_age_keypair(&args.key_file)?;
    println!("  Key file:       {} (valid)", args.key_file.display());
    println!("  Archive root:   {}", args.archive_root.display());
    println!("  Blob store:     {}", args.blob_store_root.display());
    if args.dry_run {
        println!("  Mode:           DRY RUN (no changes will be made)");
    }
    println!();

    // Step 2: Open archive and blob store.
    let archive = FileArchiveStore::open(&args.archive_root)
        .with_context(|| format!("failed to open archive at {}", args.archive_root.display()))?;

    let blob_store = BlobStore::new(args.blob_store_root.clone()).map_err(|e| {
        anyhow!(
            "failed to open blob store at {}: {e}",
            args.blob_store_root.display()
        )
    })?;

    // Step 3: Scan archive for Private entries.
    let all_docs = archive.list().context("failed to list archive entries")?;

    let private_docs: Vec<_> = all_docs
        .into_iter()
        .filter(|doc| doc.sensitivity == ArchiveSensitivity::Private)
        .collect();

    let total_archive_count = archive.list().map(|d| d.len()).unwrap_or(0) as u32;
    println!("  Total archive entries: {total_archive_count}");
    println!("  Private entries found: {}", private_docs.len());

    // Filter out already-migrated entries.
    let candidates: Vec<_> = private_docs
        .into_iter()
        .filter(|doc| !is_already_migrated(doc))
        .collect();

    println!("  Eligible for migration: {}", candidates.len());
    println!();

    if candidates.is_empty() {
        println!("Nothing to migrate.");
        return Ok(());
    }

    // Show what will be migrated.
    println!("Entries to migrate:");
    for doc in &candidates {
        let category = detect_blob_category(&doc.content);
        println!(
            "  {} | {} | category={} | {} bytes",
            doc.record_id,
            doc.title,
            category,
            doc.content.len()
        );
    }
    println!();

    if args.dry_run {
        println!("Dry run complete. No changes were made.");
        println!(
            "Re-run without --dry-run to migrate {} entries.",
            candidates.len()
        );
        return Ok(());
    }

    // Confirmation prompt (unless --yes).
    if !args.yes {
        print!(
            "Migrate {} Private entries to encrypted blob store? [y/N] ",
            candidates.len()
        );
        io::Write::flush(&mut io::stdout())?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    // Step 4: Run migration.
    println!();
    println!("Migrating...");
    let report = migrate_entries(&archive, &blob_store, &identity, &recipient, candidates)?;

    // Step 5: Print report.
    println!();
    println!("Migration Report");
    println!("================");
    println!("  Scanned:  {}", report.scanned);
    println!("  Migrated: {}", report.migrated);
    println!("  Skipped:  {}", report.skipped);
    println!("  Errors:   {}", report.errors.len());

    if !report.errors.is_empty() {
        println!();
        println!("Errors:");
        for (record_id, error_msg) in &report.errors {
            println!("  {record_id}: {error_msg}");
        }
    }

    if report.migrated > 0 {
        println!();
        println!(
            "Successfully migrated {} entries to encrypted blob store.",
            report.migrated
        );
    }

    Ok(())
}

/// Check if a document has already been migrated to the blob store.
///
/// An entry is considered already migrated if:
/// - Its title starts with "[encrypted]", OR
/// - It has a "tier3/encrypted" tag, OR
/// - Its content contains the YAML placeholder marker "status: encrypted"
fn is_already_migrated(doc: &symbiotic_archive::ArchiveDocument) -> bool {
    if doc.title.starts_with("[encrypted]") {
        return true;
    }
    if doc.tags.iter().any(|t| t == "tier3/encrypted") {
        return true;
    }
    // Also check content for the YAML placeholder signature.
    if doc.content.contains("status: encrypted") && doc.content.contains("blob_id:") {
        return true;
    }
    false
}

/// Perform the actual migration of entries.
fn migrate_entries(
    archive: &FileArchiveStore,
    blob_store: &BlobStore,
    _identity: &AgeIdentity,
    recipient: &AgeRecipient,
    candidates: Vec<symbiotic_archive::ArchiveDocument>,
) -> Result<MigrationReport> {
    let mut report = MigrationReport::new();
    report.scanned = candidates.len() as u32;

    for doc in candidates {
        let record_id = &doc.record_id;
        let title = &doc.title;
        let content = &doc.content;

        // Skip entries with no meaningful content.
        if content.trim().is_empty() {
            report.skipped += 1;
            println!("  SKIP {record_id} (empty content)");
            continue;
        }

        let category = detect_blob_category(content);
        let content_bytes = content.as_bytes();

        let metadata = BlobMetadata {
            title: title.clone(),
            tags: doc.tags.clone(),
            size_bytes: content_bytes.len() as u64,
            content_type: "text/markdown".to_string(),
        };

        // Store encrypted blob.
        match blob_store.store(
            record_id,
            category.clone(),
            metadata,
            content_bytes,
            &[recipient],
        ) {
            Ok(_blob) => {
                // Replace archive content with metadata-only placeholder.
                let placeholder = format!(
                    "---\nblob_id: {record_id}\nstatus: encrypted\ncategory: {category}\n\
                     title: {title}\n---\n\n> This entry is Tier 3 (Private). \
                     Content is stored in the age-encrypted blob store.\n"
                );

                match archive.update_content(
                    record_id,
                    &placeholder,
                    Some(&format!("[encrypted] {title}")),
                    &["tier3/encrypted".to_string()],
                ) {
                    Ok(true) => {
                        report.migrated += 1;
                        println!(
                            "  OK   {record_id} | {title} | category={category} | {} bytes",
                            content_bytes.len()
                        );
                    }
                    Ok(false) => {
                        report.errors.push((
                            record_id.to_string(),
                            "archive record not found during placeholder update".to_string(),
                        ));
                        println!("  ERR  {record_id} (record not found for placeholder update)");
                    }
                    Err(e) => {
                        report.errors.push((
                            record_id.to_string(),
                            format!("placeholder update failed: {e}"),
                        ));
                        println!("  ERR  {record_id} (placeholder update failed: {e})");
                    }
                }
            }
            Err(e) => {
                report.errors.push((
                    record_id.to_string(),
                    format!("blob store write failed: {e}"),
                ));
                println!("  ERR  {record_id} (blob store write failed: {e})");
            }
        }
    }

    Ok(report)
}

// ---------------------------------------------------------------------------
// Age key helpers
// ---------------------------------------------------------------------------

/// Load an age identity (secret key) from a file and derive the public
/// recipient from it.
fn load_age_keypair(key_file: &PathBuf) -> Result<(AgeIdentity, AgeRecipient)> {
    let key_data = fs::read_to_string(key_file)
        .with_context(|| format!("failed to read key file {}", key_file.display()))?;

    let identity: AgeIdentity = key_data
        .trim()
        .parse()
        .map_err(|e| anyhow!("invalid age identity in {}: {e}", key_file.display()))?;

    let recipient = identity.to_public();
    Ok((identity, recipient))
}

// ---------------------------------------------------------------------------
// Blob category detection (local copy -- daemon's version is pub(crate))
// ---------------------------------------------------------------------------

/// Auto-detect a `BlobCategory` from content keywords.
///
/// Uses simple keyword matching against lowercase content.
/// Falls back to `Custom("general")` when no specific category matches.
///
/// This is a local copy of `symbiotic_daemon::detect_blob_category()` which
/// is `pub(crate)` and not accessible from the CLI crate.
fn detect_blob_category(content: &str) -> BlobCategory {
    let lower = content.to_ascii_lowercase();

    // Medical keywords
    if lower.contains("diagnosis")
        || lower.contains("prescription")
        || lower.contains("bloodwork")
        || lower.contains("medical record")
        || lower.contains("patient")
        || lower.contains("cholesterol")
        || lower.contains("hemoglobin")
        || lower.contains("radiology")
        || lower.contains("pathology")
        || lower.contains("icd-10")
        || lower.contains("hipaa")
    {
        return BlobCategory::Medical;
    }

    // Financial keywords
    if lower.contains("tax return")
        || lower.contains("bank statement")
        || lower.contains("invoice")
        || lower.contains("balance sheet")
        || lower.contains("account number")
        || lower.contains("routing number")
        || lower.contains("1099")
        || lower.contains("w-2")
        || lower.contains("irs")
        || lower.contains("portfolio")
        || lower.contains("brokerage")
    {
        return BlobCategory::Financial;
    }

    // Legal keywords
    if lower.contains("contract")
        || lower.contains("non-disclosure")
        || lower.contains("nda")
        || lower.contains("power of attorney")
        || lower.contains("last will")
        || lower.contains("subpoena")
        || lower.contains("affidavit")
        || lower.contains("plaintiff")
        || lower.contains("defendant")
    {
        return BlobCategory::Legal;
    }

    // Credential keywords
    if lower.contains("password")
        || lower.contains("api_key")
        || lower.contains("secret_key")
        || lower.contains("-----begin private key-----")
        || lower.contains("-----begin rsa private key-----")
        || lower.contains("access_token")
    {
        return BlobCategory::Credential;
    }

    BlobCategory::Custom("general".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_ignores_empty_and_comments() {
        let lines =
            split_lines("\n# comment\nhttps://a.example\n   \nhttps://b.example\n".to_string());
        assert_eq!(
            lines,
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
    }

    #[test]
    fn collect_url_inputs_rejects_multiple_sources() {
        let err = collect_url_inputs(Some("https://example.com".to_string()), None, true)
            .expect_err("must reject multiple sources");
        assert!(err
            .to_string()
            .contains("choose exactly one URL input source"));
    }

    #[test]
    fn collect_url_inputs_accepts_positional_url() {
        let urls = collect_url_inputs(Some("https://example.com".to_string()), None, false)
            .expect("valid input");
        assert_eq!(urls, vec!["https://example.com".to_string()]);
    }

    #[test]
    fn detect_blob_category_medical() {
        assert_eq!(
            detect_blob_category("Patient diagnosis: acute bronchitis"),
            BlobCategory::Medical
        );
    }

    #[test]
    fn detect_blob_category_financial() {
        assert_eq!(
            detect_blob_category("2025 Tax Return - Federal"),
            BlobCategory::Financial
        );
    }

    #[test]
    fn detect_blob_category_legal() {
        assert_eq!(
            detect_blob_category("Non-Disclosure Agreement between parties"),
            BlobCategory::Legal
        );
    }

    #[test]
    fn detect_blob_category_credential() {
        assert_eq!(
            detect_blob_category("api_key=sk-12345abcdef"),
            BlobCategory::Credential
        );
    }

    #[test]
    fn detect_blob_category_general_fallback() {
        assert_eq!(
            detect_blob_category("My personal thoughts on life"),
            BlobCategory::Custom("general".to_string())
        );
    }

    #[test]
    fn is_already_migrated_by_title() {
        let doc = symbiotic_archive::ArchiveDocument {
            record_id: "test".to_string(),
            idempotency_key: "key".to_string(),
            title: "[encrypted] Some Title".to_string(),
            source_url: None,
            tags: vec![],
            sensitivity: ArchiveSensitivity::Private,
            updated_at: 0,
            content: "placeholder".to_string(),
        };
        assert!(is_already_migrated(&doc));
    }

    #[test]
    fn is_already_migrated_by_tag() {
        let doc = symbiotic_archive::ArchiveDocument {
            record_id: "test".to_string(),
            idempotency_key: "key".to_string(),
            title: "Some Title".to_string(),
            source_url: None,
            tags: vec!["tier3/encrypted".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            updated_at: 0,
            content: "placeholder".to_string(),
        };
        assert!(is_already_migrated(&doc));
    }

    #[test]
    fn is_already_migrated_by_content() {
        let doc = symbiotic_archive::ArchiveDocument {
            record_id: "test".to_string(),
            idempotency_key: "key".to_string(),
            title: "Some Title".to_string(),
            source_url: None,
            tags: vec![],
            sensitivity: ArchiveSensitivity::Private,
            updated_at: 0,
            content: "---\nblob_id: test\nstatus: encrypted\n---\n".to_string(),
        };
        assert!(is_already_migrated(&doc));
    }

    #[test]
    fn not_migrated_fresh_entry() {
        let doc = symbiotic_archive::ArchiveDocument {
            record_id: "test".to_string(),
            idempotency_key: "key".to_string(),
            title: "My Medical Record".to_string(),
            source_url: None,
            tags: vec!["personal".to_string()],
            sensitivity: ArchiveSensitivity::Private,
            updated_at: 0,
            content: "Patient diagnosis: acute bronchitis".to_string(),
        };
        assert!(!is_already_migrated(&doc));
    }

    #[test]
    fn migration_report_initializes_empty() {
        let report = MigrationReport::new();
        assert_eq!(report.scanned, 0);
        assert_eq!(report.migrated, 0);
        assert_eq!(report.skipped, 0);
        assert!(report.errors.is_empty());
    }
}
