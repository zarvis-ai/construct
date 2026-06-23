use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::path::PathBuf;

mod acp;
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
    /// Fork a session into a new sibling session backed by a (possibly
    /// different) harness. The fork inherits the source's cwd and group and,
    /// unless `--no-seed`, is seeded with a summary of the source transcript
    /// so an agent harness can continue the prior context. The original
    /// session is left untouched.
    Fork {
        /// Source session id to fork from.
        session_id: String,
        /// Harness for the new session (shell, claude, codex, smith, …).
        #[arg(long)]
        harness: String,
        /// Model spec for the new session (defaults to the harness default).
        #[arg(long)]
        model: Option<String>,
        /// Extra instruction appended after the seeded context.
        #[arg(long)]
        prompt: Option<String>,
        /// Don't seed the fork with the source transcript.
        #[arg(long, default_value_t = false)]
        no_seed: bool,
        /// Cap the seeded transcript at N bytes (0 = unlimited / full
        /// transcript, the default). When exceeded, the opening (objective)
        /// and most-recent activity are kept and the middle is elided.
        #[arg(long, default_value_t = 0)]
        max_seed_bytes: usize,
    },
    /// Run `construct` as an Agent Client Protocol stdio server.
    Acp {
        /// Default harness when `session/new` omits `harness`.
        #[arg(long)]
        harness: Option<String>,
        /// Default model when `session/new` omits `model`.
        #[arg(long)]
        model: Option<String>,
        /// Default working directory for `session/new`.
        #[arg(long, default_value = ".")]
        cwd: PathBuf,
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
    /// Run the MCP stdio server (internal).
    #[command(name = "__mcp", hide = true)]
    Mcp,
    /// Run an adapter (internal, spawned by the daemon).
    #[command(name = "__adapter", hide = true)]
    Adapter {
        #[command(subcommand)]
        adapter: AdapterCommand,
    },
}

/// Subcommands of `construct daemon`.
#[derive(Debug, Subcommand)]
enum DaemonCommand {
    /// Run the daemon in the foreground (default).
    Run,
    /// Start the daemon in the background, if not already running.
    Start,
    /// Stop the running daemon. Adapters are stopped but sessions stay
    /// resumable on the next start.
    Stop {
        /// Explicitly stop session adapters before daemon exit. This is the
        /// default `daemon stop` behavior; the flag is accepted for symmetry
        /// with `daemon restart --sessions`.
        #[arg(long)]
        sessions: bool,
    },
    /// Restart the running daemon in place (or start one if none is
    /// running). Sessions are preserved and resume after the restart.
    Restart {
        /// Also restart every session's adapter process (and its
        /// `construct-mcp` child). Sessions are preserved/resumed —
        /// they are neither archived nor deleted. Without this flag the
        /// adapters survive the restart and reattach, so their MCP
        /// children are not restarted.
        #[arg(long)]
        sessions: bool,
    },
    /// Print resolved paths and exit.
    Paths,
    /// Print the embedded default config and exit.
    DefaultConfig,
}

#[derive(Debug, Subcommand)]
enum AdapterCommand {
    #[command(hide = true)]
    Shell,
    #[command(hide = true)]
    Claude,
    #[command(hide = true)]
    Codex,
    #[command(hide = true)]
    Antigravity,
    #[command(hide = true)]
    Smith {
        /// Auto-title mode: generate a short title for the given prompt.
        #[arg(long)]
        title_mode: Option<String>,
    },
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
    let raw_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let cli = Cli::parse();
    let command = cli.command.unwrap_or(Command::Tui);

