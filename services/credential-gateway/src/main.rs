use std::collections::HashSet;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use credential_gateway::{
    auth_engine::{
        encode_worker_result, run_auth_sandbox_worker, AuthSandboxWorkerConfig,
        WorkerRequestEnvelope,
    },
    now_unix, AuthRequest, CredentialGateway, CredentialRecord, FileCredentialVault, GatewayConfig,
    SessionPolicy, SessionType, StaticThreatChecker,
};

#[derive(Debug, Parser)]
#[command(name = "credential-gateway")]
#[command(about = "Credential gateway utility")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Put {
        #[arg(long)]
        vault_file: String,
        #[arg(long)]
        service: String,
        #[arg(long)]
        username: String,
    },
    Issue {
        #[arg(long)]
        vault_file: String,
        #[arg(long)]
        target: String,
        #[arg(long, value_delimiter = ',')]
        scopes: Vec<String>,
        #[arg(long, default_value = "browser")]
        session_type: String,
    },
    #[command(hide = true)]
    AuthSandboxRun {
        #[arg(long)]
        vault_root: PathBuf,
        #[arg(long)]
        scripts_dir: PathBuf,
        #[arg(long)]
        domain: String,
        #[arg(long, default_value_t = 60)]
        timeout_secs: u64,
        #[arg(long)]
        node_bin: Option<PathBuf>,
        #[arg(long)]
        goal_scope: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Put {
            vault_file,
            service,
            username,
        } => {
            eprint!("Enter secret: ");
            let mut buf = String::new();
            std::io::stdin().read_line(&mut buf)?;
            let secret = buf.trim_end().to_string();
            let gateway = open_gateway(&vault_file)?;
            gateway.put_credential(CredentialRecord {
                service,
                username,
                secret,
                totp_secret: None,
            })?;
            println!("ok credential_stored");
        }
        Command::Issue {
            vault_file,
            target,
            scopes,
            session_type,
        } => {
            let gateway = open_gateway(&vault_file)?;
            let session_type = parse_session_type(&session_type);
            let handle = gateway.issue_session_handle(
                AuthRequest {
                    target,
                    scopes,
                    session_type,
                    policy: SessionPolicy {
                        exportable: false,
                        requires_reauth: false,
                    },
                },
                now_unix(),
            )?;
            println!(
                "ok handle_id={} expires_at={} target={}",
                handle.handle_id, handle.expires_at, handle.target
            );
        }
        Command::AuthSandboxRun {
            vault_root,
            scripts_dir,
            domain,
            timeout_secs,
            node_bin,
            goal_scope,
        } => {
            let mut request_body = String::new();
            std::io::stdin().read_to_string(&mut request_body)?;
            let request = if request_body.trim().is_empty() {
                WorkerRequestEnvelope::default()
            } else {
                serde_json::from_str(&request_body)?
            };
            let output = encode_worker_result(
                run_auth_sandbox_worker(
                    AuthSandboxWorkerConfig {
                        vault_root,
                        scripts_dir,
                        timeout: std::time::Duration::from_secs(timeout_secs),
                        node_bin,
                        goal_scope,
                    },
                    &domain,
                    request.continuation,
                )
                .await,
            )?;
            println!("{output}");
        }
    }
    Ok(())
}

fn open_gateway(vault_file: &str) -> Result<CredentialGateway> {
    let vault = Arc::new(FileCredentialVault::open(vault_file)?);
    let checker = Arc::new(StaticThreatChecker::new(HashSet::new()));
    Ok(CredentialGateway::new(
        GatewayConfig::default(),
        checker,
        vault,
    ))
}

fn parse_session_type(input: &str) -> SessionType {
    match input {
        "api" => SessionType::Api,
        _ => SessionType::Browser,
    }
}
