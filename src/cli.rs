use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "partest", about = "Distributed cargo test runner")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the partest daemon (advertises this machine on the local network)
    Daemon {
        /// SSH port to advertise for remote connections
        #[arg(long, default_value_t = 22)]
        ssh_port: u16,
    },

    /// Run tests distributed across all discovered peers
    Run {
        /// Path to SSH private key
        #[arg(long, default_value_t = default_ssh_key())]
        ssh_key: String,

        /// Build and run tests in release mode
        #[arg(long)]
        release: bool,

        /// Number of tests per batch dispatched to each worker slot (default: auto)
        #[arg(long)]
        batch_size: Option<usize>,

        /// Concurrent batch slots per worker (default: auto from worker CPU count)
        #[arg(long)]
        jobs_per_worker: Option<usize>,

        /// Extra arguments forwarded to `cargo test`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        cargo_test_args: Vec<String>,
    },

    /// List discovered peers on the local network
    Status,
}

fn default_ssh_key() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "~".into());
    format!("{home}/.ssh/id_ed25519")
}
