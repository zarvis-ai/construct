//! Tunnel providers: the things that make the remote listener
//! reachable from beyond the local network.
//!
//! The listener itself binds every interface and is gated by HTTP
//! Basic auth, so a phone on the same Wi-Fi needs no provider at all.
//! A provider is what you reach for when the phone is *not* on the
//! same Wi-Fi. Today there is one — [`cloudflare`], a `cloudflared`
//! quick tunnel: a public but unguessable URL, no account, nothing to
//! set up. The dispatch below is a seam, not a framework: another
//! provider is a backend module plus a `TunnelProvider` variant, and
//! nothing upstream of here has to change.
//!
//! Every backend presents the same long-running-future shape to the
//! supervisor, which is what keeps the rest of the daemon provider-agnostic.
//! External providers may use a child process; the first-party provider links
//! `wstunnel` and runs it inside that future:
//!
//! 1. Publish a browser URL on [`RemoteState`] once — and only once —
//!    the tunnel is actually serving.
//! 2. End when its supervisor future is cancelled, releasing whatever
//!    it registered. Child-process providers additionally record a PID so
//!    they can survive and be adopted across an exec-style daemon restart.

pub mod cloudflare;
pub mod construct;

use std::time::Duration;

use construct_protocol::{RemoteProviderInfo, TunnelProvider};

use crate::remote::{process_alive, RemoteState};

/// Providers the dialog offers, in the order it offers them.
pub const PROVIDERS: [TunnelProvider; 2] = [TunnelProvider::Construct, TunnelProvider::Cloudflare];

/// Probe every provider. Read-only — nothing is spawned, so the
/// dialog can call this on every open without side effects.
pub async fn probe_all() -> Vec<RemoteProviderInfo> {
    let mut out = Vec::with_capacity(PROVIDERS.len());
    for p in PROVIDERS {
        out.push(probe(p).await);
    }
    out
}

/// Probe one provider: could it start right now, and if not, what
/// should the user do about it?
pub async fn probe(provider: TunnelProvider) -> RemoteProviderInfo {
    let detail = match provider {
        TunnelProvider::None => None,
        TunnelProvider::Cloudflare => cloudflare::preflight().err(),
        TunnelProvider::Construct => construct::preflight().err(),
    };
    RemoteProviderInfo {
        provider,
        available: detail.is_none(),
        detail,
    }
}

/// Long-running supervisor for one provider, `tokio::spawn`ed by the
/// remote supervisor when the user picks a provider. Loops forever:
/// if the tunnel child dies, the URL is cleared (so clients can tell
/// the URL went stale) and a fresh one is spawned with backoff.
///
/// `adopt_pid != 0` is the `/construct restart` path: a tunnel child
/// spawned by the *previous* daemon survived the `exec()` and is still
/// serving the URL we already restored from the snapshot. Adopt it —
/// poll its liveness rather than spawning a second one — and only fall
/// through to a fresh spawn once it dies. Restarting the daemon must
/// never rotate the user's URL behind their back.
pub async fn run(
    provider: TunnelProvider,
    remote: RemoteState,
    local_port: u16,
    adopt_pid: u32,
    subdomain: Option<String>,
) {
    if provider == TunnelProvider::None {
        return;
    }
    let label = provider.label();

    if let Err(detail) = preflight(provider).await {
        tracing::info!(provider = label, "tunnel unavailable: {detail}");
        return;
    }

    if adopt_pid != 0 && process_alive(adopt_pid) {
        let adopted_url = remote.tunnel_url().await;
        tracing::info!(
            provider = label,
            pid = adopt_pid,
            url = adopted_url.as_deref().unwrap_or("(unknown)"),
            "adopting existing tunnel across restart"
        );
        // The adopted PID is NOT our child (it was the prior daemon's,
        // reparented to init by the new-process-group trick), so we
        // can't `wait()` on it. `kill(pid, 0)` every 2s instead — the
        // polling cost is nothing next to keeping the URL alive.
        while process_alive(adopt_pid) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tracing::warn!(
            provider = label,
            pid = adopt_pid,
            "adopted tunnel exited; spawning fresh"
        );
        remote.set_tunnel_url(None).await;
        remote.set_auth_url(None).await;
        remote.set_tunnel_pid(0).await;
    }

    let mut backoff_secs: u64 = 1;
    let construct_instance_id = uuid::Uuid::new_v4().simple().to_string();
    loop {
        match run_once(
            provider,
            &remote,
            local_port,
            subdomain.as_deref(),
            &construct_instance_id,
        )
        .await
        {
            Ok(()) => {
                tracing::warn!(provider = label, "tunnel exited cleanly; respawning");
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(provider = label, error = %e, "tunnel run failed; backing off");
            }
        }
        remote.set_tunnel_url(None).await;
        remote.set_tunnel_pid(0).await;
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

/// Can this provider start? `Err(detail)` carries a message written
/// for the user, not for the log — it is what the dialog paints under
/// a greyed-out button, and what the start-timeout diagnostic quotes.
pub async fn preflight(provider: TunnelProvider) -> Result<(), String> {
    match provider {
        TunnelProvider::None => Ok(()),
        TunnelProvider::Cloudflare => cloudflare::preflight(),
        TunnelProvider::Construct => construct::preflight(),
    }
}

async fn run_once(
    provider: TunnelProvider,
    remote: &RemoteState,
    local_port: u16,
    _subdomain: Option<&str>,
    construct_instance_id: &str,
) -> anyhow::Result<()> {
    match provider {
        TunnelProvider::None => Ok(()),
        TunnelProvider::Cloudflare => cloudflare::run_once(remote, local_port).await,
        TunnelProvider::Construct => {
            construct::run_once(remote, local_port, construct_instance_id).await
        }
    }
}
