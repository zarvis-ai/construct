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
use construct_protocol::TunnelProvider;
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
/// the file exists, parses, is fresh, AND the tunnel PID it records
/// is still alive (so the URL is actually adoptable).
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
            "snapshot's tunnel PID is gone; minting fresh credentials"
        );
        let _ = std::fs::remove_file(path);
        return None;
    }
    Some(snap)
}

/// Boot-time helper for the `/construct restart` resume path: is the
/// persisted remote snapshot at `path` still adoptable (fresh, and its
/// tunnel PID — if any — still alive), and if so, which provider was
/// publishing it? A negative verdict removes the snapshot file as a
/// side effect.
///
/// Used at boot to decide whether to *resume* the remote transport
/// across a restart or *switch it off*. A tunnel that can no longer be
/// adopted must NOT be silently replaced by a brand-new one — a restart
/// should never rotate the URL or credentials behind the user's back.
///
/// `Some(TunnelProvider::None)` and `None` mean different things:
/// the former is a listener that was up but reachable only on the LAN
/// (nothing to adopt, but do rebind it); the latter is nothing to
/// resume at all.
pub fn restorable_provider(path: &std::path::Path) -> Option<TunnelProvider> {
    let snap = load_restore_snapshot(path)?;
    // The first-party client runs inside the daemon and deliberately keeps
    // every registration capability only in memory. An exec-style daemon
    // restart therefore cannot adopt or silently recreate that tunnel: doing
    // so would require persisting a credential or opening OAuth unexpectedly.
    // Switch remote control off and let the user reconnect explicitly.
    if snap.tunnel_provider == TunnelProvider::Construct {
        let _ = std::fs::remove_file(path);
        return None;
    }
    // A snapshot written by a daemon from before providers existed
    // records a live tunnel PID but no provider, and taking its
    // `None` at face value would leave that cloudflared orphaned with
    // nobody watching it. Back then cloudflared was the only thing it
    // could be.
    Some(match (snap.tunnel_provider, snap.tunnel_pid) {
        (TunnelProvider::None, pid) if pid != 0 => TunnelProvider::Cloudflare,
        (p, _) => p,
    })
}

/// Supervisor command set. `Start` covers both opening the
/// `/remote-control` dialog (provider `None`) and picking a provider
/// in it; `Stop` is `/remote-control stop`. Stop is dispatched
/// serially with the other commands so there's no race between
/// "client A asks to start" and "client B asks to stop".
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
    /// Which provider to publish the listener through, if any.
    /// `TunnelProvider::None` binds the listener and stops there —
    /// reachable from this machine and the LAN, exposed nowhere else.
    /// That is what opening the `/remote-control` dialog does, so that
    /// looking at the dialog never publishes the daemon.
    ///
    /// Naming a provider additionally spawns its tunnel, at most once
    /// per `RemoteState` lifetime — repeat requests for a provider
    /// that is already running are no-ops. A `Stop` clears the task so
    /// the next start spawns fresh.
    pub provider: TunnelProvider,
    /// Caller-supplied override for the HTTP Basic auth password.
    /// `None` lets the daemon auto-generate. Only honored on the
    /// *first* start (when the listener is being bound); later
    /// repeat starts reuse the existing `RemoteState` and ignore
    /// this field — `/remote-stop` + `/remote-control <new-pw>`
    /// is the recommended way to change the password mid-session.
    pub password: Option<String>,
    /// Optional stable name requested from a named tunnel provider.
    pub subdomain: Option<String>,
    pub respond: oneshot::Sender<Result<StartOutcome>>,
}

