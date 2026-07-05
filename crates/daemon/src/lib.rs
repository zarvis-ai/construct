//! construct daemon: session supervisor and IPC server.
//!
//! The daemon's entire runtime lives here as a library. It is driven by the
//! `construct daemon` subcommand of the unified `construct` binary (see
//! `crates/cli`) — there is no standalone daemon binary. The TUI also calls
//! [`spawn_detached_daemon`] to auto-start one when none is running.
//!
//! The entry point calls [`run`] after [`init_tracing`]. On `daemon.restart`
//! the daemon replays its own argv (`construct daemon run …`; see
//! `session::request_daemon_restart`), so the self-`exec()` restart path picks
//! up an upgraded binary in place.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

mod adapter;
mod availability;
mod config;
mod loops;
mod remote;
mod remote_supervisor;
mod server;
mod session;
mod storage;
mod tunnel;
mod worktree;

use agentd_protocol::paths::Paths;

/// The embedded default config TOML, surfaced by the `default-config`
/// subcommand on both daemon entry points.
pub use config::DEFAULT_CONFIG_TOML;

/// Install the daemon's tracing subscriber. Defaults to a verbose
/// daemon-oriented filter (`info,agentd=debug,agentd_protocol=info`) so daemon
/// logs are useful out of the box; `RUST_LOG` overrides it. Idempotent —
/// safe to call once from whichever binary owns the process.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info,agentd=debug,agentd_protocol=info"))
        .unwrap();
    let _ = fmt().with_env_filter(filter).with_target(false).try_init();
}

/// Print the resolved config/state/data/runtime paths plus the local web UI
/// URL. Shared by both daemon entry points' `paths` subcommand.
pub fn print_paths() {
    let p = Paths::discover();
    println!("config:  {}", p.config_dir.display());
    println!("state:   {}", p.state_dir.display());
    println!("data:    {}", p.data_dir.display());
    println!("runtime: {}", p.runtime_dir.display());
    println!("socket:  {}", p.socket().display());
    println!("webui:   {}", agentd_protocol::paths::local_webui_url());
}

