use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod app;
mod keymap;
mod matrix_rain;
mod pty_render;
mod theme;
mod tui_state;
mod ui;

use agentd_client::Client;
use agentd_protocol::paths::Paths;

#[derive(Debug, Parser)]
#[command(
    name = "agent",
    about = "agent: TUI client for agentd",
    version,
)]
struct Cli {
    /// Override the daemon socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the TUI (default).
    Tui,
    /// Print resolved paths.
    Paths,
    /// Ping the daemon.
    Ping,
    /// List registered harnesses.
    Harnesses,
    /// List sessions.
    #[command(visible_alias = "ls")]
    List,
    /// Create a new session.
    New {
        /// Harness name (shell, claude, codex, …).
        harness: String,
        /// Initial prompt (empty = interactive PTY for adapters that support it).
        #[arg(default_value = "")]
        prompt: String,
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Session mode hint (e.g. "interactive" / "headless"); adapter-defined.
        #[arg(long)]
        mode: Option<String>,
        #[arg(long, default_value_t = false)]
        worktree: bool,
    },
    /// Send input to a session.
    Send {
        session_id: String,
        text: String,
    },
    /// Stop a session cleanly.
    Stop { session_id: String },
    /// Force-kill a session (SIGKILL the adapter; keeps the record errored).
    Kill { session_id: String },
    /// Delete a session entirely (kill if running, remove transcript + worktree).
    #[command(visible_alias = "rm")]
    Delete { session_id: String },
    /// Pin a session so it's always shown as a live tile in the TUI pin strip.
    Pin { session_id: String },
    /// Unpin a session.
    Unpin { session_id: String },
    /// Rename a session — sets the user-facing title (shown instead of the
    /// session hash). Pass `--clear` to remove the title and fall back to
    /// the hash.
    Rename {
        session_id: String,
        /// New title. Omit when using `--clear`.
        #[arg(required_unless_present = "clear")]
        title: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// Show diff of session's working tree.
    Diff { session_id: String },
    /// Show session detail + transcript.
    Show { session_id: String },
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().or_else(|_| EnvFilter::try_new("warn")).unwrap();
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(|| Paths::discover().socket());

    match cli.command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(socket).await,
        Command::Paths => {
            let p = Paths::discover();
            println!("config:  {}", p.config_dir.display());
            println!("state:   {}", p.state_dir.display());
            println!("data:    {}", p.data_dir.display());
            println!("runtime: {}", p.runtime_dir.display());
            println!("socket:  {}", p.socket().display());
            Ok(())
        }
        Command::Ping => {
            let c = connect(&socket).await?;
            let r = c.ping().await?;
            println!("pong: {}, version: {}", r.pong, r.version);
            Ok(())
        }
        Command::Harnesses => {
            let c = connect(&socket).await?;
            let list = c.harnesses().await?;
            for h in list {
                let status = if h.available { "ok" } else { "missing" };
                let bin = h.binary.as_deref().unwrap_or("?");
                println!(
                    "{name:<10} [{status}]  {bin}\n           {desc}",
                    name = h.name,
                    status = status,
                    bin = bin,
                    desc = h.description.unwrap_or_default()
                );
            }
            Ok(())
        }
        Command::List => {
            let c = connect(&socket).await?;
            let list = c.list().await?;
            if list.is_empty() {
                println!("(no sessions)");
                return Ok(());
            }
            for s in list {
                println!(
                    "{glyph} {id}  {harness:<7}  {state:<14}  {cwd}{title}",
                    glyph = s.state.glyph(),
                    id = &s.id[..s.id.len().min(10)],
                    harness = s.harness,
                    state = s.state.label(),
                    cwd = s.cwd,
                    title = s
                        .title
                        .as_ref()
                        .map(|t| format!("  — {t}"))
                        .unwrap_or_default(),
                );
            }
            Ok(())
        }
        Command::New {
            harness,
            prompt,
            cwd,
            model,
            title,
            mode,
            worktree,
        } => {
            let c = connect(&socket).await?;
            let cwd = std::fs::canonicalize(&cwd)
                .with_context(|| format!("resolve cwd {}", cwd.display()))?
                .to_string_lossy()
                .to_string();
            let id = c
                .create(agentd_protocol::CreateSessionParams {
                    harness,
                    cwd,
                    prompt: if prompt.trim().is_empty() { None } else { Some(prompt) },
                    model,
                    title,
                    mode,
                    pty_size: None,
                    worktree,
                    env: Default::default(),
                    args: Vec::new(),
                    kind: agentd_protocol::SessionKind::User,
                    group_id: None,
                })
                .await?;
            println!("{id}");
            Ok(())
        }
        Command::Send { session_id, text } => {
            let c = connect(&socket).await?;
            c.send_input(&session_id, text).await?;
            Ok(())
        }
        Command::Stop { session_id } => {
            let c = connect(&socket).await?;
            c.stop(&session_id).await?;
            Ok(())
        }
        Command::Kill { session_id } => {
            let c = connect(&socket).await?;
            c.kill(&session_id).await?;
            Ok(())
        }
        Command::Delete { session_id } => {
            let c = connect(&socket).await?;
            c.delete(&session_id).await?;
            Ok(())
        }
        Command::Pin { session_id } => {
            let c = connect(&socket).await?;
            c.set_pinned(&session_id, true).await?;
            Ok(())
        }
        Command::Unpin { session_id } => {
            let c = connect(&socket).await?;
            c.set_pinned(&session_id, false).await?;
            Ok(())
        }
        Command::Rename {
            session_id,
            title,
            clear,
        } => {
            let c = connect(&socket).await?;
            let new_title = if clear { None } else { title };
            c.set_title(&session_id, new_title).await?;
            Ok(())
        }
        Command::Diff { session_id } => {
            let c = connect(&socket).await?;
            let r = c.diff(&session_id).await?;
            if r.patch.is_empty() {
                println!("(no diff)");
            } else {
                print!("{}", r.patch);
            }
            Ok(())
        }
        Command::Show { session_id } => {
            let c = connect(&socket).await?;
            let d = c.get(&session_id).await?;
            println!(
                "{id}  {harness}  {state}  {cwd}",
                id = d.summary.id,
                harness = d.summary.harness,
                state = d.summary.state.label(),
                cwd = d.summary.cwd
            );
            for ev in &d.events {
                println!(
                    "  [{ts}] #{seq} {evt}",
                    ts = ev.at.format("%H:%M:%S"),
                    seq = ev.seq,
                    evt = ui::short_event_label(&ev.event)
                );
            }
            Ok(())
        }
    }
}

async fn connect(socket: &std::path::Path) -> Result<std::sync::Arc<Client>> {
    Client::connect(socket).await.with_context(|| {
        format!(
            "connect to daemon at {} (is `agentd` running?)",
            socket.display()
        )
    })
}

async fn run_tui(socket: PathBuf) -> Result<()> {
    let client = connect(&socket).await?;
    app::run(client).await
}
