//! End-to-end: properties of `remote.start` / `remote.stop` that
//! the TUI and headless-browser smokes don't cover.
//!
//! Specifically:
//!
//!  - **Security**: wrong Basic credentials → 401 (i.e. the auth
//!    gate isn't accidentally an open door).
//!  - **Lifecycle**: `remote.stop` is idempotent — first call
//!    reports `was_running: true`, repeat calls report `false`
//!    instead of erroring.
//!  - **Persistence**: the `runtime/remote.json` snapshot file is
//!    written during start and deleted by stop. That snapshot is
//!    load-bearing for the `/agentd restart` URL-preservation
//!    adoption path — if either side regresses, restart silently
//!    rotates the URL.
//!  - **Teardown**: after `remote.stop`, the listener is actually
//!    gone (not just marked stopped) — subsequent HTTP requests
//!    can't connect.
//!
//! Happy-path coverage (HTTP 200 + HTML body + JS boot + WS
//! upgrade) lives in `web_smoke.rs`; this test stays cheap and
//! Chrome-free so it still runs on dev machines without a
//! browser and catches HTTP-layer regressions on every CI run.
//!
//! Uses `local_only=true` so the test never depends on
//! cloudflared being installed or reachable.

use std::time::Duration;

use construct_e2e::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_control_security_and_lifecycle() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    let r = d
        .client
        .remote_start(/* local_only */ true, /* password */ None)
        .await
        .expect("remote.start");
    assert!(
        r.url.starts_with("http://127.0.0.1:"),
        "expected local URL, got {}",
        r.url
    );
    assert!(
        !r.password.is_empty(),
        "auto-gen password should be non-empty"
    );

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let root = format!("{}/", r.url);

    // Security gate: wrong password → 401. If this ever 200s, the
    // Basic-auth check is broken and the whole remote-control
    // model collapses.
    let bad = http
        .get(&root)
        .basic_auth("remote", Some("not-the-password"))
        .send()
        .await
        .expect("http get bad pw");
    assert_eq!(bad.status().as_u16(), 401, "wrong pw should be 401");

    // Snapshot file is written under runtime_dir — that's what
    // the `/agentd restart` adoption path reads to rehydrate the
    // token + password + port.
    let snap = d.dir.path().join("run/remote.json");
    assert!(snap.exists(), "expected snapshot at {}", snap.display());
    let snap_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert_eq!(snap_json["password"].as_str().unwrap(), r.password);
    assert!(snap_json["port"].as_u64().unwrap() > 0);

    // Lifecycle: first stop reports `was_running: true`; second
    // stop is idempotent (`was_running: false`) instead of an
    // error.
    let stop1 = d.client.remote_stop().await.expect("remote.stop #1");
    assert!(stop1.was_running, "first stop should report was_running");
    let stop2 = d.client.remote_stop().await.expect("remote.stop #2");
    assert!(!stop2.was_running, "second stop should be idempotent");

    // Snapshot is deleted after stop so the next daemon boot
    // doesn't try to adopt a tunnel that no longer exists.
    assert!(
        !snap.exists(),
        "snapshot should be removed after remote.stop, still exists at {}",
        snap.display()
    );

    // Teardown: the listener is actually gone — request can't
    // connect, or comes back as a server error.
    let after = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await;
    assert!(
        after.is_err()
            || after
                .ok()
                .map(|r| r.status().is_server_error() || r.status().as_u16() == 502)
                .unwrap_or(false),
        "expected post-stop request to fail or 5xx"
    );
}
