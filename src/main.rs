mod cli;
mod daemon;
mod discovery;
mod run;
mod ssh;
mod status;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Daemon { ssh_port } => {
            daemon::run_daemon(ssh_port).await?;
        }
        Command::Run {
            ssh_user,
            ssh_key,
            release,
            nextest_args,
        } => {
            run::run(&ssh_user, &ssh_key, release, &nextest_args).await?;
        }
        Command::Status => {
            status::show_status()?;
        }
    }

    Ok(())
}

#[test]
fn example_test() {}