//! construct daemon (`constructd` / `construct daemon`): session supervisor and
//! IPC server.
//!
//! The daemon's entire runtime lives here as a library so it can be driven from
//! two binaries that share one code path:
//!
//! - `constructd` — the standalone daemon binary (thin shim in `main.rs`),
//!   kept as a back-compat alias.
//! - `construct daemon …` — the unified `construct` binary's daemon subcommand
//!   (see `crates/cli`), so a single installed binary can run both the TUI
//!   client and the daemon.
//!
//! Both entry points call [`run`] after [`init_tracing`]. They each replay
//! their own argv on `daemon.restart` (see `session::request_daemon_restart`),
//! so the self-`exec()` restart path stays correct regardless of which name
//! launched the daemon.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;

mod adapter;
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

    let config = config::Config::load_or_default(&paths)?;
    tracing::info!(
        adapters = config.adapters.len(),
        config_dir = %paths.config_dir.display(),
        "loaded config"
    );

    let storage = Arc::new(storage::Storage::new(paths.data_dir.clone())?);
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

    let socket_path = socket_override.unwrap_or_else(|| paths.socket());

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
    // restart channel:
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
