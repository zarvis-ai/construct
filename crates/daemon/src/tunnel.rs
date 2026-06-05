//! Cloudflared quick-tunnel supervisor.
//!
//! When the daemon starts the remote WS listener and `cloudflared`
//! is on PATH, this task spawns `cloudflared tunnel --url
//! http://127.0.0.1:<port>` as a subprocess, scrapes the
//! `*.trycloudflare.com` URL out of cloudflared's stderr banner,
//! and stores the resulting browser URL on [`RemoteState`]. The full URL
//! (plus a terminal-rendered QR
//! code) is also logged so the user can scan it from a phone —
//! this is what makes the Phase 1 "just works" remote experience
//! actually just work.
//!
//! If `cloudflared` isn't installed, we log an info-level hint
//! pointing at the install instructions and keep running with
//! localhost-only WS — useful for developers testing the
//! transport over `ssh -L` or similar.
//!
//! The tunnel is automatically respawned with exponential backoff
//! if cloudflared exits — quick tunnels are intentionally
//! ephemeral, so a long-running daemon will see the URL rotate.
//! Each new URL is published on the same `RemoteState`.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::remote::{process_alive, RemoteState};

/// Long-running supervisor. Loops forever; designed to be
/// `tokio::spawn`ed once from `main` alongside the WS listener.
/// On cloudflared death the tunnel URL is cleared on `remote`
/// before respawning so connected clients can tell the URL is
/// stale.
///
/// `adopt_pid != 0`: skip the initial spawn and instead watch
/// the already-running cloudflared with that PID — the
/// `/agentd restart` path. cloudflared was spawned by the prior
/// daemon in its own process group, survived the daemon's
/// `exec()`, and the URL it terminates is still valid. We just
/// poll its liveness; once it dies, fall through to the spawn
/// loop with a fresh URL.
pub async fn run(remote: RemoteState, local_port: u16, adopt_pid: u32) {
    if which::which("cloudflared").is_err() {
        tracing::info!(
            "cloudflared not found on PATH; remote tunnel disabled. \
             install with `brew install cloudflared` (or download from \
             github.com/cloudflare/cloudflared/releases) to expose this \
             daemon over the internet."
        );
        return;
    }

    if adopt_pid != 0 && process_alive(adopt_pid) {
        let adopted_url = remote.tunnel_url().await;
        tracing::info!(
            pid = adopt_pid,
            url = adopted_url.as_deref().unwrap_or("(unknown)"),
            "adopting existing cloudflared tunnel across restart"
        );
        // The adopted PID is NOT a child of this daemon (it was
        // a child of the prior daemon, now reparented to init
        // because of `setsid`). We can't `wait()` on it, so we
        // poll with `kill(pid, 0)` every 2s. The polling overhead
        // is negligible compared to keeping the URL alive.
        while process_alive(adopt_pid) {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        tracing::warn!(
            pid = adopt_pid,
            "adopted cloudflared exited; spawning fresh"
        );
        remote.set_tunnel_url(None).await;
        remote.set_tunnel_pid(0).await;
    }

    let mut backoff_secs: u64 = 1;
    loop {
        match run_once(&remote, local_port).await {
            Ok(()) => {
                tracing::warn!("cloudflared exited cleanly; respawning");
                backoff_secs = 1;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cloudflared run failed; backing off");
            }
        }
        remote.set_tunnel_url(None).await;
        remote.set_tunnel_pid(0).await;
        tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        backoff_secs = (backoff_secs * 2).min(30);
    }
}

async fn run_once(remote: &RemoteState, local_port: u16) -> Result<()> {
    let mut child = Command::new("cloudflared")
        .args([
            "tunnel",
            "--no-autoupdate",
            "--url",
            &format!("http://127.0.0.1:{local_port}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        // Detach into a new process group so cloudflared survives
        // the daemon's `exec()` on `/agentd restart`. With this,
        // the new daemon adopts the still-running subprocess and
        // the public URL stays valid across the restart.
        //
        // `kill_on_drop` defaults to false — we explicitly SIGTERM
        // the recorded PID from `handle_stop` / signal-handlers
        // when we actually want it gone. A daemon SIGKILL still
        // leaks the subprocess; that's the trade-off for URL
        // preservation, and the next daemon boot's "stale
        // snapshot" check sweeps any orphans.
        .process_group(0)
        .spawn()
        .context("spawn cloudflared")?;
    let pid = child.id().unwrap_or(0);
    if pid != 0 {
        remote.set_tunnel_pid(pid).await;
    }

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("cloudflared stderr not captured"))?;

    let scan_remote = remote.clone();
    let scan_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut announced = false;
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "cloudflared", "{line}");
            if !announced {
                if let Some(https_url) = extract_trycloudflare_url(&line) {
                    let host_path = https_url.trim_start_matches("https://");
                    // The URL a user scans (or types) is the
                    // browser-facing https:// form: the HTML at
                    // that URL boots a tiny JS app that swaps
                    // `http(s)` → `ws(s)` for its WebSocket
                    // connection back to the daemon. Pointing the
                    // QR at the `wss://` form makes the QR
                    // unscannable in any browser-camera flow,
                    // which is what bit us in the first phone
                    // test.
                    let browser_url = format!("https://{host_path}/");
                    let ws_url = format!("wss://{host_path}/");
                    // The QR + URL render in the TUI's `/remote-control` modal
                    // and the webui — no need to dump them to the daemon's
                    // stdout where every restart re-paints a full-screen QR
                    // into the user's scrollback. The structured log line below
                    // keeps both URLs discoverable for `tail -f` / journalctl.
                    tracing::info!(
                        browser = %browser_url,
                        wss = %ws_url,
                        "remote tunnel ready"
                    );
                    scan_remote.set_tunnel_url(Some(browser_url)).await;
                    announced = true;
                }
            }
            // Continue draining so the subprocess never blocks on
            // a full stderr pipe.
        }
    });

    let status = child.wait().await.context("wait cloudflared")?;
    scan_task.abort();
    if !status.success() {
        return Err(anyhow!("cloudflared exited: {status}"));
    }
    Ok(())
}

