//! Single long-running task that owns the side effects of starting
//! and stopping the remote WS transport: binding the listener,
//! spawning the WS accept loop, spawning the cloudflared
//! supervisor, and tearing both back down on demand.
//!
//! Sits behind an mpsc channel so `SessionManager::start_remote`
//! / `stop_remote` can request transitions without statically
//! depending on `crate::server::serve_ws_on`. That dependency
//! would otherwise create a recursive Send-inference cycle
//! (`dispatch` → `start_remote` → `serve_ws_on` →
//! `handle_ws_connection` → `dispatch`), which rustc bails out of
//! as "future cannot be sent between threads safely". Breaking
//! the static call edge with a channel-based handoff lets each
//! function's Send-ness be inferred independently.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use crate::remote::RemoteState;
use crate::session::{RemoteHandle, SessionManager};

/// Supervisor command set. `Start` is the `/remote-control` /
/// `/remote-control-debug` path; `Stop` is the `/remote-stop`
/// path. Stop is dispatched serially with the other commands so
/// there's no race between "client A asks to start" and "client B
/// asks to stop".
pub enum SupervisorMsg {
    Start(StartRequest),
    Stop(StopRequest),
}

/// One "please start the remote transport" request. The supervisor
/// processes these serially; the response goes back via `respond`
/// so the caller can know whether the bind succeeded and which
/// `RemoteState`/`port` ended up being installed.
pub struct StartRequest {
    pub port_hint: Option<u16>,
    /// Whether to also spawn the cloudflared quick-tunnel. False
    /// is the `/remote-control-debug` path: bind the local WS
    /// listener and stop there. True is the `/remote-control`
    /// path: bind + start cloudflared so the caller can poll for
    /// the public URL. Idempotent — if cloudflared has already
    /// been spawned for the *current* RemoteState's lifetime,
    /// repeat requests are no-ops. A `Stop` clears the spawned
    /// flag so the next start re-spawns cloudflared.
    pub spawn_tunnel: bool,
    /// Caller-supplied override for the HTTP Basic auth password.
    /// `None` lets the daemon auto-generate. Only honored on the
    /// *first* start (when the listener is being bound); later
    /// repeat starts reuse the existing `RemoteState` and ignore
    /// this field — `/remote-stop` + `/remote-control <new-pw>`
    /// is the recommended way to change the password mid-session.
    pub password: Option<String>,
    pub respond: oneshot::Sender<Result<StartOutcome>>,
}

/// One "please tear down the remote transport" request. Idempotent
/// — calling stop while nothing is running returns `was_running:
/// false`, not an error.
pub struct StopRequest {
    pub respond: oneshot::Sender<Result<StopOutcome>>,
}

/// Side effects of a successful bind: the `RemoteState` that owns
/// the token + tunnel URL, and the port the listener was bound to.
/// `SessionManager::start_remote` stuffs this into its `remote`
/// field if it isn't already populated (handling the "concurrent
/// caller already won the race" case there, not here).
pub struct StartOutcome {
    pub state: RemoteState,
    pub port: u16,
}

/// Side effects of a stop: whether there was anything to stop.
/// Lets the CLI emit "remote stopped" vs "remote wasn't running"
/// in the status line.
pub struct StopOutcome {
    pub was_running: bool,
}

/// Run the supervisor loop until the channel closes (daemon
/// shutdown). Spawned once at startup from `main.rs`. Owns the
/// `JoinHandle`s for the WS accept loop + cloudflared supervisor
/// so `Stop` can abort them.
pub async fn run(manager: Arc<SessionManager>, mut rx: mpsc::UnboundedReceiver<SupervisorMsg>) {
    let mut ws_task: Option<JoinHandle<()>> = None;
    let mut tunnel_task: Option<JoinHandle<()>> = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            SupervisorMsg::Start(req) => {
                let outcome = handle_start(
                    &manager,
                    &mut ws_task,
                    &mut tunnel_task,
                    req.port_hint,
                    req.spawn_tunnel,
                    req.password,
                )
                .await;
                let _ = req.respond.send(outcome);
            }
            SupervisorMsg::Stop(req) => {
                let outcome = handle_stop(&manager, &mut ws_task, &mut tunnel_task).await;
                let _ = req.respond.send(Ok(outcome));
            }
        }
    }
    tracing::debug!("remote supervisor channel closed; exiting");
}