/// Run the daemon in the foreground until a shutdown signal, a fatal server
/// error, or a `daemon.restart` request. `socket_override` replaces the
/// discovered IPC socket path when set.
pub async fn run(socket_override: Option<PathBuf>) -> Result<()> {
    // Capture the executable path now, before anything can replace
    // the binary on disk. `/construct restart` re-`exec()`s this path;
    // resolving it lazily at restart time is unreliable once an
    // upgrade has rename-replaced the file (Linux's
    // `current_exe()` then reads as "… (deleted)"). See
    // `session::capture_startup_exe`.
    session::capture_startup_exe();

    let paths = Paths::discover();
    warn_legacy_paths(&paths);
    std::fs::create_dir_all(&paths.state_dir).ok();
    std::fs::create_dir_all(&paths.data_dir).ok();
    std::fs::create_dir_all(&paths.runtime_dir).ok();
    std::fs::create_dir_all(&paths.config_dir).ok();
    config::write_template(&paths);

    // Resolve the socket path early so the single-instance lock can key off
    // it: two daemons on the *same* socket must not coexist (the second would
    // unlink and steal the first's socket in `server::serve`), while daemons
    // on *different* sockets are independent.
    let socket_path = socket_override.unwrap_or_else(|| paths.socket());

    // Single-instance guard. Auto-start (a client spawning the daemon when it
    // finds no live socket — see `spawn_detached_daemon`) means two `construct`
    // launches can race to start a daemon. Whoever wins this exclusive
    // advisory lock binds the socket; the loser exits cleanly instead of
    // stealing the socket. Held for the process lifetime — dropped on exit,
    // and released across the restart `exec()` (the fd is CLOEXEC) so the new
    // image re-acquires it.
    let _singleton = match acquire_singleton_lock(&socket_path) {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            tracing::info!(
                socket = %socket_path.display(),
                "another construct daemon already owns this socket; exiting"
            );
            eprintln!(
                "construct: a daemon already owns {}; not starting another.",
                socket_path.display()
            );
            return Ok(());
        }
        Err(e) => return Err(e.context("acquire daemon single-instance lock")),
    };

    let config = config::Config::load_or_default(&paths)?;
    tracing::info!(
        adapters = config.adapters.len(),
        config_dir = %paths.config_dir.display(),
        "loaded config"
    );

    let program_templates_dir = config.program_templates_dir_override();
    if let Some(dir) = program_templates_dir.as_ref() {
        tracing::info!(dir = %dir.display(), "program templates dir override");
    }
    let storage = Arc::new(
        storage::Storage::new(paths.data_dir.clone())?
            .with_program_templates_dir(program_templates_dir),
    );
    let (manager, remote_rx, mut restart_rx) =
        session::SessionManager::new(storage.clone(), Arc::new(config), paths.runtime_dir.clone())
            .await
            .context("init session manager")?;
    let manager = Arc::new(manager);
    // Spawn the remote supervisor first so any subsequent
    // `start_remote` call (boot-time env-var path or in-flight
    // `remote.start` IPC) has a live receiver to send to.
    {
        let mgr = manager.clone();
        tokio::spawn(async move {
            remote_supervisor::run(mgr, remote_rx).await;
        });
    }
    // Resume + orchestrator bootstrap in the BACKGROUND so the IPC
    // socket (`server::serve` below) binds immediately, before any of
    // this slow, network/subprocess-bound work runs.
    //
    // Why this matters for `/construct restart`: each adapter reattach
    // / respawn is bounded but slow — `connect_with_retry` waits up to
    // 5s for the adapter to re-bind its socket and `initialize()` waits
    // up to 60s for the handshake — and resume runs them sequentially.
    // A fresh orchestrator spawn (when the prior smith session was
    // terminal) adds another such round-trip. If even one adapter is
    // wedged, awaiting all of this before binding the socket leaves the
    // daemon unreachable for tens of seconds (worst case minutes), and
    // a TUI/web client that dropped on the restart `exec()` just spins
    // in "reconnecting…" the whole time — indistinguishable from a
    // hang. Binding first means the client reconnects within a poll
    // cycle; sessions and the orchestrator panel then populate as they
    // resume (each reattach broadcasts its State, and the orchestrator
    // appears via the same event). `resume` stays first so an existing
    // orchestrator is reattached before `ensure_orchestrator` checks
    // for a live one (no duplicate spawn).
    //
    // Both steps are best-effort and log-only on failure: resume marks
    // un-resumable sessions Errored; a failed orchestrator spawn just
    // leaves clients in palette mode.
    {
        let mgr = manager.clone();
        tokio::spawn(async move {
            mgr.clone().resume_running_sessions().await;
            mgr.ensure_orchestrator().await;
        });
    }
    manager.spawn_widget_watcher();
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

    // Always expose the browser UI on localhost without remote-control
    // credentials. This is intentionally local-only: `/remote-control`
    // remains the opt-in public tunnel path and still layers token +
    // Basic auth on top.
    let local_webui_port = agentd_protocol::paths::local_webui_port();
    {
        let mgr = manager.clone();
        tokio::spawn(async move {
            let addr = format!("127.0.0.1:{local_webui_port}");
            match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => {
                    tracing::info!(url = %format!("http://{addr}/"), "local webui ready (localhost-only, no auth)");
                    if let Err(e) = server::serve_local_webui_on(mgr, listener).await {
                        tracing::error!(error = %e, "local webui listener failed");
                    }
                }
                Err(e) => {
                    tracing::warn!(addr = %addr, error = %e, "local webui disabled; bind failed");
                }
            }
        });
    }

    // Auto-start the remote WS listener at boot when
    // `CONSTRUCT_REMOTE_WS_PORT` is set — the headless / scripted
    // entry point. Interactive users get the same machinery via
    // the TUI's `/remote-control` slash (which calls
    // `remote.start` over IPC and shows a QR), so the env var is
    // only needed when nobody is at the terminal to type the
    // command.
    if let Ok(port_raw) = std::env::var("CONSTRUCT_REMOTE_WS_PORT") {
        match port_raw.parse::<u16>() {
            Ok(port) => {
                let mgr = manager.clone();
                tokio::spawn(async move {
                    // Boot path = tunnel mode. The 15s wait is
                    // tolerable here because the daemon is starting
                    // and nobody is staring at a UI. Failure
                    // (cloudflared missing, no public URL) is
                    // logged but doesn't kill the daemon — local
                    // WS is still up.
                    // Env-var boot path uses the auto-generated
                    // password; nobody is at the TUI to type one.
                    // The password lands in the info log so it's
                    // visible to the operator running the daemon.
                    let params = agentd_protocol::RemoteStartParams {
                        local_only: false,
                        password: None,
                        wait_for_tunnel: true,
                    };
                    if let Err(e) = mgr.start_remote(Some(port), params).await {
                        tracing::error!(error = %e, "boot-time start_remote failed");
                    }
                });
            }
            Err(_) => tracing::warn!(
                value = %port_raw,
                "CONSTRUCT_REMOTE_WS_PORT is not a valid u16; skipping ws listener"
            ),
        }
    } else if paths.runtime_dir.join("remote.json").exists() {
        // `/construct restart` path: the prior daemon had the remote
        // listener up and persisted a snapshot. Resume ONLY if that
        // snapshot is still adoptable (fresh, and its cloudflared
        // tunnel PID — if any — still alive), picking the port +
        // token + password back up without the user retyping
        // `/remote-control`.
        //
        // If the tunnel can no longer be restored, switch
        // remote-control OFF rather than spinning up a brand-new
        // tunnel: a restart must never silently rotate the public
        // URL/credentials. `snapshot_restorable` removes the dead
        // snapshot as a side effect, so the next boot stays off too.
        let snapshot_path = paths.runtime_dir.join("remote.json");
        if crate::remote_supervisor::snapshot_restorable(&snapshot_path) {
            let mgr = manager.clone();
            tokio::spawn(async move {
                let params = agentd_protocol::RemoteStartParams {
                    local_only: false,
                    password: None,
                    wait_for_tunnel: true,
                };
                // port_hint=None — the supervisor reads the snapshot
                // and uses snapshot.port instead.
                if let Err(e) = mgr.start_remote(None, params).await {
                    tracing::warn!(error = %e, "remote snapshot resume failed");
                }
            });
        } else {
            tracing::info!("remote: prior tunnel no longer restorable across restart; staying off");
        }
    }

    // Race the IPC accept loop against shutdown signals + the
    // lifecycle channel:
    //
    //   - SIGTERM/SIGINT: drain adapters first (flush state), then
    //     exit normally.
    //   - SIGHUP: exit without touching adapters so reload-like
    //     supervisors don't kill running sessions.
    //   - daemon.restart RPC: skip adapter drain (the new daemon
    //     will resume them from on-disk state immediately) and
    //     `exec()` the current binary in place, picking up any
    //     on-disk upgrade. PID is preserved; cloudflared (a child
    //     subprocess) gets killed via `kill_on_drop` and a new
    //     one is spawned by the new daemon if `/remote-control`
    //     is re-issued — URL preservation across restart is
    //     follow-up work, see issue #90 comments.
    //   - daemon.restart RPC with `restart_sessions`: same exec, but
    //     stop every adapter first so the new daemon respawns each
    //     session (and its MCP child) fresh instead of reattaching.
    //   - daemon.shutdown RPC: stop adapters (leaving sessions
    //     resumable) and exit without re-exec'ing — the `daemon stop`
    //     command.
    let outcome = tokio::select! {
        result = server::serve(manager.clone(), socket_path) => MainOutcome::Server(result),
        signal = shutdown_signal() => MainOutcome::Signal(signal),
        Some(cmd) = restart_rx.recv() => MainOutcome::Restart(cmd),
    };
    match outcome {
        MainOutcome::Server(r) => r,
        MainOutcome::Signal(DaemonSignal::Reload) => {
            tracing::info!("received SIGHUP; exiting without stopping adapters");
            Ok(())
        }
        MainOutcome::Signal(DaemonSignal::Terminate) => {
            tracing::info!("received termination signal; shutting down adapters");
            manager.shutdown_adapters().await;
            Ok(())
        }
        MainOutcome::Restart(cmd) => {
            use crate::session::RestartAction;
            // For RestartSessions / Stop, gracefully stop every adapter
            // first. `shutdown_adapters` sets the shutting-down flag so
            // each adapter's exit is treated as expected and its
            // persisted session state is left resumable (not marked
            // Done/Errored) — the restart path then respawns it, and the
            // stop path leaves it for the next `daemon start` to resume.
            if matches!(cmd.action, RestartAction::RestartSessions | RestartAction::Stop) {
                tracing::info!(
                    action = ?cmd.action,
                    "stopping adapters before daemon lifecycle action"
                );
                manager.shutdown_adapters().await;
            }
            if cmd.action == RestartAction::Stop {
                tracing::info!("daemon shutdown requested; exiting");
                return Ok(());
            }
            tracing::info!(exe = %cmd.exe.display(), "daemon restart requested; exec self");
            // exec() replaces the process image in place — kernel
            // closes any FDs marked CLOEXEC (which tokio sockets
            // are by default), so the IPC + WS listeners are
            // released cleanly. cloudflared (child subprocess)
            // is taken down via kill_on_drop as the tokio runtime
            // tears down. The new daemon will rebind whichever
            // listeners are configured.
            //
            // Returns only on error — successful exec doesn't
            // return. Surface the error as the daemon's exit
            // status so the operator sees why the restart failed.
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(&cmd.exe).args(&cmd.args).exec();
            Err(anyhow::anyhow!("exec({}) failed: {err}", cmd.exe.display()))
        }
    }
}

