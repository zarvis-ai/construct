use construct_client::Client;
use construct_protocol::ipc_method;
use serde_json::Value;
use std::time::Instant;
use tempfile::tempdir;
use tokio::io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;

// Robust reconnect integration test using a mock Unix-socket daemon.
// - Each mock server accepts one connection, replies to RPCs with
//   sensible minimal responses, and emits one notification.
// - Per-read timeouts prevent the test from blocking indefinitely.
// - The whole test is wrapped in a global timeout so it fails fast
//   when something goes wrong.

#[tokio::test]
async fn test_reconnect_flow() {
    let global_timeout = tokio::time::Duration::from_secs(10);
    let res = tokio::time::timeout(global_timeout, async {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("construct.sock");
        let _ = std::fs::remove_file(&sock);

        // helper to run a one-shot mock server that accepts a single
        // connection, replies to a few RPCs, and emits a notification.
        async fn run_one_shot_server<P: AsRef<std::path::Path>>(path: P, ready: tokio::sync::oneshot::Sender<()>) {
            let listener = UnixListener::bind(path.as_ref()).unwrap();
            // signal that bind succeeded so the test can connect
            let _ = ready.send(());
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = split(stream);
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            let start = Instant::now();
            let mut seen = 0usize;
            // read at most for 2 seconds total, using short per-read timeouts
            while start.elapsed() < std::time::Duration::from_secs(2) {
                line.clear();
                match tokio::time::timeout(std::time::Duration::from_millis(300), reader.read_line(&mut line)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(_n)) => {
                        // try parse; ignore malformed lines
                        if let Ok(req) = serde_json::from_str::<Value>(&line) {
                            let id = req.get("id").cloned().unwrap_or(Value::Null);
                            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                            let resp = match method {
                                ipc_method::SUBSCRIBE_EVENTS => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": null})
                                }
                                ipc_method::SESSION_LIST => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": []})
                                }
                                ipc_method::GROUP_LIST | ipc_method::PROJECT_LIST => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": []})
                                }
                                ipc_method::HARNESS_LIST => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": []})
                                }
                                ipc_method::SESSION_TRANSCRIPT => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": {"events": []}})
                                }
                                ipc_method::SESSION_PTY_REPLAY => {
                                    serde_json::json!({"jsonrpc":"2.0","id": id, "result": {"data":"","size":null}})
                                }
                                _ => serde_json::json!({"jsonrpc":"2.0","id": id, "result": null}),
                            };
                            let s = resp.to_string() + "\n";
                            let _ = w.write_all(s.as_bytes()).await;
                        }
                        seen += 1;
                        if seen >= 8 {
                            break;
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {
                        // read timeout; exit if idle long enough
                        break;
                    }
                }
            }
            // emit one notification (best-effort)
            let notif = serde_json::json!({"jsonrpc":"2.0","method":"session/event","params": {"session_id":"s1","event":{"type":"status","state":"running"}}});
            let _ = w.write_all((notif.to_string() + "\n").as_bytes()).await;
            // short pause then return
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Spawn first server and exercise client subscribe
        let path1 = sock.clone();
        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let srv1 = tokio::spawn(async move { run_one_shot_server(path1, tx1).await });
        // wait for bind signal from server
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx1).await.expect("server bind timeout");
        // Client connect
        let client = Client::connect(&sock).await.unwrap();
        let mut notif_rx = client.take_notifications().await.expect("take notifications");
        client.subscribe(None).await.unwrap();
        // wait for notification
        let got = tokio::time::timeout(std::time::Duration::from_secs(2), notif_rx.recv()).await;
        assert!(matches!(got, Ok(Some(_))));
        // ensure server finishes
        let _ = srv1.await;

        // Remove socket, small pause
        let _ = std::fs::remove_file(&sock);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Spawn second server
        let path2 = sock.clone();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        let srv2 = tokio::spawn(async move { run_one_shot_server(path2, tx2).await });
        // wait for bind signal
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), rx2).await.expect("server2 bind timeout");

        // Second client connect+subscribe
        let client2 = Client::connect(&sock).await.unwrap();
        let mut notif_rx2 = client2.take_notifications().await.expect("take notifications 2");
        client2.subscribe(None).await.unwrap();
        let got2 = tokio::time::timeout(std::time::Duration::from_secs(2), notif_rx2.recv()).await;
        assert!(matches!(got2, Ok(Some(_))));
        let _ = srv2.await;

        Ok::<(), anyhow::Error>(())
    })
    .await;

    match res {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => panic!("test inner error: {e}"),
        Err(_) => panic!("test timed out"),
    }
}
