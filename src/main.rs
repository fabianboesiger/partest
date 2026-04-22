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
            ssh_key,
            release,
            batch_size,
            jobs_per_worker,
            cargo_test_args,
        } => {
            run::run(&ssh_key, release, batch_size, jobs_per_worker, &cargo_test_args).await?;
        }
        Command::Status => {
            status::show_status()?;
        }
    }

    Ok(())
}

mod tests {
    #[test]
    fn example_test1() {}
    #[test]
    fn example_test2() {}
    #[test]
    fn example_test3() {}
    #[test]
    fn example_test4() {}
    #[test]
    fn example_test5() {}
}
