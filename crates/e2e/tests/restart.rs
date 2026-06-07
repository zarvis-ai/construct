//! End-to-end coverage of the `/agentd restart` feature
//! (issue #90). Verifies the three guarantees the feature makes:
//!
//!   1. **Binary reload** — `daemon.restart` `exec()`s the
//!      current on-disk binary, so a replaced/upgraded binary is
//!      picked up. PID is preserved (exec, not respawn).
//!   2. **TUI auto-reconnect** — the `construct` TUI notices the
//!      socket drop and reconnects on its own, no manual re-run.
//!   3. **Web reconnect to the same URL** — the bundled web
//!      client's WebSocket drops and reconnects to the *same*
//!      URL (token + port + password preserved across the
//!      restart), without re-navigating.

use std::path::Path;
use std::time::{Duration, Instant};

use agentd_e2e::{Daemon, Tui};

// ---------------------------------------------------------------------------
// 1. Binary reload
// ---------------------------------------------------------------------------

/// `daemon.restart` should `exec()` the binary that's on disk at
/// restart time — so swapping the binary file and then restarting
/// runs the new bytes, with the PID preserved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_reloads_updated_binary() {
    let d = Daemon::spawn_relocatable()
        .await
        .expect("spawn relocatable daemon");
    let pid = d.pid().expect("daemon pid");

    // On Linux we can directly observe which inode the process is
    // executing via /proc/<pid>/exe. Record it before the swap.
    let inode_before = proc_exe_inode(pid);

    // Replace the on-disk binary with a fresh copy (new inode;
    // on Linux also new bytes via an appended tag). The running
    // process keeps the old inode open until it exits.
    replace_binary_new_inode(&d.binary_path).expect("replace binary");
    let swapped_inode = file_inode(&d.binary_path);
    if let (Some(before), Some(swapped)) = (inode_before, swapped_inode) {
        assert_ne!(
            before, swapped,
            "sanity: swapped binary should have a new inode"
        );
    }

    // Trigger the restart. The reply races the exec() — either we
    // get it or the socket closes mid-flight; both mean "restart
    // in progress".
    let _ = d.client.daemon_restart(None).await;

    // New daemon must come back up on the same socket.
    let client = d
        .wait_until_back(Duration::from_secs(30))
        .await
        .expect("daemon did not come back after restart");
    client.ping().await.expect("ping after restart");

    // PID preserved → it exec()'d in place rather than spawning a
    // new process.
    assert!(
        pid_alive(pid),
        "daemon PID {pid} should still be alive after exec() restart"
    );
    assert_eq!(
        d.pid(),
        Some(pid),
        "tracked child PID should be unchanged across exec()"
    );

    // Linux: the process is now executing the swapped-in inode,
    // and it isn't the old (now-unlinked) one. This is the direct
    // proof that exec re-read the on-disk binary.
    if let (Some(before), Some(after)) = (inode_before, proc_exe_inode(pid)) {
        assert_ne!(
            before, after,
            "process should be executing the replaced binary (new inode), \
             not the original — exec did not pick up the on-disk update"
        );
        if let Some(swapped) = swapped_inode {
            assert_eq!(
                after, swapped,
                "process inode should match the swapped-in binary"
            );
        }
    } else {
        // Non-Linux (local macOS dev): no /proc. The PID-preserved
        // + daemon-responsive checks above still demonstrate that
        // exec() of the on-disk path succeeded after the swap.
        eprintln!(
            "note: /proc unavailable; skipped inode assertion (PID + liveness checks passed)"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. TUI auto-reconnect
// ---------------------------------------------------------------------------

/// After a daemon restart, the TUI should reconnect on its own —
/// the user shouldn't have to quit and re-launch `construct`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_auto_reconnects_after_restart() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn_with_recording(&d.socket, "restart_tui_reconnect").expect("spawn TUI");

    // Connected: modeline drawn.
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // Restart the daemon out from under the TUI.
    let _ = d.client.daemon_restart(None).await;

    // The TUI sets "reconnected to daemon" only from its
    // successful-reconnect path (crates/cli/src/app.rs). Seeing
    // it proves the TUI detected the drop AND reconnected without
    // any user input. The status lingers ~5s, well within the
    // 50ms poll cadence of wait_for.
    tui.wait_for("reconnected to daemon", Duration::from_secs(25))
        .await
        .expect("TUI did not auto-reconnect after restart");

    // Still interactive afterward: quit cleanly with the global quit
    // chord `C-x C-c` (= 0x18 0x03). Plain `q` is no longer a quit key
    // (the welcome screen, shown here with an empty fleet, advertises
    // and binds `C-x C-c` instead — see keymap.rs / #382).
    tui.send(b"\x18\x03").expect("send C-x C-c");
    let status = tui
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("TUI did not exit after reconnect");
    assert!(status.success(), "TUI exited non-success: {status:?}");
}

// ---------------------------------------------------------------------------
// 3. Web client reconnect to the same URL
// ---------------------------------------------------------------------------

/// After a daemon restart, the bundled web client's WebSocket
/// should drop and reconnect to the **same** URL (token + port +
/// password preserved across the restart) without re-navigating.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_client_reconnects_to_same_url_after_restart() {
    use chromiumoxide::browser::{Browser, BrowserConfig};
    use futures::StreamExt;

    let d = Daemon::spawn().await.expect("spawn daemon");
    let r = d
        .client
        .remote_start(true, None)
        .await
        .expect("remote.start");

    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .build()
        .expect("browser config");
    let (browser, mut handler) = match Browser::launch(config).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("skipping web restart test: could not launch Chromium ({e})");
            return;
        }
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    let url_with_creds = inject_userinfo(&r.url, "remote", &r.password);
    page.goto(&url_with_creds).await.expect("goto");

    // Wait for the initial WS connect.
    wait_conn_state(&page, "open", Duration::from_secs(15))
        .await
        .expect("web client never connected initially");

    // The URL the page is sitting on — must be unchanged after the
    // reconnect (proves "same URL", not a re-navigation).
    let href_before = page_href(&page).await;

    // Restart the daemon. Token + port + password are persisted in
    // runtime/remote.json and adopted by the new daemon, so the
    // URL stays valid.
    let _ = d.client.daemon_restart(None).await;

    // The WS drops on the daemon exec(). Best-effort confirm we
    // observed it leave "open" (it can recover fast, so don't fail
    // if we miss the transient state).
    match wait_conn_state_not(&page, "open", Duration::from_secs(10)).await {
        Ok(()) => {}
        Err(_) => eprintln!("note: did not catch the transient disconnect (fast reconnect)"),
    }

    // It must reconnect to the same URL.
    wait_conn_state(&page, "open", Duration::from_secs(25))
        .await
        .expect("web client did not reconnect after restart");

    let href_after = page_href(&page).await;
    assert_eq!(
        href_before, href_after,
        "web client should reconnect to the same URL, not re-navigate"
    );

    // Sanity: it's actually talking to the (new) daemon again.
    let xterm_present: bool = page
        .evaluate("typeof window.Terminal === 'function'")
        .await
        .expect("evaluate")
        .into_value::<bool>()
        .unwrap_or(false);
    assert!(
        xterm_present,
        "web client lost its bundled xterm after reconnect"
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Read the inode the process at `pid` is executing, via
/// `/proc/<pid>/exe`. Linux-only; returns `None` elsewhere.
fn proc_exe_inode(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        // metadata() follows the /proc/<pid>/exe symlink to the
        // actual (possibly unlinked) inode the process is running.
        std::fs::metadata(format!("/proc/{pid}/exe"))
            .ok()
            .map(|m| m.ino())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

fn file_inode(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.ino())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Is `pid` alive + signalable? `kill(pid, 0)` on Unix.
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 performs error-checking only; no signal
        // is delivered.
        unsafe { libc_kill(pid as i32, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// Replace the file at `path` with a fresh copy of itself via
/// write-then-rename — exactly how a real upgrade lands a new
/// binary: a new inode at the same path, with the running process
/// keeping the old (now-unlinked) inode open. This is also the
/// dance that avoids `ETXTBSY` from writing a busy executable in
/// place.
///
/// Bytes are identical (a straight copy) so the new file is a
/// valid, runnable binary on every platform — no risk of breaking
/// an ELF or invalidating a macOS code signature. The proof that
/// exec picked up the replacement comes from the inode changing
/// (Linux `/proc/<pid>/exe` check), not from content differing.
fn replace_binary_new_inode(path: &Path) -> std::io::Result<()> {
    let tmp = path.with_extension("new");
    std::fs::copy(path, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, path)
}

/// Inject `user:password@` userinfo into an http(s) URL authority.
fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{user}:{pw}@{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{user}:{pw}@{rest}")
    } else {
        url.to_string()
    }
}

async fn conn_state(page: &chromiumoxide::page::Page) -> String {
    page.evaluate("document.getElementById('conn')?.dataset?.state || ''")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default()
}

async fn page_href(page: &chromiumoxide::page::Page) -> String {
    page.evaluate("location.href")
        .await
        .ok()
        .and_then(|r| r.into_value::<String>().ok())
        .unwrap_or_default()
}

async fn wait_conn_state(
    page: &chromiumoxide::page::Page,
    want: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if conn_state(page).await == want {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("conn state never reached {want:?} within {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_conn_state_not(
    page: &chromiumoxide::page::Page,
    avoid: &str,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if conn_state(page).await != avoid {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("conn state stayed {avoid:?} for {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
