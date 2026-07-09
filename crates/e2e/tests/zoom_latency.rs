//! Benchmark: TUI freeze on zoom / unzoom / list hide-show with a
//! long-scrollback session (issue #230).
//!
//! Toggling zoom changes the view pane width. vt100 can't reflow
//! soft-wrapped lines on `set_size`, so the PTY history is
//! re-parsed at the new width — and the pre-fix code re-fed the
//! *entire* session history synchronously on the event loop,
//! freezing the UI for sessions with lots of scrollback.
//!
//! `#[ignore]` (timing benchmark; numbers are hardware-dependent):
//!
//!   cargo test -p agentd-e2e --release --test zoom_latency -- --ignored --nocapture
//!
//! Drives the real TUI in a PTY against a real daemon running an
//! interactive shell, floods it with N lines of output to build a
//! deep history, then times a zoom toggle (which is what freezes).

use std::time::{Duration, Instant};

use agentd_e2e::{Daemon, Tui};
use agentd_protocol::CreateSessionParams;

/// Lines of scrollback history to build before measuring. Well
/// past SCROLLBACK_MAX (5000) so the "re-feed the whole history"
/// bug is exercised, not just the retained tail.
const HISTORY_LINES: usize = 80_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
async fn zoom_toggle_latency() {
    let d = Daemon::spawn().await.expect("spawn daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();
    let _sid = d
        .client
        .create(CreateSessionParams {
            harness: "shell".into(),
            cwd,
            prompt: None,
            model: None,
            title: Some("zoombench".into()),
            mode: None,
            pty_size: None,
            worktree: false,
            env: std::collections::HashMap::new(),
            args: Vec::new(),
            kind: Default::default(),
            group_id: None,
            parent_session_id: None,
            position_after_session_id: None,
            forked_from: None,
        })
        .await
        .expect("create shell session");

    let mut tui = Tui::spawn_with_recording(&d.socket, "zoom_latency").expect("spawn TUI");
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline");

    // Select + focus the session view.
    tui.send(b"\x0e").ok(); // C-n
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.send(b"\r").ok(); // Enter → focus view
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Build a deep history: print HISTORY_LINES numbered lines.
    // Wait until the last line shows, proving the whole stream was
    // ingested into the TUI's per-session history.
    let sentinel = HISTORY_LINES.to_string();
    tui.send(format!("seq 1 {HISTORY_LINES}\r").as_bytes()).ok();
    tui.wait_for(&sentinel, Duration::from_secs(60))
        .await
        .expect("history flood never finished");
    // Let the final chunks settle into items.
    tokio::time::sleep(Duration::from_millis(500)).await;

    eprintln!("\n=== zoom latency ({HISTORY_LINES}-line history, min of {REPS}) ===");

    let mut zoom = Vec::new();
    let mut unzoom = Vec::new();
    for _ in 0..REPS {
        // Zoom the view (list hides). C-x z.
        let t = Instant::now();
        tui.send(b"\x18z").ok();
        wait_until(&tui, |s| !s.contains("sessions"), Duration::from_secs(20))
            .await
            .expect("zoom never hid the list");
        zoom.push(t.elapsed());

        // Unzoom (list returns).
        let t = Instant::now();
        tui.send(b"\x18z").ok();
        wait_until(&tui, |s| s.contains("sessions"), Duration::from_secs(20))
            .await
            .expect("unzoom never restored the list");
        unzoom.push(t.elapsed());
    }

    eprintln!("\n=== summary (min / median ms) ===");
    report("zoom  ", &mut zoom);
    report("unzoom", &mut unzoom);

    tui.send(b"\x18\x03").ok(); // C-x C-c
    let _ = tui.wait_exit(Duration::from_secs(5)).await;
}

const REPS: usize = 5;

fn report(label: &str, samples: &mut [Duration]) {
    samples.sort();
    eprintln!(
        "{label}: min {:.1}ms  median {:.1}ms",
        samples[0].as_secs_f64() * 1000.0,
        samples[samples.len() / 2].as_secs_f64() * 1000.0,
    );
}

async fn wait_until(
    tui: &Tui,
    pred: impl Fn(&str) -> bool,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if pred(&tui.screen()) {
            return Ok(());
        }
        if Instant::now() > deadline {
            anyhow::bail!("wait_until timed out");
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
