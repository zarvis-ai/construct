use std::collections::HashSet;
use std::path::{Path, PathBuf};

use construct_client::Client;
use construct_protocol::{
    jsonrpc::{self, ErrorObject, Response},
    transport, EventNotificationPayload, MessageRole, SessionEvent, SessionKind, TimestampedEvent,
};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{self, BufReader, BufWriter};
use tokio::sync::mpsc::{self, UnboundedSender};

const ACP_PROTOCOL_VERSION: &str = "1.0";

#[derive(Debug, Default)]
struct SessionState {
    initialized: bool,
    default_harness: String,
    default_model: Option<String>,
    default_cwd: String,
    sessions: HashSet<String>,
    load_supported: bool,
    resume_supported: bool,
    close_supported: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NewSessionRequest {
    harness: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    prompt: Option<Value>,
    title: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    worktree: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdRequest {
    session_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptRequest {
    session_id: String,
    prompt: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResumeRequest {
    session_id: String,
}

pub async fn run(
    socket: PathBuf,
    default_harness: Option<String>,
    default_model: Option<String>,
    default_cwd: PathBuf,
) -> Result<()> {
    let client = Client::connect(&socket)
        .await
        .with_context(|| format!("connect to daemon at {}", socket.display()))?;

    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(writer_task(out_rx));

    client
        .subscribe(None)
        .await
        .context("subscribe to construct session events")?;

    let mut state = SessionState {
        default_harness: default_harness.unwrap_or_else(|| "shell".to_string()),
        default_model,
        default_cwd: default_cwd.to_string_lossy().to_string(),
        load_supported: true,
        resume_supported: true,
        close_supported: true,
        sessions: HashSet::new(),
        initialized: false,
    };

    if let Some(mut daemon_notifs) = client.take_notifications().await {
        let forward_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Some(notif) = daemon_notifs.recv().await {
                if notif.method != "session/event" {
                    continue;
                }

                let payload = match notif.params {
                    Some(params) => {
                        match serde_json::from_value::<EventNotificationPayload>(params) {
                            Ok(p) => p,
                            Err(_) => continue,
                        }
                    }
                    None => continue,
                };

                if let Some(update) = session_update_from_event(&payload.event, payload.seq) {
                    let _ = forward_tx.send(json!({
                        "jsonrpc": "2.0",
                        "method": "session/update",
                        "params": {
                            "sessionId": payload.session_id,
                            "update": update,
                        },
                    }));
                }
            }
        });
    }

    let mut stdin = BufReader::new(io::stdin());
    loop {
        let raw = match transport::read_message(&mut stdin).await {
            Ok(Some(v)) => v,
            Ok(None) => return Ok(()),
            Err(err) => {
                return Err(err);
            }
        };

        match jsonrpc::classify(&raw) {
            Some(jsonrpc::MessageKind::Request) => {
                let method = raw.get("method").and_then(Value::as_str).unwrap_or("");
                let id = raw.get("id").cloned();
                let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
                if let Err(err) =
                    handle_request(method, id.clone(), params, &client, &out_tx, &mut state).await
                {
                    if let Some(req_id) = id {
                        let fallback_id = req_id.clone();
                        let _ = out_tx.send(
                            serde_json::to_value(Response::err(req_id, ErrorObject::internal(err.to_string())))
                                .unwrap_or_else(|_| json!({"jsonrpc":"2.0","id":fallback_id,"error":{"code":-32603,"message":"internal error"}})),
                        );
                    }
                }
            }
            Some(jsonrpc::MessageKind::Notification) => {
                let method = raw.get("method").and_then(Value::as_str).unwrap_or("");
                let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
                let _ = handle_notification(method, params, &client, &mut state).await;
            }
            Some(jsonrpc::MessageKind::Response) | None => {}
        }
    }
}

async fn handle_request(
    method: &str,
    id: Option<Value>,
    params: Value,
    client: &Client,
    out_tx: &UnboundedSender<Value>,
    state: &mut SessionState,
) -> Result<()> {
    match method {
        "initialize" => {
            state.initialized = true;
            state.load_supported = true;
            state.resume_supported = true;
            state.close_supported = true;

            maybe_respond(
                id,
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "agentCapabilities": {
                        "loadSession": true,
                        "promptCapabilities": {
                            "image": false,
                            "audio": false,
                            "embeddedContext": false,
                        },
                        "mcpCapabilities": {
                            "http": false,
                            "sse": false,
                        },
                        "sessionCapabilities": {
                            "resume": true,
                            "close": true,
                        },
                    },
                    "agentInfo": {
                        "name": "construct",
                        "title": "construct",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "authMethods": [],
                }),
                out_tx,
            )
            .await
        }
        "session/new" => {
            ensure_initialized(state)?;
            let req: NewSessionRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/new params: {e}"))?;
            let cwd = resolve_cwd(req.cwd.as_deref(), &state.default_cwd);
            let harness = req.harness.unwrap_or_else(|| state.default_harness.clone());
            let model = req.model.or_else(|| state.default_model.clone());
            let prompt = extract_prompt_text(req.prompt.as_ref());

            let sid = client
                .create(construct_protocol::CreateSessionParams {
                    harness,
                    cwd,
                    prompt,
                    model,
                    title: req.title,
                    mode: req.mode,
                    pty_size: None,
                    worktree: req.worktree,
                    env: Default::default(),
                    args: Vec::new(),
                    kind: SessionKind::User,
                    parent_session_id: None,
                    group_id: None,
                    position_after_session_id: None,
                    forked_from: None,
                })
                .await
                .context("create construct session")?;
            state.sessions.insert(sid.clone());

            maybe_respond(id, json!({ "sessionId": sid }), out_tx).await
        }
        "session/load" => {
            ensure_initialized(state)?;
            if !state.load_supported {
                maybe_respond(id, json!({ "error": "session/load not supported" }), out_tx).await?;
                return Ok(());
            }

            let req: ResumeRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/load params: {e}"))?;
            let detail = client.get(&req.session_id).await?;
            state.sessions.insert(req.session_id.clone());

            forward_transcript(out_tx, &req.session_id, detail.events).await?;
            maybe_respond(id, json!(null), out_tx).await
        }
        "session/resume" => {
            ensure_initialized(state)?;
            if !state.resume_supported {
                maybe_respond(id, json!({}), out_tx).await?;
                return Ok(());
            }

            let req: ResumeRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/resume params: {e}"))?;
            let _ = client.get(&req.session_id).await?;
            state.sessions.insert(req.session_id);
            maybe_respond(id, json!({}), out_tx).await
        }
        "session/prompt" => {
            ensure_initialized(state)?;
            let req: PromptRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/prompt params: {e}"))?;
            if let Some(text) = extract_prompt_text(req.prompt.as_ref()) {
                if !text.trim().is_empty() {
                    client
                        .send_input(&req.session_id, text)
                        .await
                        .with_context(|| format!("send prompt to {}", req.session_id))?;
                }
            }
            maybe_respond(id, json!({ "stopReason": "end_turn" }), out_tx).await
        }
        "session/cancel" => {
            let req: SessionIdRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/cancel params: {e}"))?;
            let _ = client.stop(&req.session_id).await;
            maybe_respond(id, json!({}), out_tx).await
        }
        "session/close" => {
            ensure_initialized(state)?;
            if !state.close_supported {
                maybe_respond(id, json!({}), out_tx).await?;
                return Ok(());
            }
            let req: SessionIdRequest = serde_json::from_value(params)
                .map_err(|e| anyhow!("invalid session/close params: {e}"))?;
            client.delete(&req.session_id).await?;
            state.sessions.remove(&req.session_id);
            maybe_respond(id, json!({}), out_tx).await
        }
        _ => {
            if let Some(req_id) = id {
                out_tx.send(serde_json::to_value(Response::err(
                    req_id,
                    ErrorObject::method_not_found(method),
                ))?)?;
            }
            Ok(())
        }
    }
}

async fn handle_notification(
    method: &str,
    params: Value,
    client: &Client,
    _state: &mut SessionState,
) -> Result<()> {
    if method == "session/cancel" {
        let req: SessionIdRequest = serde_json::from_value(params)?;
        let _ = client.stop(&req.session_id).await;
    }
    Ok(())
}

fn ensure_initialized(state: &SessionState) -> Result<()> {
    if !state.initialized {
        return Err(anyhow!("not initialized"));
    }
    Ok(())
}

async fn maybe_respond(
    id: Option<Value>,
    result: Value,
    out_tx: &UnboundedSender<Value>,
) -> Result<()> {
    if let Some(id) = id {
        out_tx.send(
            serde_json::to_value(Response::ok(id, result)).context("serialize ACP response")?,
        )?;
    }
    Ok(())
}

fn resolve_cwd(requested: Option<&str>, fallback: &str) -> String {
    let requested = requested.filter(|s| !s.is_empty()).unwrap_or(fallback);
    let requested_path = Path::new(requested);
    let path = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        Path::new(fallback).join(requested_path)
    };
    std::fs::canonicalize(&path)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn extract_prompt_text(prompt: Option<&Value>) -> Option<String> {
    match prompt {
        None => None,
        Some(v) => prompt_to_text(v),
    }
}

fn prompt_to_text(value: &Value) -> Option<String> {
    match value {
        Value::String(v) => {
            let value = v.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        Value::Array(items) => {
            let lines = items
                .iter()
                .filter_map(prompt_block_to_text)
                .map(|s| s.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        _ => prompt_block_to_text(value),
    }
}

fn prompt_block_to_text(block: &Value) -> Option<String> {
    match block {
        Value::String(v) => {
            let value = v.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        }
        Value::Object(map) => {
            let kind = map.get("type").and_then(Value::as_str).unwrap_or("text");
            match kind {
                "text" => map.get("text").and_then(Value::as_str).map(str::to_string),
                "resource" | "resourceLink" | "resource_link" => map
                    .get("uri")
                    .and_then(Value::as_str)
                    .map(|uri| format!("[resource] {uri}")),
                "audio" => Some("[audio content]".to_string()),
                _ => map.get("text").and_then(Value::as_str).map(str::to_string),
            }
        }
        _ => None,
    }
}

fn session_update_from_event(event: &SessionEvent, seq: u64) -> Option<Value> {
    let message_id = format!("msg_{seq}");

    match event {
        SessionEvent::Message { role, text } => match role {
            MessageRole::User => Some(build_chunk_update("user_message_chunk", &message_id, text)),
            _ => Some(build_chunk_update("agent_message_chunk", &message_id, text)),
        },
        SessionEvent::Reasoning { text } => {
            Some(build_chunk_update("thought_chunk", &message_id, text))
        }
        SessionEvent::ToolUse {
            tool,
            args,
            call_id,
        } => {
            let output = serde_json::to_string(args).ok();
            // Prefer the explicit `call_id` as the ACP toolCallId so a
            // ToolUse and its ToolResult correlate; legacy events without
            // one fall back to a per-event id.
            let tool_call_id = call_id.clone().unwrap_or_else(|| format!("tool_{seq}"));
            Some(build_tool_call_update(
                "tool_call",
                &tool_call_id,
                tool,
                "in_progress",
                output,
            ))
        }
        SessionEvent::ToolResult {
            tool,
            ok,
            output,
            call_id,
        } => {
            let tool_call_id = call_id.clone().unwrap_or_else(|| format!("tool_{seq}"));
            Some(build_tool_call_update(
                "tool_call_update",
                &tool_call_id,
                tool,
                if *ok { "completed" } else { "failed" },
                Some(output.clone()),
            ))
        }
        SessionEvent::TaskStart { call_id, tool, .. } => Some(build_tool_call_update(
            "tool_call_update",
            call_id,
            tool,
            "in_progress",
            None,
        )),
        SessionEvent::TaskBackgrounded { call_id } => Some(build_tool_call_update(
            "tool_call_update",
            call_id,
            "",
            "in_progress",
            None,
        )),
        SessionEvent::TaskEnd {
            call_id,
            ok,
            output_preview,
        } => Some(build_tool_call_update(
            "tool_call_update",
            call_id,
            "",
            if *ok { "completed" } else { "failed" },
            Some(output_preview.clone()),
        )),
        SessionEvent::Cost {
            usd,
            tokens_in,
            tokens_out,
            ..
        } => Some(json!({
            "sessionUpdate": "usage_update",
            "used": tokens_in + tokens_out,
            "size": tokens_in + tokens_out,
            "cost": {
                "amount": *usd,
                "currency": "USD",
            },
        })),
        SessionEvent::Error { message } => Some(build_chunk_update(
            "agent_message_chunk",
            &message_id,
            &format!("error: {message}"),
        )),
        SessionEvent::AwaitingInput { prompt } => {
            let text = match prompt {
                Some(p) => p,
                None => "awaiting input",
            };
            Some(build_chunk_update("thought_chunk", &message_id, text))
        }
        SessionEvent::Pty { data } => {
            use base64::Engine;
            let text = base64::engine::general_purpose::STANDARD
                .decode(data)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_default();
            if text.is_empty() {
                None
            } else {
                Some(build_chunk_update(
                    "agent_message_chunk",
                    &message_id,
                    &text,
                ))
            }
        }
        _ => None,
    }
}

async fn forward_transcript(
    out_tx: &UnboundedSender<Value>,
    session_id: &str,
    events: Vec<TimestampedEvent>,
) -> Result<()> {
    for event in events {
        if let Some(update) = session_update_from_event(&event.event, event.seq) {
            out_tx.send(json!({
                "jsonrpc": "2.0",
                "method": "session/update",
                "params": {
                    "sessionId": session_id,
                    "update": update,
                },
            }))?;
        }
    }
    Ok(())
}

fn build_chunk_update(session_update: &str, message_id: &str, text: &str) -> Value {
    json!({
        "sessionUpdate": session_update,
        "messageId": message_id,
        "content": {
            "type": "text",
            "text": text,
        },
    })
}

fn build_tool_call_update(
    session_update: &str,
    call_id: &str,
    title: &str,
    status: &str,
    output: Option<String>,
) -> Value {
    let mut update = json!({
        "sessionUpdate": session_update,
        "toolCallId": call_id,
        "title": title,
        "status": status,
        "kind": "other",
    });

    if let Some(output) = output {
        if !output.trim().is_empty() {
            update["content"] = json!({
                "type": "content",
                "content": {
                    "type": "text",
                    "text": output,
                },
            });
        }
    }

    update
}

async fn writer_task(mut out_rx: mpsc::UnboundedReceiver<Value>) {
    let mut stdout = BufWriter::new(io::stdout());
    while let Some(msg) = out_rx.recv().await {
        let _ = transport::write_message(&mut stdout, &msg).await;
    }
}
