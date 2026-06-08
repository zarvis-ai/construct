//! `constructd` — standalone daemon binary.
//!
//! Thin shim over the `agentd` library crate, kept as a back-compat alias for
//! the unified `construct daemon …` entry point. Both share the same
//! [`agentd::run`] code path; this binary simply parses the legacy
//! `constructd <subcommand>` CLI and dispatches.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "constructd", about = "construct daemon", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the daemon in the foreground (default).
    Run {
        /// Override the socket path.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Print resolved paths and exit.
    Paths,
    /// Print the embedded default config and exit.
    DefaultConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    agentd::init_tracing();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run { socket: None }) {
        Command::Run { socket } => agentd::run(socket).await,
        Command::Paths => {
            agentd::print_paths();
            Ok(())
        }
        Command::DefaultConfig => {
            println!("{}", agentd::DEFAULT_CONFIG_TOML);
            Ok(())
        }
    }
}