    // Daemon mode runs the supervisor in-process via the shared `agentd`
    // library. Handled before the client tracing init so the daemon's verbose filter
    // applies, and before socket discovery (the daemon *owns* the socket
    // rather than connecting to it). The daemon's restart self-`exec()`
    // replays this argv (`construct daemon run …`) verbatim, so picking up an
    // upgraded binary keeps working.
    if let Command::Daemon {
        command: daemon_cmd,
    } = command
    {
        agentd::init_tracing();
        return match daemon_cmd.unwrap_or(DaemonCommand::Run) {
            DaemonCommand::Run => agentd::run(cli.socket).await,
            DaemonCommand::Start => daemon_start(cli.socket).await,
            DaemonCommand::Stop { sessions } => daemon_stop(cli.socket, sessions).await,
            DaemonCommand::Restart { sessions } => daemon_restart_cmd(cli.socket, sessions).await,
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

    if command_allows_upgrade_prompt(&command) {
        if let Some(exe) = upgrade::prompt_and_upgrade_if_available(&socket).await? {
            return reexec_upgraded_construct(exe, &raw_args);
        }
    }

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
                    position_after_session_id: None,
                })
                .await?;
            println!("{id}");
            Ok(())
        }
        Command::Fork {
            session_id,
            harness,
            model,
            prompt,
            no_seed,
            max_seed_bytes,
        } => {
            let c = connect(&socket).await?;
            let id = c
                .fork_session(
                    &session_id,
                    &harness,
                    agentd_client::ForkOptions {
                        model,
                        prompt,
                        seed: !no_seed,
                        max_seed_bytes,
                        pty_size: None,
                    },
                )
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
        Command::Acp {
            harness,
            model,
            cwd,
        } => {
            let cwd = std::fs::canonicalize(&cwd)
                .with_context(|| format!("resolve cwd {}", cwd.display()))?;
            ensure_daemon_running(&socket).await;
            acp::run(socket, harness, model, cwd).await
        }
        Command::Mcp => {
            construct_mcp::run().await?;
            Ok(())
        }
        Command::Adapter { adapter } => match adapter {
            AdapterCommand::Shell => {
                construct_adapter_shell::run().await?;
                Ok(())
            }
            AdapterCommand::Claude => {
                construct_adapter_claude::run().await?;
                Ok(())
            }
            AdapterCommand::Codex => {
                construct_adapter_codex::run().await?;
                Ok(())
            }
            AdapterCommand::Antigravity => {
                construct_adapter_antigravity::run().await?;
                Ok(())
            }
            AdapterCommand::Smith {
                title_mode: Some(prompt),
            } => {
                construct_adapter_smith::run_title_mode(&prompt).await?;
                Ok(())
            }
            AdapterCommand::Smith { title_mode: None } => {
                construct_adapter_smith::run().await?;
                Ok(())
            }
        },
    }
}

fn command_allows_upgrade_prompt(command: &Command) -> bool {
    !matches!(
        command,
        Command::Daemon { .. }
            | Command::Upgrade { .. }
            | Command::Acp { .. }
            | Command::AskGate
            | Command::Mcp
            | Command::Adapter { .. }
    )
}

#[cfg(unix)]
fn reexec_upgraded_construct(exe: PathBuf, args: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let err = std::process::Command::new(&exe).args(args).exec();
    Err(anyhow::Error::new(err).context(format!("re-exec upgraded construct at {}", exe.display())))
}

