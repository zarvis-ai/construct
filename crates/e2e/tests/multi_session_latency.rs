//! Repro for the "multiple large sessions loaded → typing lags"
//! regression: the TUI feeds EVERY session's PTY output into its
//! per-session history on the main event loop (app.rs
//! `on_notification`), plus a shadow parser + matrix-rain word
//! harvest per byte. When several background sessions stream
//! heavily, that feed work competes with input handling, so held
//! keys in the focused session queue up and the cursor jumps.
//!
//! Measures focused-session repeated-key settle latency with no
//! background activity vs. with N background sessions flooding,
//! so the per-background-session cost is visible.
//!
//!   cargo test -p agentd-e2e --release --test multi_session_latency -- --ignored --nocapture
//!
//! `#[ignore]` (timing benchmark; hardware-dependent).

use std::time::{Duration, Instant};

use agentd_e2e::{Daemon, Tui};
use agentd_protocol::CreateSessionParams;

const BG_SESSIONS: usize = 4;
const MARKER_LEN: usize = 40;
/// Each background session streams this many lines — substantial
/// and overlapping the measurement, but not an infinite firehose
/// that saturates everything (which makes the measurement racy).
const FLOOD_LINES: u64 = 300_000;

fn shell_params(cwd: &str, title: &str) -> CreateSessionParams {
    CreateSessionParams {
        harness: "shell".into(),
        cwd: cwd.into(),
        prompt: None,
        model: None,
        title: Some(title.into()),
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
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
async fn typing_latency_under_background_floods() {
    let d = Daemon::spawn().await.expect("daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();

    // Focused session for typing.
    let focused = d
        .client
        .create(shell_params(&cwd, "focused"))
        .await
        .expect("create focused");

    // Background sessions (created but idle for now).
    let mut bg = Vec::new();
    for i in 0..BG_SESSIONS {
        bg.push(
            d.client
                .create(shell_params(&cwd, &format!("bg{i}")))
                .await
                .expect("create bg"),
        );
    }

    let mut tui = Tui::spawn_with_recording(&d.socket, "multi_session_latency").expect("spawn TUI");
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline");

    // Select + focus the FIRST session (the focused one). C-n once.
    tui.send(b"\x0e").ok();
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.send(b"\r").ok();
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        probe(&mut tui).await,
        "PTY capture not active:\n{}",
        tui.screen()
    );
    tui.send(b"\x15").ok(); // clear line
    tokio::time::sleep(Duration::from_millis(300)).await;

    // --- Baseline: no background activity ---
    let baseline = measure_backspace_burst(&mut tui)
        .await
        .expect("baseline should always settle");
    eprintln!("baseline done: {:.1} ms", baseline.as_secs_f64() * 1000.0);

    // --- Start floods in the background sessions via IPC (bypassing
    //     the TUI, which only types into the focused session). ---
    for id in &bg {
        let _ = d
            .client
            .pty_input(id, format!("seq 1 {FLOOD_LINES}\r").into_bytes())
            .await;
    }
    // Let the floods ramp up.
    tokio::time::sleep(Duration::from_secs(1)).await;

    // --- Under load: same measurement while backgrounds stream ---
    let under_load = measure_backspace_burst(&mut tui).await;

    eprintln!("\n=== focused typing: backspace x{MARKER_LEN} settle ===");
    eprintln!(
        "baseline (idle bg):        {:.1} ms",
        baseline.as_secs_f64() * 1000.0
    );
    match under_load {
        Some(d) => eprintln!(
            "under {BG_SESSIONS} flooding bg sessions: {:.1} ms  ({:.1}x)",
            d.as_secs_f64() * 1000.0,
            d.as_secs_f64() / baseline.as_secs_f64().max(1e-6),
        ),
        None => eprintln!(
            "under {BG_SESSIONS} flooding bg sessions: SATURATED (>{}s — focused \
             input starved by background-session feed work)",
            UNDER_LOAD_CAP.as_secs(),
        ),
    }
    let _ = focused;

    tui.send(b"\x18\x03").ok();
    let _ = tui.wait_exit(Duration::from_secs(5)).await;
}

fn count_char(s: &str, c: char) -> usize {
    s.chars().filter(|x| *x == c).count()
}

async fn probe(tui: &mut Tui) -> bool {
    for _ in 0..5 {
        tui.send(b"Q").ok();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if count_char(&tui.screen(), 'Q') >= 5 {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

const UNDER_LOAD_CAP: Duration = Duration::from_secs(30);

/// Type a marker, burst-delete it, time until cleared. Returns
/// `None` if it doesn't settle within the cap (the loop is so
/// starved by background feed work that focused input barely
/// progresses — the severe form of the regression).
async fn measure_backspace_burst(tui: &mut Tui) -> Option<Duration> {
    for _ in 0..MARKER_LEN {
        tui.send(b"Z").ok();
    }
    wait_until(tui, |s| count_char(s, 'Z') >= MARKER_LEN, UNDER_LOAD_CAP)
        .await
        .ok()?;
    let t0 = Instant::now();
    for _ in 0..MARKER_LEN {
        tui.send(b"\x7f").ok();
    }
    wait_until(tui, |s| count_char(s, 'Z') == 0, UNDER_LOAD_CAP)
        .await
        .ok()?;
    Some(t0.elapsed())
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
