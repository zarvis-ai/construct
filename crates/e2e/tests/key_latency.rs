//! Benchmark: keystroke → screen-update latency for **held /
//! repeated keys** in a TUI session view (backspace across a long
//! line, left-arrow across long text). This is the "is typing
//! snappy?" measurement.
//!
//! `#[ignore]` by default — it's a timing benchmark, not a
//! pass/fail correctness test, and the absolute numbers are
//! hardware-dependent. Run it explicitly:
//!
//!   cargo test -p agentd-e2e --test key_latency -- --ignored --nocapture
//!
//! It drives the real `construct` TUI in a PTY against a real daemon
//! running an interactive shell session (`$SHELL -il` via the
//! shell adapter), so it exercises the entire pipeline:
//!
//!   crossterm key → on_key → queue_pty_input → pump → IPC →
//!   daemon → shell PTY → output → notification → vt100 → render
//!
//! and reports wall-clock settle time for a burst of repeated
//! keys.

use std::time::{Duration, Instant};

use agentd_e2e::{Daemon, Tui};
use agentd_protocol::CreateSessionParams;

const MARKER_LEN: usize = 40;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "timing benchmark; run with --ignored --nocapture"]
async fn repeated_key_latency() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    // Interactive shell session — `$SHELL -il`, a real readline
    // PTY that does line editing (backspace, cursor moves).
    let cwd = d.dir.path().to_string_lossy().to_string();
    let session_id = d
        .client
        .create(CreateSessionParams {
            harness: "shell".into(),
            cwd,
            prompt: None,
            model: None,
            title: Some("bench".into()),
            mode: None,
            pty_size: None,
            worktree: false,
            env: std::collections::HashMap::new(),
            args: Vec::new(),
            kind: Default::default(),
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
            forked_from: None,
        })
        .await
        .expect("create shell session");
    eprintln!("created shell session {session_id}");

    let mut tui = Tui::spawn_with_recording(&d.socket, "key_latency").expect("spawn TUI");
    tui.wait_for("construct  focus:", Duration::from_secs(15))
        .await
        .expect("modeline");

    // The session shows in the list; select + drill into its view
    // so keystrokes are captured as pty_input. C-n selects the
    // (only) session, Enter focuses the view pane. Give the shell
    // PTY time to spawn + paint its prompt before we type.
    tui.send(b"\x0e").ok(); // C-n → NextSession (select first)
    tokio::time::sleep(Duration::from_millis(400)).await;
    tui.send(b"\r").ok(); // Enter → FocusView
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Confirm we're in PTY-capture mode: type a few probe chars
    // and see the shell echo them. Count-based so a wrapped line
    // in the narrow view doesn't fool a substring check. Retry a
    // focus attempt if the probe doesn't land.
    if !probe_capture(&mut tui).await {
        tui.send(b"\x15").ok(); // ^U clear any partial line
        tui.send(b"\r").ok();
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            probe_capture(&mut tui).await,
            "could not enter PTY-capture mode; screen was:\n{}",
            tui.screen()
        );
    }
    // Clear the probe text (^U kills the readline line).
    tui.send(b"\x15").ok();
    tokio::time::sleep(Duration::from_millis(300)).await;

    eprintln!("\n=== repeated-key latency ({MARKER_LEN} keys/burst, min of {REPS} reps) ===");

    // Each burst is deterministic (waits for a definitive end
    // state, not a stability heuristic). Repeat and take the min —
    // the cleanest run, least perturbed by scheduler noise.
    let mut typ = Vec::new();
    let mut del = Vec::new();
    let mut arr = Vec::new();
    for _ in 0..REPS {
        clear_line(&mut tui).await;
        typ.push(measure_type_burst(&mut tui).await);
        del.push(measure_delete_burst(&mut tui).await);
        clear_line(&mut tui).await;
        arr.push(measure_arrow_burst(&mut tui).await);
        clear_line(&mut tui).await;
    }

    eprintln!("\n=== summary (min / median ms; ms per key) ===");
    report("type     ", &mut typ);
    report("backspace", &mut del);
    report("left-arrow", &mut arr);

    // Quit cleanly.
    tui.send(b"\x18\x03").ok(); // C-x C-c
    let _ = tui.wait_exit(Duration::from_secs(5)).await;
}

