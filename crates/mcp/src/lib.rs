//! `construct-mcp` — MCP stdio server that lets an agent (running inside an
//! construct session) control the construct daemon: list sessions, read their
//! output, send input, spawn new sessions, etc.
//!
//! Wire protocol is MCP 2024-11-05 (JSON-RPC 2.0 over line-delimited JSON
//! on stdin/stdout). Reuses `construct_protocol::transport` for framing and
//! `construct_protocol::jsonrpc` for envelope types.
//!
//! Environment:
//! - `CONSTRUCT_SOCKET` — override the daemon's Unix socket path
//! - `CONSTRUCT_SESSION_ID` — the calling agent's session id (returned by the
//!   `construct_whoami` tool). Set by the construct adapter when it spawns the
//!   child CLI.

use construct_client::Client;
use construct_protocol::jsonrpc::{self, error_codes, ErrorObject, MessageKind, Request, Response};
use construct_protocol::paths::Paths;
use construct_protocol::transport;
use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::BufReader;

mod tools;

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run() -> Result<()> {
    let socket = std::env::var("CONSTRUCT_SOCKET")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| Paths::discover().socket());
    let session_id = std::env::var("CONSTRUCT_SESSION_ID").ok();

    let client = match Client::connect(&socket).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "construct-mcp: failed to connect to {}: {e}",
                socket.display()
            );
            std::process::exit(1);
        }
    };

    run_inner(socket, client, session_id).await
}

async fn run_inner(
    socket: PathBuf,
    mut client: Arc<Client>,
    session_id: Option<String>,
) -> Result<()> {
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    // One MCP server process serves one agent, so what the context tool has
    // already sent that agent is process state (spec 0095).
    let context_state = tools::ContextServeState::default();

    loop {
        let raw = match transport::read_message(&mut stdin).await {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(()), // EOF
            Err(e) => {
                eprintln!("construct-mcp: invalid JSON on stdin: {e}");
                continue;
            }
        };
        match jsonrpc::classify(&raw) {
            Some(MessageKind::Request) => {
                let req: Request = match serde_json::from_value(raw) {
                    Ok(r) => r,
                    Err(_) => continue,
                };
                if client.is_disconnected() {
                    if let Ok(new_client) = Client::connect(&socket).await {
                        client = new_client;
                    }
                }
                let resp =
                    handle_request(&client, session_id.as_deref(), &context_state, req).await;
                let v = match serde_json::to_value(&resp) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if transport::write_message(&mut stdout, &v).await.is_err() {
                    return Ok(());
                }
            }
            Some(MessageKind::Notification) => {
                // MCP `notifications/initialized` etc. — nothing to do here.
            }
            _ => {}
        }
    }
}

async fn handle_request(
    client: &Arc<Client>,
    session_id: Option<&str>,
    context_state: &tools::ContextServeState,
    req: Request,
) -> Response {
    let id = req.id.clone();
    let params = req.params.clone().unwrap_or(serde_json::Value::Null);

    match req.method.as_str() {
        "initialize" => Response::ok(
            id,
            serde_json::json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "serverInfo": {
                    "name": "construct-mcp",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": { "listChanged": false }
                }
            }),
        ),
        "tools/list" => Response::ok(id, serde_json::json!({ "tools": tools::catalog() })),
        "tools/call" => match tools::call(client, session_id, context_state, params).await {
            Ok(content) => Response::ok(id, content),
            Err(e) => Response::ok(
                id,
                serde_json::json!({
                    "content": [{ "type": "text", "text": format!("error: {e}") }],
                    "isError": true,
                }),
            ),
        },
        "ping" => Response::ok(id, serde_json::Value::Object(Default::default())),
        other => Response::err(
            id,
            ErrorObject::new(
                error_codes::METHOD_NOT_FOUND,
                format!("method not found: {other}"),
            ),
        ),
    }
}