/// RAII holder for the daemon single-instance advisory lock. The lock is
/// released when this is dropped (process exit) or when the fd is closed
/// (across the restart `exec()` — the fd is CLOEXEC by default).
pub struct SingletonGuard {
    _file: std::fs::File,
}

/// Try to acquire the exclusive single-instance lock for `socket_path`. The
/// lock file sits next to the socket (`<socket>.lock`), so daemons on
/// different sockets don't contend. Returns `Ok(Some(guard))` when acquired,
/// `Ok(None)` when another daemon already holds it, or `Err` on an unexpected
/// I/O failure.
fn acquire_singleton_lock(socket_path: &std::path::Path) -> Result<Option<SingletonGuard>> {
    use std::os::unix::io::AsRawFd;

    let mut lock_path = socket_path.as_os_str().to_owned();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open lock file {}", lock_path.display()))?;

    // Non-blocking exclusive advisory lock; EWOULDBLOCK means another live
    // daemon holds it.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(Some(SingletonGuard { _file: file }));
    }
    let err = std::io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) => Ok(None),
        _ => Err(anyhow::Error::new(err).context(format!("flock {}", lock_path.display()))),
    }
}

/// Spawn the construct daemon as a detached background process. Used by the
/// client to auto-start a daemon when none is listening: runs `<current exe>
/// daemon run [--socket <socket>]` in a new session (so it survives the
/// spawning TUI's exit and terminal SIGHUP), with stdio redirected to the
/// daemon log (falling back to `/dev/null`).
///
/// Safe to call from multiple clients concurrently — the single-instance lock
/// in [`run`] ensures only one of the spawned daemons survives; the rest exit
/// immediately.
pub fn spawn_detached_daemon(socket: Option<&std::path::Path>) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.arg("daemon").arg("run");
    if let Some(s) = socket {
        cmd.arg("--socket").arg(s);
    }
    cmd.stdin(Stdio::null());

    // Redirect logs to a file so an auto-started daemon stays debuggable;
    // discard them if the log can't be opened.
    let paths = Paths::discover();
    std::fs::create_dir_all(&paths.state_dir).ok();
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(paths.state_dir.join("daemon.log"))
    {
        Ok(log) => {
            let log2 = log.try_clone()?;
            cmd.stdout(log).stderr(log2);
        }
        Err(_) => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }

    // New session: detach from the spawning process's controlling terminal and
    // process group so the daemon isn't taken down when the TUI exits.
    unsafe {
        cmd.pre_exec(|| {
            // Ignore errors (e.g. EPERM if already a session leader).
            let _ = nix::unistd::setsid();
            Ok(())
        });
    }

    cmd.spawn()?;
    Ok(())
}

