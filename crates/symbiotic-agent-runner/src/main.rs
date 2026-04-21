use anyhow::Result;
use clap::Parser;

use symbiotic_agent_runner::{init_tracing, run, RunnerArgs};

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let args = RunnerArgs::parse();
    if let Some(result) = run(args).await? {
        println!("{}", result.output);
    }

    Ok(())
}