/// Scan a single line of cloudflared output for the public quick-
/// tunnel URL. Looks for the `https://<sub>.trycloudflare.com`
/// shape; ignores any other URLs in the banner. Returns `None`
/// when the line has no match.
fn extract_trycloudflare_url(line: &str) -> Option<String> {
    let start = line.find("https://")?;
    let rest = &line[start..];
    // Trim at the first whitespace / control char — cloudflared's
    // banner pads the URL with spaces and box-drawing characters,
    // so we can't just take to end-of-line.
    let end = rest
        .find(|c: char| c.is_whitespace() || c.is_control())
        .unwrap_or(rest.len());
    let candidate = &rest[..end];
    // Tolerate trailing punctuation cloudflared sometimes inserts.
    let trimmed = candidate.trim_end_matches(|c: char| !(c.is_alphanumeric() || c == '/'));
    if trimmed.contains(".trycloudflare.com") {
        Some(trimmed.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parses the URL out of a typical cloudflared banner line.
    /// The real banner is multi-line ASCII art so we test a few
    /// representative shapes.
    #[test]
    fn extracts_url_from_banner_line() {
        let line = "2026-05-19 INF +-------------------------------------+";
        assert_eq!(extract_trycloudflare_url(line), None);

        let line = "2026-05-19 INF | https://big-fox-42.trycloudflare.com |";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://big-fox-42.trycloudflare.com"),
        );

        let line = "Visit https://abc-def.trycloudflare.com to access your tunnel";
        assert_eq!(
            extract_trycloudflare_url(line).as_deref(),
            Some("https://abc-def.trycloudflare.com"),
        );

        // Non-trycloudflare URLs (cloudflared sometimes mentions
        // its own homepage) are ignored.
        let line = "Documentation: https://developers.cloudflare.com/argo-tunnel/";
        assert_eq!(extract_trycloudflare_url(line), None);
    }
}
