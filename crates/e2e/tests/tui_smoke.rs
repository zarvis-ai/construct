//! End-to-end: drive the `construct` TUI inside a pseudo-terminal
//! against a real `agentd`, type a slash command, observe the
//! resulting popup, and quit cleanly.
//!
//! Coverage:
//!
//! - TUI connects to the daemon over IPC and renders the
//!   modeline (so the IPC + render path is exercised).
//! - Command palette opens on `:` (the default keymap binding
//!   for non-orchestrator panels).
//! - `:remote-control` submits, which goes through
//!   `run_slash_command` → `Client::remote_start` → the
//!   supervisor and back as a popup.
//! - The popup contents include the expected auth labels and the
//!   provider buttons, and no tunnel was started to get them.
//! - `Esc` dismisses the popup; `q` exits.

use std::time::Duration;

use construct_e2e::{Daemon, Tui};

/// Minimal smoke: TUI starts, draws the modeline (IPC + render
/// path), and quits cleanly on `q`. Keeps the bar low for the
/// first TUI e2e — assertions on the slash-command popup go in
/// a separate test once this baseline is stable.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_starts_and_quits() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn_with_recording(&d.socket, "tui_starts_and_quits").expect("spawn TUI");

    // Modeline. The format starts with " construct  focus:" — see
    // `render_modeline` in crates/cli/src/ui.rs.
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // C-x C-c (0x18 0x03) is the global Quit chord — see
    // crates/cli/src/keymap.rs (plain `q` is no longer bound to Quit).
    tui.send(b"\x18\x03").expect("send C-x C-c");
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

/// Drive the TUI through `remote-control` via the command palette and
/// verify the resulting dialog renders. Exercises the full path: TUI
/// keypress → keymap chord → palette → `run_slash_command` →
/// `Client::remote_start` → supervisor → `RemoteControlPopup` render.
///
/// The dialog must open in its no-tunnel resting state, so this test
/// never depends on cloudflared or tailscale being installed on the
/// runner — and if opening it ever starts a tunnel again, the daemon
/// would block for 15s here and the test would time out.
///
/// `Ctrl-x x` is the default-profile (emacs) palette chord — `:`
/// is the vim-profile alias and would silently no-op under the
/// default keymap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_remote_control_popup_via_palette() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn_with_recording(&d.socket, "tui_remote_control_popup_via_palette")
        .expect("spawn TUI");

    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline never rendered");

    // Ctrl-x then x (the palette chord under the default emacs
    // keymap). Allow the TUI a moment to draw the palette
    // prompt before typing into it.
    tui.send(b"\x18x").expect("send C-x x");
    tokio::time::sleep(Duration::from_millis(200)).await;

    tui.send(b"remote-control\r").expect("send command");

    // Header + auth labels rendered by
    // `render_remote_control_popup`. Use substrings that don't
    // depend on the popup's internal alignment (the labels are
    // padded with spaces so a `user: remote` literal would
    // mismatch). The popup title is itself a useful needle.
    tui.wait_for("/remote-connect", Duration::from_secs(15))
        .await
        .expect("popup title never appeared");
    tui.wait_for("user:", Duration::from_secs(5))
        .await
        .expect("popup user label never appeared");
    tui.wait_for("password:", Duration::from_secs(5))
        .await
        .expect("popup password label never appeared");
    // The dialog offers a way out of the local network — the Cloudflare
    // button, shown even where cloudflared isn't installed.
    tui.wait_for("Cloudflare", Duration::from_secs(5))
        .await
        .expect("cloudflare option never appeared");
    // Sanity: the popup must show some content under those
    // labels — `remote` username + the loopback URL, which is always
    // rendered whether or not this host has a LAN address.
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
    tui.send(b"\x18\x03").expect("send C-x C-c"); // global Quit chord
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

/// The stopped remote-control status is a mouse-first discovery path. Clicking
/// it starts the local listener and opens the same chooser as
/// `/remote-connect`; after dismissing the dialog, the status remains visible
/// with a zero-client count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_remote_status_click_opens_popup_and_remains_at_zero_clients() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let mut tui = Tui::spawn_with_recording(
        &d.socket,
        "tui_remote_status_click_opens_popup_and_remains_at_zero_clients",
    )
    .expect("spawn TUI");

    tui.wait_for("○ remote", Duration::from_secs(15))
        .await
        .expect("stopped remote status never rendered");
    let screen = tui.screen();
    let (row, col) = screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| line.find("○ remote").map(|col| (row, col)))
        .expect("remote status coordinates");
    let mouse_down = format!("\x1b[<0;{};{}M", col + 1, row + 1);
    let mouse_up = format!("\x1b[<0;{};{}m", col + 1, row + 1);
    tui.send(mouse_down.as_bytes()).expect("mouse down");
    tui.send(mouse_up.as_bytes()).expect("mouse up");

    tui.wait_for("/remote-connect", Duration::from_secs(15))
        .await
        .expect("clicking remote status did not open dialog");
    tui.send(b"\x1b").expect("dismiss remote dialog");
    tui.wait_for_absence("/remote-connect", Duration::from_secs(5))
        .await
        .expect("remote dialog did not close");
    tui.wait_for("● remote:0", Duration::from_secs(5))
        .await
        .expect("active zero-client remote status never rendered");

    tui.send(b"\x18\x03").expect("send C-x C-c");
    let status = tui
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("TUI did not exit");
    assert!(status.success(), "TUI exited unsuccessfully: {status:?}");

    // A fresh TUI subscription must receive the listener snapshot immediately;
    // there may be no further client-count transition to trigger a broadcast.
    let mut reconnected = Tui::spawn_with_recording(
        &d.socket,
        "tui_remote_status_snapshot_after_reconnect",
    )
    .expect("spawn reconnected TUI");
    reconnected
        .wait_for("● remote:0", Duration::from_secs(15))
        .await
        .expect("reconnected TUI did not receive remote state snapshot");
    reconnected
        .send(b"\x18\x03")
        .expect("quit reconnected TUI");
    let status = reconnected
        .wait_exit(Duration::from_secs(5))
        .await
        .expect("reconnected TUI did not exit");
    assert!(
        status.success(),
        "reconnected TUI exited unsuccessfully: {status:?}"
    );
}