/// One "please tear down the remote transport" request. Idempotent
/// — calling stop while nothing is running returns `was_running:
/// false`, not an error.
pub struct StopRequest {
    /// Stop only the tunnel, leaving the LAN listener + password up.
    /// This is the dialog's `stop` button: drop the public URL, keep
    /// working on the local network. False tears everything down.
    pub tunnel_only: bool,
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
                    req.provider,
                    req.password,
                    req.subdomain,
                )
                .await;
                let _ = req.respond.send(outcome);
            }
            SupervisorMsg::Stop(req) => {
                let outcome = if req.tunnel_only {
                    handle_stop_tunnel(&manager, &mut tunnel_task).await
                } else {
                    handle_stop(&manager, &mut ws_task, &mut tunnel_task).await
                };
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
    provider: TunnelProvider,
    password: Option<String>,
    subdomain: Option<String>,
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

    // The listener is shared, but providers are mutually exclusive. If
    // the user chooses a different provider, stop the old child before
    // starting the new one so its URL can never be mislabeled as the
    // newly requested provider.
    if provider != TunnelProvider::None
        && tunnel_task.is_some()
        && state.tunnel_provider() != provider
    {
        handle_stop_tunnel(manager, tunnel_task).await;
    }

    // Spawn the provider's tunnel at most once per current RemoteState
    // lifetime. The tunnel itself is restart-on-death inside
    // `tunnel::run`, so we don't track health here — only whether we
    // have ever started one for this state. A `Stop` clears
    // `tunnel_task`, so a subsequent start spawns fresh.
    if provider != TunnelProvider::None && tunnel_task.is_none() {
        if std::env::var("CONSTRUCT_REMOTE_NO_TUNNEL").is_err() {
            state.set_tunnel_error(None).await;
            state.set_tunnel_provider(provider).await;
            // `adopt_pid` is non-zero only after a `/construct
            // restart`: the snapshot captured a still-running tunnel
            // PID and `bind_and_install` rehydrated the state from it.
            // `tunnel::run` watches that PID and falls back to
            // spawning only once it dies.
            let adopt_pid = state.tunnel_pid();
            let st = state.clone();
            let handle = tokio::spawn(async move {
                crate::tunnel::run(provider, st, port, adopt_pid, subdomain).await;
            });
            *tunnel_task = Some(handle);
        } else {
            tracing::info!("CONSTRUCT_REMOTE_NO_TUNNEL is set; skipping tunnel spawn");
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
    // Capture the tunnel PID + state for cleanup after the slot is
    // empty (the tunnel child is in its own process group, so aborting
    // the tokio task does not take the subprocess down — we have to
    // SIGTERM by PID).
    //
    // SIGTERM, not SIGKILL: the tunnel child cleans up on the way out
    // (a provider that registers a mapping withdraws it on exit), and
    // killing it outright could leave that mapping behind.
    let (was_running, tunnel_pid, provider, state_for_cleanup) = {
        let mut guard = manager.remote_slot().expect("remote mutex poisoned");
        let pid = guard.as_ref().map(|h| h.state.tunnel_pid()).unwrap_or(0);
        let provider = guard
            .as_ref()
            .map(|h| h.state.tunnel_provider())
            .unwrap_or(TunnelProvider::None);
        let state = guard.as_ref().map(|h| h.state.clone());
        let was = guard.take().is_some();
        (was, pid, provider, state)
    };
    if tunnel_pid != 0 {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let label = provider.label();
        match kill(Pid::from_raw(tunnel_pid as i32), Signal::SIGTERM) {
            Ok(()) => tracing::info!(pid = tunnel_pid, provider = label, "sent SIGTERM to tunnel"),
            Err(e) => {
                tracing::warn!(error = %e, pid = tunnel_pid, provider = label, "SIGTERM tunnel failed")
            }
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

/// Stop just the tunnel, leaving the LAN listener (and its password,
/// and any LAN-connected clients) running. This is the dialog's `stop`
/// button: drop the public URL without ending the remote-control
/// session, so the user can keep working over the local network and
/// start a fresh tunnel later.
///
/// The tunnel supervisor task is aborted *before* the SIGTERM so its
/// respawn loop can't race a new child into existence between the
/// signal and the abort. The listener's slot is left in place; only
/// the tunnel fields on the shared state are cleared, which also
/// rewrites the snapshot so a restart doesn't try to adopt the URL we
/// just killed.
async fn handle_stop_tunnel(
    manager: &Arc<SessionManager>,
    tunnel_task: &mut Option<JoinHandle<()>>,
) -> StopOutcome {
    let (state, tunnel_pid, provider) = {
        let guard = manager.remote_slot().expect("remote mutex poisoned");
        match guard.as_ref() {
            Some(h) => (
                Some(h.state.clone()),
                h.state.tunnel_pid(),
                h.state.tunnel_provider(),
            ),
            None => (None, 0, TunnelProvider::None),
        }
    };

    // Abort the supervisor first so it doesn't respawn on the death we
    // are about to cause.
    if let Some(h) = tunnel_task.take() {
        h.abort();
    }
    if tunnel_pid != 0 {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let label = provider.label();
        match kill(Pid::from_raw(tunnel_pid as i32), Signal::SIGTERM) {
            Ok(()) => tracing::info!(pid = tunnel_pid, provider = label, "stopped tunnel (listener kept)"),
            Err(e) => {
                tracing::warn!(error = %e, pid = tunnel_pid, provider = label, "SIGTERM tunnel failed")
            }
        }
    }

    let was_running = tunnel_pid != 0 || provider != TunnelProvider::None;
    if let Some(state) = state {
        // Clear the tunnel fields (and persist) so the listener now
        // looks LAN-only, and a subsequent start re-spawns a fresh
        // tunnel rather than short-circuiting on the dead URL.
        state.set_tunnel_url(None).await;
        state.set_auth_url(None).await;
        state.set_tunnel_error(None).await;
        state.set_tunnel_pid(0).await;
        state.set_tunnel_provider(TunnelProvider::None).await;
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
    // Bind every interface, not just loopback.
    //
    // The common way to use remote control is a phone on the same
    // Wi-Fi, and that needs no tunnel at all — but only if the
    // listener answers on the LAN address. A loopback bind would make
    // the "local network" option in the dialog a QR code pointing the
    // phone at itself.
    //
    // This is safe *because* every request through this listener is
    // gated by Basic auth with a throttled password (see
    // `RemoteState::note_auth_failure`). Note the separate always-on
    // web UI has no auth at all, which is exactly why that one stays
    // bound to loopback and this one does not get to borrow its
    // reasoning.
    let bind_addr = format!("0.0.0.0:{}", bind_port.unwrap_or(0));
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
        lan = ?crate::remote::lan_ipv4().map(|ip| format!("http://{ip}:{port}/")),
        "remote ws ready (basic-auth-gated, all interfaces)"
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
            tunnel_provider: TunnelProvider::Cloudflare,
            generated_at,
        }
    }

    #[test]
    fn in_process_construct_tunnel_requires_explicit_reconnect_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        let mut snap = snapshot(0, now_secs());
        snap.tunnel_provider = TunnelProvider::Construct;
        snap.tunnel_url = Some("https://swift-willow-4827.tunnel.zarvis.ai/".into());
        snap.write(&path).unwrap();

        assert_eq!(restorable_provider(&path), None);
        assert!(!path.exists(), "non-adoptable snapshot must be removed");
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
        assert_eq!(restorable_provider(&path), None);
    }

    /// A listener that was only ever reachable on the LAN has no tunnel
    /// to verify, so it is always adoptable — and it comes back as the
    /// LAN-only listener it was, not as a tunnel.
    #[test]
    fn fresh_lan_only_snapshot_is_restorable_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        let mut snap = snapshot(0, now_secs());
        snap.tunnel_provider = TunnelProvider::None;
        snap.tunnel_url = None;
        snap.write(&path).unwrap();
        assert_eq!(restorable_provider(&path), Some(TunnelProvider::None));
        assert!(path.exists(), "a restorable snapshot must be left in place");
    }

    /// The provider is carried across a restart rather than re-derived,
    /// so the URL the user is looking at keeps working.
    #[test]
    fn fresh_snapshot_with_live_tunnel_is_restorable() {
        // The test process itself is a guaranteed-alive PID.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        snapshot(std::process::id(), now_secs())
            .write(&path)
            .unwrap();
        assert_eq!(restorable_provider(&path), Some(TunnelProvider::Cloudflare));
    }

    /// Snapshots written before providers existed record a live PID and
    /// no provider. Reading that `None` literally would abandon a
    /// running cloudflared with nothing watching it, so it decodes as
    /// the only provider that existed back then.
    #[test]
    fn pre_provider_snapshot_with_live_pid_adopts_as_cloudflare() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        let json = serde_json::json!({
            "version": RemoteSnapshot::CURRENT_VERSION,
            "token": "tok",
            "password": "pw",
            "port": 12345,
            "tunnel_url": "https://example.trycloudflare.com",
            "tunnel_pid": std::process::id(),
            "generated_at": now_secs(),
        });
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(restorable_provider(&path), Some(TunnelProvider::Cloudflare));
    }

    #[test]
    fn stale_snapshot_is_not_restorable_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        let stale = now_secs().saturating_sub(SNAPSHOT_MAX_AGE_SECS + 60);
        snapshot(0, stale).write(&path).unwrap();
        assert_eq!(restorable_provider(&path), None);
        assert!(!path.exists(), "a stale snapshot must be cleaned up");
    }

    #[test]
    fn fresh_snapshot_with_dead_tunnel_is_not_restorable_and_removed() {
        // The tunnel can no longer be adopted, so the daemon must stay
        // off (and clean up) rather than quietly mint a new one.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("remote.json");
        snapshot(dead_pid(), now_secs()).write(&path).unwrap();
        assert_eq!(restorable_provider(&path), None);
        assert!(
            !path.exists(),
            "a snapshot with a dead tunnel PID must be cleaned up"
        );
    }
}
