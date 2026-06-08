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
mod upgrade;

use agentd_client::Client;
use agentd_protocol::paths::Paths;

#[derive(Debug, Parser)]
#[command(
    name = "construct",
    about = "construct: TUI client and daemon for the agent fleet",
    version
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
    /// Run the construct daemon (session supervisor + IPC server).
    ///
    /// One installed `construct` binary runs both the client and the daemon;
    /// the TUI also auto-starts a daemon when none is running, so you rarely
    /// need to invoke this directly (mainly servers / process supervisors).
    Daemon {
        #[command(subcommand)]
        command: Option<DaemonCommand>,
    },
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
    Send { session_id: String, text: String },
    /// Internal: `PreToolUse` hook body for the AskUserQuestion chat-gate.
    /// Reads the hook payload on stdin; if a chat viewer is active for
    /// `$CONSTRUCT_SESSION_ID`, prints a `deny` decision that degrades Claude's
    /// picker to a plain-text question. Fails open (allow) on any error.
    #[command(hide = true)]
    AskGate,
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
    /// Download and install the latest release (or `--version TAG`),
    /// atomically replacing the installed binaries in place.
    Upgrade {
        /// Install a specific release tag (e.g. v0.2.0). Default: latest.
        #[arg(long)]
        version: Option<String>,
        /// Install directory. Default: the directory of the running `construct`.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
        /// After upgrading, ask the running daemon to restart so the new
        /// binary takes effect immediately.
        #[arg(long)]
        restart: bool,
        /// Don't install — just report the current and latest versions.
        #[arg(long)]
        check: bool,
    },
}

/// Subcommands of `construct daemon`.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground (default).
    Run,
    /// Print resolved paths and exit.
    Paths,
    /// Print the embedded default config and exit.
    DefaultConfig,
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("warn"))
        .unwrap();
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Tui);

    // Daemon mode runs the supervisor in-process via the shared `agentd`
    // library. Handled before the client tracing init so the daemon's verbose filter
    // applies, and before socket discovery (the daemon *owns* the socket
    // rather than connecting to it). The daemon's restart self-`exec()`
    // replays this argv (`construct daemon run …`) verbatim, so picking up an
    // upgraded binary keeps working.
    if let Command::Daemon { command: daemon_cmd } = command {
        agentd::init_tracing();
        return match daemon_cmd.unwrap_or(DaemonCommand::Run) {
            DaemonCommand::Run => agentd::run(cli.socket).await,
            DaemonCommand::Paths => {
                agentd::print_paths();
                Ok(())
            }
            DaemonCommand::DefaultConfig => {
                println!("{}", agentd::DEFAULT_CONFIG_TOML);
                Ok(())
            }
        };
    }

    init_tracing();
    let socket = cli.socket.unwrap_or_else(|| Paths::discover().socket());

    match command {
        Command::Tui => run_tui(socket).await,
        // Handled above (early return); listed for match exhaustiveness.
        Command::Daemon { .. } => unreachable!("daemon mode handled before this match"),
        Command::Paths => {
            let p = Paths::discover();
            println!("config:  {}", p.config_dir.display());
            println!("state:   {}", p.state_dir.display());
            println!("data:    {}", p.data_dir.display());
            println!("runtime: {}", p.runtime_dir.display());
            println!("socket:  {}", p.socket().display());
            println!("webui:   {}", agentd_protocol::paths::local_webui_url());
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
                    prompt: if prompt.trim().is_empty() {
                        None
                    } else {
                        Some(prompt)
                    },
                    model,
                    title,
                    mode,
                    pty_size: None,
                    worktree,
                    env: Default::default(),
                    args: Vec::new(),
                    kind: agentd_protocol::SessionKind::User,
                    parent_session_id: None,
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
        Command::AskGate => {
            // Drain stdin (the PreToolUse payload) so the hook's pipe closes
            // cleanly, then resolve the session id from the adapter-set env
            // (preferred) or the payload.
            use std::io::Read as _;
            let mut buf = String::new();
            let _ = std::io::stdin().read_to_string(&mut buf);
            let session_id = std::env::var("CONSTRUCT_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    serde_json::from_str::<serde_json::Value>(&buf)
                        .ok()
                        .and_then(|v| {
                            v.get("session_id")
                                .and_then(|s| s.as_str())
                                .map(String::from)
                        })
                });
            // Fail open: deny *only* when we positively confirm a chat viewer.
            // Any missing piece, connect error, or query error → allow (print
            // nothing) so we never strand the model.
            if let Some(sid) = session_id {
                if let Ok(c) = connect(&socket).await {
                    if c.chat_viewer_active(&sid).await.unwrap_or(false) {
                        let deny = serde_json::json!({
                            "hookSpecificOutput": {
                                "hookEventName": "PreToolUse",
                                "permissionDecision": "deny",
                                "permissionDecisionReason": "The AskUserQuestion interactive picker isn't available in the active chat view. Ask your question as a plain text message, listing the options inline (e.g. \"1) ...  2) ...\"), and wait for the user's text reply."
                            }
                        });
                        println!("{deny}");
                    }
                }
            }
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
        Command::Upgrade {
            version,
            bin_dir,
            restart,
            check,
        } => upgrade::run(version, bin_dir, restart, check, &socket).await,
    }
}

async fn connect(socket: &std::path::Path) -> Result<std::sync::Arc<Client>> {
    Client::connect(socket).await.with_context(|| {
        format!(
            "connect to daemon at {} (start one with `construct daemon run`)",
            socket.display()
        )
    })
}

async fn run_tui(socket: PathBuf) -> Result<()> {
    // The TUI is the default `construct` entry point, so make it "just work"
    // when no daemon is running yet: auto-start one in the background.
    ensure_daemon_running(&socket).await;
    app::run_with_socket(socket).await
}

/// Ensure a daemon is listening on `socket`, auto-starting one in the
/// background if not. Best-effort: on any failure we fall through and let
/// `run_with_socket`'s own connect surface the original error. Set
/// `CONSTRUCT_NO_AUTOSTART=1` to opt out (e.g. scripts that manage the daemon
/// themselves). Concurrent auto-starts are safe — the daemon's single-instance
/// lock lets only one survive.
async fn ensure_daemon_running(socket: &std::path::Path) {
    use std::time::Duration;

    if socket_is_live(socket) {
        return;
    }
    if std::env::var("CONSTRUCT_NO_AUTOSTART").as_deref() == Ok("1") {
        return;
    }
    if let Err(e) = agentd::spawn_detached_daemon(Some(socket)) {
        tracing::warn!(error = %e, "failed to auto-start construct daemon");
        return;
    }
    tracing::info!(socket = %socket.display(), "no daemon running; auto-started one");

    // The daemon binds the socket early in startup; poll for readiness (~5s).
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if socket_is_live(socket) {
            return;
        }
    }
    tracing::warn!(socket = %socket.display(), "auto-started daemon not ready yet; continuing");
}

/// Cheap readiness probe: can we open the IPC socket? A stale socket file
/// (the daemon is gone) fails to connect, so this correctly reports "not live".
fn socket_is_live(socket: &std::path::Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}
