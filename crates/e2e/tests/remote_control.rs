//! End-to-end: spawn a real `agentd`, drive `remote.start` /
//! `remote.stop` over IPC, and verify the local WS / HTTP
//! listener actually serves the web client behind HTTP Basic
//! auth.
//!
//! Uses `local_only=true` so the test never depends on
//! cloudflared being installed or reachable. The path the real
//! production flow exercises (cloudflared → public URL) is the
//! same code with one extra subprocess; preserving that
//! happens in `remote_supervisor`'s unit tests + manual phone
//! testing.

use std::time::Duration;

use agentd_e2e::Daemon;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_control_local_round_trip() {
    let d = Daemon::spawn().await.expect("spawn daemon");

    // 1. Start in local-only mode.
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
    assert!(!r.password.is_empty(), "auto-gen password should be non-empty");
    // local_only is reported as "not tunnel ready" — that's the
    // signal to the caller that this is the debug path.
    assert!(!r.tunnel_ready, "local_only should not report tunnel_ready");

    // 2. Right credentials → 200 + HTML body that the phone
    //    client would render.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let root = format!("{}/", r.url);
    let resp = http
        .get(&root)
        .basic_auth("remote", Some(&r.password))
        .send()
        .await
        .expect("http get");
    assert_eq!(resp.status().as_u16(), 200, "expected 200 OK at {root}");
    let body = resp.text().await.expect("body");
    assert!(
        body.contains("<html") || body.contains("<!DOCTYPE"),
        "body did not look like HTML (first 200 chars): {}",
        &body[..body.len().min(200)]
    );

    // 3. Wrong credentials → 401.
    let bad = http
        .get(&root)
        .basic_auth("remote", Some("not-the-password"))
        .send()
        .await
        .expect("http get bad pw");
    assert_eq!(bad.status().as_u16(), 401, "wrong pw should be 401");

    // 4. Snapshot file written under runtime_dir — that's what
    //    the `/agentd restart` adoption path reads.
    let snap = d.dir.path().join("run/remote.json");
    assert!(snap.exists(), "expected snapshot at {}", snap.display());
    let snap_json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&snap).unwrap()).unwrap();
    assert_eq!(snap_json["password"].as_str().unwrap(), r.password);
    assert!(snap_json["port"].as_u64().unwrap() > 0);

    // 5. Stop. The first stop reports was_running=true; a second
    //    stop is idempotent and reports was_running=false.
    let stop1 = d.client.remote_stop().await.expect("remote.stop #1");
    assert!(stop1.was_running, "first stop should report was_running");
    let stop2 = d.client.remote_stop().await.expect("remote.stop #2");
    assert!(!stop2.was_running, "second stop should be idempotent");

    // 6. After stop, the snapshot is deleted (so the next daemon
    //    boot doesn't try to adopt a tunnel that no longer
    //    exists).
    assert!(
        !snap.exists(),
        "snapshot should be removed after remote.stop, still exists at {}",
        snap.display()
    );

    // 7. After stop, the listener is gone — the HTTP request
    //    fails to connect (or returns whatever an OS-level reset
    //    looks like through reqwest).
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
