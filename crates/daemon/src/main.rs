use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

mod adapter;
mod config;
mod loops;
mod server;
mod session;
mod storage;
mod worktree;

use agentd_protocol::paths::Paths;

#[derive(Debug, Parser)]
#[command(name = "agentd", about = "agentd daemon", version)]
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

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info,agentd=debug,agentd_protocol=info"))
        .unwrap();
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Run { socket: None }) {
        Command::Run { socket } => run(socket).await,
        Command::Paths => {
            let p = Paths::discover();
            println!("config:  {}", p.config_dir.display());
            println!("state:   {}", p.state_dir.display());
            println!("data:    {}", p.data_dir.display());
            println!("runtime: {}", p.runtime_dir.display());
            println!("socket:  {}", p.socket().display());
            Ok(())
        }
        Command::DefaultConfig => {
            println!("{}", config::DEFAULT_CONFIG_TOML);
            Ok(())
        }
    }
}

async fn run(socket_override: Option<PathBuf>) -> Result<()> {
    let paths = Paths::discover();
    std::fs::create_dir_all(&paths.state_dir).ok();
    std::fs::create_dir_all(&paths.data_dir).ok();
    std::fs::create_dir_all(&paths.runtime_dir).ok();
    std::fs::create_dir_all(&paths.config_dir).ok();

    let config = config::Config::load_or_default(&paths)?;
    tracing::info!(
        adapters = config.adapters.len(),
        config_dir = %paths.config_dir.display(),
        "loaded config"
    );

    let storage = Arc::new(storage::Storage::new(paths.data_dir.clone())?);
    let manager = Arc::new(
        session::SessionManager::new(storage.clone(), Arc::new(config))
            .await
            .context("init session manager")?,
    );
    // Best-effort resume: re-spawn adapters for sessions that were alive at
    // the previous shutdown. Sessions whose adapter binary is missing or
    // whose start params can't be loaded get marked Errored. Logs only;
    // never fatal.
    manager.clone().resume_running_sessions().await;
    // Best-effort: create the orchestrator session if config enables
    // one and no orchestrator exists yet. Logged-only on failure (e.g.
    // chosen harness missing or no API key); clients fall back to the
    // static palette in that case.
    manager.clone().ensure_orchestrator().await;
    // Loop scheduler: wakes every second, fires due loops by
    // calling `SessionManager::send_input`. Persisted per-session
    // in `sessions/<id>/loops.json`; daemon restart picks them
    // back up.
    {
        let mgr = manager.clone();
        let loops = mgr.loops.clone();
        tokio::spawn(async move {
            loops::run_scheduler(mgr, loops).await;
        });
    }

    let socket_path = socket_override.unwrap_or_else(|| paths.socket());
    server::serve(manager, socket_path).await
}