const REPS: usize = 5;

fn report(label: &str, samples: &mut [Duration]) {
    samples.sort();
    let min = samples[0];
    let median = samples[samples.len() / 2];
    eprintln!(
        "{label}: min {:.1}ms  median {:.1}ms  ({:.2} ms/key at min)",
        min.as_secs_f64() * 1000.0,
        median.as_secs_f64() * 1000.0,
        min.as_secs_f64() * 1000.0 / MARKER_LEN as f64,
    );
}

/// ^U kills the readline line; wait until the markers are gone.
async fn clear_line(tui: &mut Tui) {
    tui.send(b"\x15").ok();
    let _ = wait_until(
        tui,
        |s| count_char(s, 'Z') == 0 && count_char(s, 'Y') == 0 && count_char(s, 'Q') == 0,
        Duration::from_secs(5),
    )
    .await;
}

/// Type a handful of probe chars ('Q') and return true once the
/// shell echoes them. Count-based, so a wrapped readline line in
/// the narrow view pane doesn't break the check the way a
/// substring match would.
async fn probe_capture(tui: &mut Tui) -> bool {
    const N: usize = 5;
    for _ in 0..N {
        tui.send(b"Q").ok();
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if count_char(&tui.screen(), 'Q') >= N {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Total count of `c` on screen. Robust to the readline line
/// wrapping across rows in the narrow view pane (a consecutive-run
/// count would be split by the inter-row newline; a total count
/// isn't). Marker chars ('Z'/'Y') are picked to not appear in the
/// shell prompt.
fn count_char(screen: &str, c: char) -> usize {
    screen.chars().filter(|ch| *ch == c).count()
}

/// Forward typing: burst MARKER_LEN 'Z's from an empty line, time
/// until all are echoed. Deterministic (count reaches MARKER_LEN).
async fn measure_type_burst(tui: &mut Tui) -> Duration {
    let t0 = Instant::now();
    for _ in 0..MARKER_LEN {
        tui.send(b"Z").ok();
    }
    wait_until(
        tui,
        |s| count_char(s, 'Z') >= MARKER_LEN,
        Duration::from_secs(30),
    )
    .await
    .expect("type burst never fully echoed");
    t0.elapsed()
}

/// Delete burst: precondition MARKER_LEN 'Z's on the line. Send
/// MARKER_LEN backspaces, time until gone. 0x7f is DEL (Backspace)
/// — crossterm maps it to KeyCode::Backspace, forwarded to the PTY.
async fn measure_delete_burst(tui: &mut Tui) -> Duration {
    let t0 = Instant::now();
    for _ in 0..MARKER_LEN {
        tui.send(b"\x7f").ok();
    }
    wait_until(tui, |s| count_char(s, 'Z') == 0, Duration::from_secs(30))
        .await
        .expect("backspaces never cleared the marker");
    t0.elapsed()
}

/// Left-arrow burst: type a marker, then send MARKER_LEN left
/// arrows followed by a sentinel 'Q'. Keystrokes are processed
/// in order, so the sentinel echoing proves all the arrows were
/// processed — a deterministic end state, no stability heuristic.
async fn measure_arrow_burst(tui: &mut Tui) -> Duration {
    for _ in 0..MARKER_LEN {
        tui.send(b"Y").ok();
    }
    wait_until(
        tui,
        |s| count_char(s, 'Y') >= MARKER_LEN,
        Duration::from_secs(10),
    )
    .await
    .expect("arrow marker never rendered");

    let t0 = Instant::now();
    for _ in 0..MARKER_LEN {
        tui.send(b"\x1b[D").ok(); // ESC [ D = Left arrow
    }
    tui.send(b"Q").ok(); // sentinel — echoes only after all arrows
    wait_until(tui, |s| count_char(s, 'Q') >= 1, Duration::from_secs(30))
        .await
        .expect("arrow burst sentinel never echoed");
    t0.elapsed()
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