fn warn_legacy_paths(current: &Paths) {
    if let Some(msg) = legacy_migration_notice(current) {
        eprint!("{}", msg);
    }
}

fn shell_quote(path: &std::path::Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn legacy_migration_notice(current: &Paths) -> Option<String> {
    legacy_migration_notice_with_paths(current, &Paths::discover_legacy())
}

fn legacy_migration_notice_with_paths(current: &Paths, legacy: &Paths) -> Option<String> {
    let mut found = Vec::<(&str, &std::path::Path)>::new();
    let legacy_items = [
        ("config directory", legacy.config_dir.as_path()),
        ("state directory", legacy.state_dir.as_path()),
        ("data directory", legacy.data_dir.as_path()),
        ("runtime directory", legacy.runtime_dir.as_path()),
    ];

    for (label, p) in legacy_items {
        if p.exists() {
            found.push((label, p));
        }
    }

    let legacy_config = legacy.config_file();
    if legacy_config.exists() {
        found.push(("legacy config file", legacy_config.as_path()));
    }
    if found.is_empty() {
        return None;
    }

    let mut out = String::new();
    out.push_str("\n[construct] Detected existing legacy `agentd` layout;\nthis daemon no longer reads those locations.\n");
    for (label, path) in found {
        out.push_str(&format!("  - legacy {label}: {}\n", path.display()));
    }

    out.push_str("\nSuggested migration:\n");
    out.push_str(&format!(
        "  config: {old} -> {new}\n",
        old = legacy.config_dir.display(),
        new = current.config_dir.display()
    ));
    out.push_str(&format!(
        "   state: {old} -> {new}\n",
        old = legacy.state_dir.display(),
        new = current.state_dir.display()
    ));
    out.push_str(&format!(
        "    data: {old} -> {new}\n",
        old = legacy.data_dir.display(),
        new = current.data_dir.display()
    ));
    out.push_str(&format!(
        " runtime: {old} -> {new}\n",
        old = legacy.runtime_dir.display(),
        new = current.runtime_dir.display()
    ));

    out.push_str("\nCopy/paste migration command:\n");
    out.push_str(&format!(
        "  mkdir -p {} {} {} {}\n",
        shell_quote(&current.config_dir),
        shell_quote(&current.state_dir),
        shell_quote(&current.data_dir),
        shell_quote(&current.runtime_dir)
    ));

    let migration_dirs = [
        (legacy.config_dir.as_path(), current.config_dir.as_path()),
        (legacy.state_dir.as_path(), current.state_dir.as_path()),
        (legacy.data_dir.as_path(), current.data_dir.as_path()),
        (legacy.runtime_dir.as_path(), current.runtime_dir.as_path()),
    ];
    for (old_dir, new_dir) in migration_dirs {
        if old_dir.exists() {
            out.push_str(&format!(
                "  cp -a {old}/. {new}/\n",
                old = shell_quote(old_dir),
                new = shell_quote(new_dir)
            ));
        }
    }
    if legacy_config.exists() {
        out.push_str(&format!(
            "  cp -a {old_config} {new_config}\n",
            old_config = shell_quote(&legacy_config),
            new_config = shell_quote(&current.config_file())
        ));

        let new_config = shell_quote(&current.config_file());
        out.push_str(&format!(
            "  tmp=$(mktemp) && perl -0pe 's/\\[adapters\\.zarvis([^\\]]*)\\]/[adapters.smith$1]/g' {new_config} > \"$tmp\" && mv \"$tmp\" {new_config}\n",
            new_config = new_config
        ));
    }

    out.push_str("\nMove or copy what you need; restart after migration.\n");

    Some(out)
}

enum MainOutcome {
    Server(Result<()>),
    Signal(DaemonSignal),
    Restart(crate::session::RestartCommand),
}

#[derive(Debug, Clone, Copy)]
enum DaemonSignal {
    Reload,
    Terminate,
}

#[cfg(unix)]
async fn shutdown_signal() -> DaemonSignal {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hup = signal(SignalKind::hangup()).expect("install SIGHUP handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");

    tokio::select! {
        _ = hup.recv() => DaemonSignal::Reload,
        _ = int.recv() => DaemonSignal::Terminate,
        _ = term.recv() => DaemonSignal::Terminate,
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> DaemonSignal {
    let _ = tokio::signal::ctrl_c().await;
    DaemonSignal::Terminate
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn path(base: &std::path::Path, tail: &str) -> std::path::PathBuf {
        base.join(tail)
    }

    #[test]
    fn legacy_migration_notice_omits_output_when_no_legacy_layout() {
        let root = tempdir().unwrap();
        let current = Paths {
            config_dir: path(root.path(), "construct-config"),
            state_dir: path(root.path(), "construct-state"),
            data_dir: path(root.path(), "construct-data"),
            runtime_dir: path(root.path(), "construct-runtime"),
        };
        let legacy = Paths {
            config_dir: path(root.path(), "agentd-config"),
            state_dir: path(root.path(), "agentd-state"),
            data_dir: path(root.path(), "agentd-data"),
            runtime_dir: path(root.path(), "agentd-runtime"),
        };
        assert!(super::legacy_migration_notice_with_paths(&current, &legacy).is_none());
    }

    #[test]
    fn legacy_migration_notice_includes_detected_items() {
        let root = tempdir().unwrap();
        let legacy = Paths {
            config_dir: path(root.path(), "agentd-config"),
            state_dir: path(root.path(), "agentd-state"),
            data_dir: path(root.path(), "agentd-data"),
            runtime_dir: path(root.path(), "agentd-runtime"),
        };
        let current = Paths {
            config_dir: path(root.path(), "construct-config"),
            state_dir: path(root.path(), "construct-state"),
            data_dir: path(root.path(), "construct-data"),
            runtime_dir: path(root.path(), "construct-runtime"),
        };
        std::fs::create_dir_all(&legacy.config_dir).unwrap();
        std::fs::create_dir_all(&legacy.state_dir).unwrap();
        std::fs::write(legacy.config_file(), "configured = true").unwrap();

        let msg = super::legacy_migration_notice_with_paths(&current, &legacy).expect("notice");
        assert!(msg.contains("Detected existing legacy `agentd` layout"));
        assert!(msg.contains(&format!(
            "legacy config file: {}",
            legacy.config_file().display()
        )));
        assert!(msg.contains("state directory"));
        assert!(msg.contains("legacy config file"));
        assert!(msg.contains(&format!("config: {}", legacy.config_dir.display())));
        assert!(msg.contains(&format!(
            "  config: {} -> {}",
            legacy.config_dir.display(),
            current.config_dir.display()
        )));
        assert!(msg.contains(&format!(
            " state: {} -> {}",
            legacy.state_dir.display(),
            current.state_dir.display()
        )));
        assert!(msg.contains("Copy/paste migration command:"));
        assert!(msg.contains("mkdir -p"));
        assert!(msg.contains("cp -a"));
        assert!(msg.contains("tmp=$(mktemp) && perl"));
        assert!(msg.contains("adapters.smith"));
    }
}
