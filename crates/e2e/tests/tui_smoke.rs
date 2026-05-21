//! End-to-end: drive the `agent` TUI inside a pseudo-terminal
//! against a real `agentd`, type a slash command, observe the
//! resulting popup, and quit cleanly.
//!
//! Coverage:
//!
//! - TUI connects to the daemon over IPC and renders the
//!   modeline (so the IPC + render path is exercised).
//! - Command palette opens on `:` (the default keymap binding
//!   for non-orchestrator panels).
//! - `:remote-control debug` submits, which goes through
//!   `run_slash_command` → `Client::remote_start` → the
//!   supervisor and back as a popup.
//! - The popup contents include the expected auth labels.
//! - `Esc` dismisses the popup; `q` exits.

use std::time::Duration;

use agentd_e2e::{Daemon, Tui};

/// Minimal smoke: TUI starts, draws the modeline (IPC + render
/// path), and quits cleanly on `q`. Keeps the bar low for the
/// first TUI e2e — assertions on the slash-command popup go in
/// a separate test once this baseline is stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_starts_and_quits() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn(&d.socket).expect("spawn TUI");

    // Modeline. The format starts with " agentd  focus:" — see
    // `render_modeline` in crates/cli/src/ui.rs.
    tui.wait_for("agentd  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // q is the default Quit chord (see crates/cli/src/keymap.rs).
    tui.send(b"q").expect("send q");
    let status = tui
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("TUI did not exit after q");
    assert!(
        status.success(),
        "TUI exited with non-success status: {:?}",
        status
    );
}

/// Drive the TUI through `remote-control debug` via the command
/// palette and verify the resulting popup renders. Exercises
/// the full path: TUI keypress → keymap chord → palette →
/// `run_slash_command` → `Client::remote_start` → supervisor →
/// `RemoteControlPopup` render.
///
/// `Ctrl-x x` is the default-profile (emacs) palette chord — `:`
/// is the vim-profile alias and would silently no-op under the
/// default keymap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_remote_control_popup_via_palette() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn(&d.socket).expect("spawn TUI");

    tui.wait_for("agentd  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // Ctrl-x then x (the palette chord under the default emacs
    // keymap). Allow the TUI a moment to draw the palette
    // prompt before typing into it.
    tui.send(b"\x18x").expect("send C-x x");
    tokio::time::sleep(Duration::from_millis(200)).await;

    // `remote-control debug` is the local-only path — no
    // cloudflared spawn, so the test doesn't depend on that
    // binary being installed on the CI runner.
    tui.send(b"remote-control debug\r").expect("send command");

    // Header + auth labels rendered by
    // `render_remote_control_popup`. Use substrings that don't
    // depend on the popup's internal alignment (the labels are
    // padded with spaces so a `user: remote` literal would
    // mismatch). The popup title is itself a useful needle.
    tui.wait_for("/remote-control debug", Duration::from_secs(15))
        .await
        .expect("popup title never appeared");
    tui.wait_for("user:", Duration::from_secs(5))
        .await
        .expect("popup user label never appeared");
    tui.wait_for("password:", Duration::from_secs(5))
        .await
        .expect("popup password label never appeared");
    // Sanity: the popup must show some content under those
    // labels — `remote` username + a 127.0.0.1 URL.
    let screen = tui.screen();
    assert!(
        screen.contains("remote"),
        "expected literal 'remote' username in popup, got:\n{screen}"
    );
    assert!(
        screen.contains("127.0.0.1"),
        "expected local URL in popup, got:\n{screen}"
    );

    tui.send(b"\x1b").expect("send Esc");
    tokio::time::sleep(Duration::from_millis(200)).await;
    tui.send(b"q").expect("send q");
    let status = tui
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("TUI did not exit after q");
    assert!(status.success(), "TUI exited with non-success status: {:?}", status);
}
