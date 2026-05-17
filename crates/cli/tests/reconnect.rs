use tempfile::tempdir;
use tokio::net::UnixListener;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, split};
use serde_json::Value;
use agentd_client::Client;
use agentd_protocol::ipc_method;

use std::path::Path;

// A minimal mock daemon: accept one client, handle subscribe + list calls,
// then send a fake notification and keep the socket open.

#[tokio::test]
async fn test_reconnect_flow() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("agentd.sock");
    let _ = std::fs::remove_file(&sock);

    let listener = UnixListener::bind(&sock).unwrap();

    // Spawn server task
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = split(stream);
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        // read one request
        reader.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        // respond to subscribe and list calls
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        if method == ipc_method::SUBSCRIBE_EVENTS {
            let resp = serde_json::json!({"jsonrpc":"2.0","id": id, "result": null});
            let s = resp.to_string() + "\n";
            w.write_all(s.as_bytes()).await.unwrap();
            // send a notification after a short delay
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let notif = serde_json::json!({"jsonrpc":"2.0","method":"session/event","params": {"session_id":"s1","event":{"type":"status","state":"running"}}});
            let ns = notif.to_string() + "\n";
            w.write_all(ns.as_bytes()).await.unwrap();
        } else if method == ipc_method::SESSION_LIST {
            let resp = serde_json::json!({"jsonrpc":"2.0","id": id, "result": []});
            let s = resp.to_string() + "\n";
            w.write_all(s.as_bytes()).await.unwrap();
        } else {
            let resp = serde_json::json!({"jsonrpc":"2.0","id": id, "result": null});
            let s = resp.to_string() + "\n";
            w.write_all(s.as_bytes()).await.unwrap();
        }
        // keep open a bit
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    });

    // Client connect
    let client = Client::connect(&sock).await.unwrap();
    // subscribe
    client.subscribe(None).await.unwrap();

    // now drop server by awaiting the server task
    server.await.unwrap();

    // Now test reconnect by starting a new server on same socket
    let listener2 = UnixListener::bind(&sock).unwrap();
    let server2 = tokio::spawn(async move {
        let (stream, _) = listener2.accept().await.unwrap();
        let (r, mut w) = split(stream);
        let mut reader = BufReader::new(r);
        let mut line = String::new();
        // read a few requests and reply with basic results
        for _ in 0..5 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let req: Value = serde_json::from_str(&line).unwrap();
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let resp = match method {
                ipc_method::SUBSCRIBE_EVENTS => serde_json::json!({"jsonrpc":"2.0","id": id, "result": null}),
                ipc_method::SESSION_LIST => serde_json::json!({"jsonrpc":"2.0","id": id, "result": []}),
                ipc_method::GROUP_LIST => serde_json::json!({"jsonrpc":"2.0","id": id, "result": []}),
                ipc_method::HARNESS_LIST => serde_json::json!({"jsonrpc":"2.0","id": id, "result": []}),
                _ => serde_json::json!({"jsonrpc":"2.0","id": id, "result": null}),
            };
            let s = resp.to_string() + "\n";
            w.write_all(s.as_bytes()).await.unwrap();
        }
        // send a notification so client has something to receive
        let notif = serde_json::json!({"jsonrpc":"2.0","method":"session/event","params": {"session_id":"s1","event":{"type":"status","state":"running"}}});
        let ns = notif.to_string() + "\n";
        w.write_all(ns.as_bytes()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    });

    // Try reconnecting with a new Client
    let client2 = Client::connect(&sock).await.unwrap();
    client2.subscribe(None).await.unwrap();

    server2.await.unwrap();
}
