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

use crate::remote::{process_alive, RemoteSnapshot, RemoteState};
use crate::session::{RemoteHandle, SessionManager};

/// Snapshots older than this are treated as stale and ignored
/// at restore time. The `/agentd restart` gap is sub-second on
/// healthy systems; minutes give plenty of headroom for slow
/// hardware, swap, or a paused-with-debugger restart, while
/// rejecting yesterday's leftover snapshot from a long-dead
/// daemon.
const SNAPSHOT_MAX_AGE_SECS: u64 = 300;

/// Inspect the snapshot file at `path`. Returns `Some(snap)` iff
/// the file exists, parses, is fresh, AND the cloudflared PID it
/// records is still alive (so the URL is actually adoptable).
/// Returns `None` in every other case — boot-time "no snapshot"
/// and "stale/dead snapshot" both fall through to fresh-mint.
///
/// On stale-or-dead, the file is deleted as a side effect so a
/// later boot doesn't keep retrying a hopeless adoption.
fn load_restore_snapshot(path: &std::path::Path) -> Option<RemoteSnapshot> {
    let snap = match RemoteSnapshot::read(path) {
        Ok(Some(s)) => s,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "remote snapshot read failed; ignoring");
            // Best-effort cleanup of an unreadable snapshot so it
            // doesn't keep producing warnings on every boot.
            let _ = std::fs::remove_file(path);
            return None;
        }
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if !snap.fresh_enough(now, SNAPSHOT_MAX_AGE_SECS) {
        tracing::info!(
            age_secs = now.saturating_sub(snap.generated_at),
            "remote snapshot is stale; minting fresh credentials"
        );
        let _ = std::fs::remove_file(path);
        return None;
    }
    if snap.tunnel_pid != 0 && !process_alive(snap.tunnel_pid) {
        tracing::info!(
            pid = snap.tunnel_pid,
            "snapshot's cloudflared PID is gone; minting fresh credentials"
        );
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(snap)
}