async fn handle_start(
    manager: &Arc<SessionManager>,
    ws_task: &mut Option<JoinHandle<()>>,
    tunnel_task: &mut Option<JoinHandle<()>>,
    port_hint: Option<u16>,
    spawn_tunnel: bool,
    password: Option<String>,
) -> Result<StartOutcome> {
    // Fast path: listener already installed (previous request, or
    // boot-time env-var startup). Reuse it and only kick the
    // tunnel if asked. The `password` override is ignored on this
    // path — the user must `/remote-stop` first to mint a new
    // RemoteState if they want a new password.
    let existing = {
        let guard = manager.remote_slot().expect("remote mutex poisoned");
        guard.as_ref().map(|h| (h.state.clone(), h.port))
    };
    let (state, port) = if let Some(pair) = existing {
        pair
    } else {
        bind_and_install(manager, ws_task, port_hint, password).await?
    };

    // Spawn cloudflared at most once per current RemoteState
    // lifetime. The tunnel itself is restart-on-death inside
    // `tunnel::run`, so we don't need to track health here — just
    // whether we've ever started it for this state. A `Stop`
    // clears `tunnel_task`, so a subsequent start re-spawns
    // cloudflared with the (new) token.
    if spawn_tunnel && tunnel_task.is_none() {
        if std::env::var("AGENTD_REMOTE_NO_TUNNEL").is_err() {
            let st = state.clone();
            let handle = tokio::spawn(async move {
                crate::tunnel::run(st, port).await;
            });
            *tunnel_task = Some(handle);
        } else {
            tracing::info!(
                "AGENTD_REMOTE_NO_TUNNEL is set; skipping cloudflared spawn"
            );
        }
    }

    Ok(StartOutcome { state, port })
}

async fn handle_stop(
    manager: &Arc<SessionManager>,
    ws_task: &mut Option<JoinHandle<()>>,
    tunnel_task: &mut Option<JoinHandle<()>>,
) -> StopOutcome {
    // Clear the manager's slot first so any concurrent IPC method
    // sees "not running" before we abort the loops underneath.
    let was_running = {
        let mut guard = manager.remote_slot().expect("remote mutex poisoned");
        guard.take().is_some()
    };
    // Abort cloudflared first — once its task is gone, the
    // subprocess dies (kill_on_drop) and the public URL stops
    // resolving at Cloudflare's edge. Then abort the WS accept
    // loop, which drops the `TcpListener` and stops accepting new
    // connections. Existing per-connection tasks aren't tracked
    // individually; they exit naturally once the client disconnects
    // (typical timeout: minutes) or sooner once cloudflared
    // tears the tunnel down.
    if let Some(h) = tunnel_task.take() {
        h.abort();
    }
    if let Some(h) = ws_task.take() {
        h.abort();
    }
    // Broadcast `remote/state` with clients=0 so the local TUI
    // drops its `[● remote: N]` badge even if individual per-
    // connection `RemoteClientGuard` drops haven't fired yet (they
    // will once those tasks observe the aborted listener).
    manager.broadcast_remote_state(0);
    if was_running {
        tracing::info!("remote stopped: listener + tunnel torn down");
    }
    StopOutcome { was_running }
}

/// Bind a fresh listener, install the resulting `RemoteHandle` in
/// the manager's slot, and spawn the WS accept loop. Returns the
/// `(state, port)` for the installed handle. Race-protected — if
/// a concurrent caller installed first, we drop our listener and
/// return the existing state instead. Records the spawn handle on
/// `ws_task` so `handle_stop` can abort it later.
async fn bind_and_install(
    manager: &Arc<SessionManager>,
    ws_task: &mut Option<JoinHandle<()>>,
    port_hint: Option<u16>,
    password: Option<String>,
) -> Result<(RemoteState, u16)> {
    let bind_addr = format!("127.0.0.1:{}", port_hint.unwrap_or(0));
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("bind WS listener {bind_addr}"))?;
    let port = listener.local_addr().context("query bound port")?.port();
    let state = RemoteState::with_password(password);
    tracing::info!(
        port,
        url = %format!("http://127.0.0.1:{port}/t/{}", state.token()),
        "remote ws ready (token-gated, localhost-bind)"
    );

    let installed = {
        let mut guard = manager.remote_slot().expect("remote mutex poisoned");
        if guard.is_some() {
            None
        } else {
            *guard = Some(RemoteHandle {
                state: state.clone(),
                port,
            });
            Some(())
        }
    };
    if installed.is_none() {
        drop(listener);
        let snapshot = {
            let guard = manager.remote_slot().expect("remote mutex poisoned");
            guard.as_ref().map(|h| (h.state.clone(), h.port))
        };
        if let Some((state, port)) = snapshot {
            return Ok((state, port));
        }
        anyhow::bail!("concurrent remote start raced and lost; please retry");
    }

    // We're the one installing. Spawn the WS accept loop. Tunnel
    // (if any) is spawned by the caller of `bind_and_install`,
    // depending on the request's `spawn_tunnel` flag.
    let mgr = manager.clone();
    let st = state.clone();
    let handle = tokio::spawn(async move {
        if let Err(e) = crate::server::serve_ws_on(mgr, st, listener).await {
            tracing::error!(error = %e, "ws server exited");
        }
    });
    *ws_task = Some(handle);

    Ok((state, port))
}