#[cfg(not(unix))]
fn reexec_upgraded_construct(exe: PathBuf, _args: &[OsString]) -> Result<()> {
    println!(
        "Upgraded. Run {} again to continue with the new binary.",
        exe.display()
    );
    Ok(())
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

/// Resolve the daemon socket the same way the client commands do: honor
/// `--socket`, else the discovered default.
fn resolve_socket(socket_override: Option<PathBuf>) -> PathBuf {
    socket_override.unwrap_or_else(|| Paths::discover().socket())
}

/// Poll `socket_is_live` until it reaches `want`, up to `tries` attempts
/// spaced 100ms apart. Returns true if the desired state was reached.
async fn poll_socket(socket: &std::path::Path, want: bool, tries: u32) -> bool {
    use std::time::Duration;
    for _ in 0..tries {
        if socket_is_live(socket) == want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    socket_is_live(socket) == want
}

/// `construct daemon start`: spawn a detached background daemon if one
/// isn't already listening, then wait for it to bind the socket.
/// Idempotent — succeeds as a no-op when a daemon already owns the socket.
async fn daemon_start(socket_override: Option<PathBuf>) -> Result<()> {
    let socket = resolve_socket(socket_override);
    if socket_is_live(&socket) {
        println!("construct daemon already running ({})", socket.display());
        return Ok(());
    }
    agentd::spawn_detached_daemon(Some(&socket))
        .with_context(|| format!("spawn detached daemon for {}", socket.display()))?;
    // The daemon binds the socket early in startup; poll for readiness (~5s).
    if poll_socket(&socket, true, 50).await {
        println!("construct daemon started ({})", socket.display());
        Ok(())
    } else {
        anyhow::bail!("construct daemon did not become ready within 5s; check the daemon log")
    }
}

/// `construct daemon stop`: ask the running daemon to stop its adapters
/// (leaving sessions resumable on the next start) and exit. `--sessions` is
/// accepted for symmetry with `daemon restart --sessions`; stop already drains
/// adapters either way. Idempotent — succeeds as a no-op when no daemon is
/// running.
async fn daemon_stop(socket_override: Option<PathBuf>, sessions: bool) -> Result<()> {
    let socket = resolve_socket(socket_override);
    if !socket_is_live(&socket) {
        println!("construct daemon is not running ({})", socket.display());
        return Ok(());
    }
    let client = connect(&socket).await?;
    // The daemon closes the IPC connection as it exits, so a broken-pipe
    // error here means the shutdown is in flight — both outcomes count as
    // success.
    match client.daemon_shutdown().await {
        Ok(r) => tracing::debug!(pid = r.pid, "daemon acknowledged shutdown"),
        Err(e) => tracing::debug!(error = %e, "shutdown reply lost (daemon already exiting)"),
    }
    // Wait for the socket to go dead so the command only returns once the
    // daemon is actually gone. Adapter teardown is bounded but can be slow
    // (a few seconds per wedged adapter), so allow a generous window.
    if poll_socket(&socket, false, 200).await {
        if sessions {
            println!(
                "construct daemon stopped; sessions are resumable ({})",
                socket.display()
            );
        } else {
            println!("construct daemon stopped ({})", socket.display());
        }
        Ok(())
    } else {
        anyhow::bail!("construct daemon shutdown requested but it is still running after 20s")
    }
}

/// `construct daemon restart`: restart the running daemon in place, or
/// start one if none is running. With `sessions = true`, every session's
/// adapter (and its `construct-mcp` child) is also restarted; sessions are
/// preserved and resume either way.
async fn daemon_restart_cmd(socket_override: Option<PathBuf>, sessions: bool) -> Result<()> {
    let socket = resolve_socket(socket_override);
    if !socket_is_live(&socket) {
        println!("no construct daemon running; starting one");
        return daemon_start(Some(socket)).await;
    }
    let client = connect(&socket).await?;
    // The daemon re-execs and drops this connection, so a broken-pipe
    // error here means the restart is in flight — both count as success.
    match client.daemon_restart(None, sessions).await {
        Ok(r) => tracing::debug!(exe = %r.exe, pid = r.pid, "daemon acknowledged restart"),
        Err(e) => tracing::debug!(error = %e, "restart reply lost (daemon already re-exec'ing)"),
    }
    // The daemon re-execs in place on the same socket. With `--sessions`
    // it stops every adapter first (bounded but slow), so poll generously
    // until a daemon is reachable again.
    if poll_socket(&socket, true, 200).await {
        if sessions {
            // The new daemon respawns each adapter (and its MCP child) in
            // the background after the in-place exec, so by the time the
            // socket is reachable again the bounce is typically still in
            // flight — report it as in progress, not done.
            println!(
                "construct daemon restarted; sessions are restarting in the background ({})",
                socket.display()
            );
        } else {
            println!("construct daemon restarted ({})", socket.display());
        }
        Ok(())
    } else {
        anyhow::bail!("construct daemon did not come back within 20s after restart")
    }
}