/// Boot-time helper for the `/agentd restart` resume path: is the
/// persisted remote snapshot at `path` still adoptable (fresh, and its
/// cloudflared PID — if any — still alive)? A negative verdict removes
/// the snapshot file as a side effect.
///
/// Used by `main.rs` to decide whether to *resume* the remote transport
/// across a restart or *switch it off*. A tunnel that can no longer be
/// adopted must NOT be silently replaced by a brand-new one — a restart
/// should never rotate the public URL/credentials behind the user's back.
pub fn snapshot_restorable(path: &std::path::Path) -> bool {
    load_restore_snapshot(path).is_some()
}

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
            // `adopt_pid` is non-zero only after a `/agentd
            // restart`: the snapshot captured a still-running
            // cloudflared PID and `bind_and_install` rehydrated
            // the state from it. `tunnel::run` watches that PID
            // and falls back to spawning only when it dies.
            let adopt_pid = state.tunnel_pid();
            let st = state.clone();
            let handle = tokio::spawn(async move {
                crate::tunnel::run(st, port, adopt_pid).await;
            });
            *tunnel_task = Some(handle);
        } else {
            tracing::info!("AGENTD_REMOTE_NO_TUNNEL is set; skipping cloudflared spawn");
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
    // Capture the cloudflared PID + state for cleanup after the
    // slot is empty (cloudflared is in its own process group, so
    // aborting the tokio task no longer takes the subprocess down
    // — we have to SIGTERM by PID).
    let (was_running, tunnel_pid, state_for_cleanup) = {
        let mut guard = manager.remote_slot().expect("remote mutex poisoned");
        let pid = guard.as_ref().map(|h| h.state.tunnel_pid()).unwrap_or(0);
        let state = guard.as_ref().map(|h| h.state.clone());
        let was = guard.take().is_some();
        (was, pid, state)
    };
    if tunnel_pid != 0 {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        match kill(Pid::from_raw(tunnel_pid as i32), Signal::SIGTERM) {
            Ok(()) => tracing::info!(pid = tunnel_pid, "sent SIGTERM to cloudflared"),
            Err(e) => tracing::warn!(error = %e, pid = tunnel_pid, "SIGTERM cloudflared failed"),
        }
    }
    if let Some(h) = tunnel_task.take() {
        h.abort();
    }
    if let Some(h) = ws_task.take() {
        h.abort();
    }
    // Delete the on-disk snapshot so the next daemon boot doesn't
    // try to adopt the (now-killed) URL. Idempotent — missing
    // file is fine.
    if let Some(state) = state_for_cleanup {
        state.clear_persisted();
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
    let snapshot_path = manager.remote_snapshot_path();
    // Try restoring from a freshly-written snapshot whose
    // cloudflared PID is still alive — that's the `/agentd
    // restart` path. If anything is off (no file, stale, PID
    // gone), mint fresh as usual. The user-supplied `password`
    // override is honored only for fresh starts; on restore the
    // existing password is preserved (the user expects same URL
    // + same pw on restart).
    let restored = load_restore_snapshot(&snapshot_path);
    let (bind_port, snapshot_pid) = match &restored {
        Some(snap) => (Some(snap.port), snap.tunnel_pid),
        None => (port_hint, 0),
    };
    let bind_addr = format!("127.0.0.1:{}", bind_port.unwrap_or(0));
    let listener = TcpListener::bind(&bind_addr)
        .await
        .with_context(|| format!("bind WS listener {bind_addr}"))?;
    let port = listener.local_addr().context("query bound port")?.port();
    let state = match restored {
        Some(snap) => {
            tracing::info!(
                token_prefix = &snap.token[..8.min(snap.token.len())],
                port = snap.port,
                tunnel_pid = snap.tunnel_pid,
                tunnel_url = ?snap.tunnel_url,
                "remote: restored from snapshot — preserving URL + password"
            );
            RemoteState::from_snapshot(&snap).with_snapshot_path(snapshot_path)
        }
        None => RemoteState::with_password(password).with_snapshot_path(snapshot_path),
    };
    // Record the (possibly fresh) port — restore already has it
    // but it's idempotent + ensures the snapshot file gets a
    // post-bind write.
    state.set_port(port).await;
    // Carry the snapshot's tunnel_pid forward so the supervisor's
    // tunnel-spawn step picks "adopt" vs "spawn fresh".
    if snapshot_pid != 0 {
        state.set_tunnel_pid(snapshot_pid).await;
    }
    tracing::info!(
        port,
        url = %format!("http://127.0.0.1:{port}/"),
        "remote ws ready (basic-auth-gated, localhost-bind)"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::RemoteSnapshot;

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn snapshot(tunnel_pid: u32, generated_at: u64) -> RemoteSnapshot {
        RemoteSnapshot {
            version: RemoteSnapshot::CURRENT_VERSION,
            token: "tok".into(),
            password: "pw".into(),
            port: 12345,
            tunnel_url: Some("https://example.trycloudflare.com".into()),
            tunnel_pid,
            generated_at,
        }
    }

    /// A PID that is guaranteed dead: spawn a trivial process, reap it,
    /// then reuse its (now-freed) PID.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn throwaway process");
        let pid = child.id();
        child.wait().expect("reap throwaway process");
        pid
    }

    #[test]
    fn missing_snapshot_is_not_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        assert!(!snapshot_restorable(&path));
    }

    #[test]
    fn fresh_local_only_snapshot_is_restorable_and_kept() {
        // tunnel_pid == 0 → local-only (no tunnel to verify). Fresh → adoptable.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        snapshot(0, now_secs()).write(&path).unwrap();
        assert!(snapshot_restorable(&path));
        assert!(path.exists(), "a restorable snapshot must be left in place");
    }

    #[test]
    fn fresh_snapshot_with_live_tunnel_is_restorable() {
        // The test process itself is a guaranteed-alive PID.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        snapshot(std::process::id(), now_secs())
            .write(&path)
            .unwrap();
        assert!(snapshot_restorable(&path));
    }

    #[test]
    fn stale_snapshot_is_not_restorable_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        let stale = now_secs().saturating_sub(SNAPSHOT_MAX_AGE_SECS + 60);
        snapshot(0, stale).write(&path).unwrap();
        assert!(!snapshot_restorable(&path));
        assert!(!path.exists(), "a stale snapshot must be cleaned up");
    }

    #[test]
    fn fresh_snapshot_with_dead_tunnel_is_not_restorable_and_removed() {
        // This is the issue's case: the tunnel can no longer be adopted,
        // so the daemon must stay off (and clean up) rather than create a
        // new tunnel.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        snapshot(dead_pid(), now_secs()).write(&path).unwrap();
        assert!(!snapshot_restorable(&path));
        assert!(
            !path.exists(),
            "a snapshot with a dead tunnel PID must be cleaned up"
        );
    }
}
