//! OpenAI Codex CLI adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns `codex`
//!   under a PTY, giving the user the real Codex TUI experience.
//!
//! - **headless (opt-in)** — multi-turn structured mode that spawns
//!   `codex exec <prompt>` per turn. Best-effort: if your codex build
//!   supports session resumption, set `AGENTD_CODEX_RESUME_FLAG` to the flag
//!   name (e.g. `--session-id`) and the adapter will pass any captured
//!   `session_id` back in for subsequent turns.
//!
//! Pick mode via `--mode interactive|headless` on `agent new`, or via
//! `AGENTD_CODEX_MODE=interactive|headless`. Honors `AGENTD_CODEX_BIN` for
//! the binary path.

use agentd_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use agentd_protocol::adapter::{run, AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "codex".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    run(metadata, |params, ctx| async move {
        match resolve_mode(&params) {
            Mode::Interactive => run_interactive(params, ctx).await,
            Mode::Headless => run_session(params, ctx).await,
        }
    })
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Interactive,
    Headless,
}

fn resolve_mode(params: &SessionStartParams) -> Mode {
    if let Ok(m) = std::env::var("AGENTD_CODEX_MODE") {
        match m.as_str() {
            "interactive" => return Mode::Interactive,
            "headless" => return Mode::Headless,
            _ => {}
        }
    }
    match params.mode.as_deref() {
        Some("interactive") => Mode::Interactive,
        Some("headless") => Mode::Headless,
        _ if params.pty_size.is_some() => Mode::Interactive,
        _ => Mode::Headless,
    }
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let bin = std::env::var("AGENTD_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let mut args = params.args.clone();
    // Resume support: on daemon-restart respawn we use codex's
    // `resume <SESSION_ID>` subcommand instead of starting fresh. We
    // capture the id from the first turn's transcript (best-effort —
    // codex prints "session id: <uuid>" in the banner). If we never
    // captured one, fall back to `resume --last`. Honor the user's
    // explicit override via `AGENTD_CODEX_RESUME_ID`.
    let resuming = std::env::var("AGENTD_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("AGENTD_SESSION_DATA_DIR").ok().map(|d| {
        std::path::PathBuf::from(d).join("codex_session_id.txt")
    });
    if resuming {
        args.insert(0, "resume".into());
        let explicit = std::env::var("AGENTD_CODEX_RESUME_ID").ok();
        let from_file = sid_file.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
        if let Some(id) = explicit.or(from_file) {
            args.insert(1, id);
        } else {
            args.insert(1, "--last".into());
        }
    }
    if let Some(m) = params.model.as_ref() {
        args.push("-m".into());
        args.push(m.clone());
    }
    // Auto-inject agentd MCP server via codex's `-c` override (codex has no
    // `--mcp-config` flag — MCP servers live in `[mcp_servers.<name>]`).
    // Opt out with AGENTD_INJECT_MCP=0.
    for a in agentd_protocol::adapter::maybe_inject_codex_mcp_args(&ctx.session_id) {
        args.push(a);
    }
    // Skip the initial prompt on resume — codex's resume already has it.
    if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.clone());
        }
    }
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("AGENTD_SESSION_ID".into(), ctx.session_id.clone()));
    let label = bin.clone();
    let spec = PtySpec {
        bin,
        args,
        cwd: std::path::PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize { cols: 100, rows: 30 }),
        status_detail: Some(format!("{label} (interactive)")),
    };
    let _ = run_pty(spec, ctx).await;
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: _,
        emit,
        mut inbox,
    } = ctx;

    let bin = std::env::var("AGENTD_CODEX_BIN").unwrap_or_else(|_| "codex".into());
    let resume_flag = std::env::var("AGENTD_CODEX_RESUME_FLAG").ok();
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut codex_session_id: Option<String> = None;
    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = params.prompt.clone() {
        if !p.trim().is_empty() {
            pending.push_back(p);
        }
    }

    let exit_code = loop {
        let user_text = match pending.pop_front() {
            Some(t) => t,
            None => {
                emit.emit(SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                });
                match inbox.recv().await {
                    None => break 0,
                    Some(AdapterInboxMsg::Input(t)) => t,
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::Stop) => break 0,
                    Some(AdapterInboxMsg::PtyInput(_))
                    | Some(AdapterInboxMsg::PtyResize { .. })
                    | Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetAutoMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => continue,
                }
            }
        };
        if user_text.trim().is_empty() {
            continue;
        }

        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });

        let mut child_args: Vec<String> = Vec::new();
        child_args.push("exec".into());
        if let (Some(flag), Some(sid)) = (resume_flag.as_ref(), codex_session_id.as_ref()) {
            child_args.push(flag.clone());
            child_args.push(sid.clone());
        }
        if let Some(m) = &model {
            child_args.push("-m".into());
            child_args.push(m.clone());
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }
        child_args.push(user_text.clone());
        let mut command = Command::new(&bin);
        for a in &child_args {
            command.arg(a);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &env {
            command.env(k, v);
        }

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: agentd_protocol::adapter::missing_bin_hint(&bin, &e),
                });
                break 127;
            }
        };

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let stdout_task = spawn_stdout(child_stdout, emit.clone(), captured_sid.clone());
        let stderr_task = spawn_stderr(child_stderr, emit.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        if codex_session_id.is_none() {
            codex_session_id = captured_sid.lock().unwrap().clone();
        }

        match outcome {
            TurnOutcome::Completed => continue,
            TurnOutcome::Interrupted => {
                emit.log("turn interrupted; awaiting next input");
                continue;
            }
            TurnOutcome::Stopped => break 0,
        }
    };

    emit.emit(SessionEvent::Done { exit_code });
}

#[derive(Debug)]
enum TurnOutcome {
    Completed,
    Interrupted,
    Stopped,
}

async fn drive_turn(
    child: &mut tokio::process::Child,
    inbox: &mut mpsc::Receiver<AdapterInboxMsg>,
    emit: &EventEmitter,
    pending: &mut VecDeque<String>,
) -> TurnOutcome {
    loop {
        tokio::select! {
            biased;
            msg = inbox.recv() => {
                match msg {
                    None => {
                        let _ = child.start_kill();
                        return TurnOutcome::Stopped;
                    }
                    Some(AdapterInboxMsg::Stop) => {
                        let _ = child.start_kill();
                        return TurnOutcome::Stopped;
                    }
                    Some(AdapterInboxMsg::Interrupt) => {
                        let _ = child.start_kill();
                        return TurnOutcome::Interrupted;
                    }
                    Some(AdapterInboxMsg::Input(t)) => {
                        emit.log(format!("queued input for next turn: {}", short(&t, 60)));
                        pending.push_back(t);
                    }
                    Some(AdapterInboxMsg::PtyInput(_))
                    | Some(AdapterInboxMsg::PtyResize { .. })
                    | Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetAutoMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => {
                        // headless codex doesn't gate tools; ignore.
                    }
                }
            }
            _ = child.wait() => {
                return TurnOutcome::Completed;
            }
        }
    }
}

fn spawn_stdout<R>(
    reader: R,
    emit: EventEmitter,
    captured_sid: Arc<StdMutex<Option<String>>>,
) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            // Best-effort JSON parse; if not JSON, emit as plain assistant text.
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    let mut g = captured_sid.lock().unwrap();
                    if g.is_none() {
                        *g = Some(sid.to_string());
                    }
                }
                if !try_emit_structured(&emit, &v) {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: line,
                    });
                }
            } else {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: line,
                });
            }
        }
    })
}

fn spawn_stderr<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            emit.log(format!("stderr: {line}"));
        }
    })
}

/// Try to pull structured fields out of a codex JSON event. Returns `true` if
/// the value was recognized; otherwise the caller falls back to emitting raw.
fn try_emit_structured(emit: &EventEmitter, v: &Value) -> bool {
    let ty = match v.get("type").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    match ty {
        "message" | "assistant" => {
            if let Some(text) = v
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| s.to_string())
                .or_else(|| extract_text_from_blocks(v.get("content")))
            {
                if !text.is_empty() {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text,
                    });
                    return true;
                }
            }
            false
        }
        "tool_use" => {
            let name = v
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let args = v.get("input").cloned().unwrap_or(Value::Null);
            emit.emit(SessionEvent::ToolUse { tool: name, args });
            true
        }
        "tool_result" => {
            let tool = v
                .get("tool_use_id")
                .or_else(|| v.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !v
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let output = match v.get("output").or_else(|| v.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            emit.emit(SessionEvent::ToolResult { tool, ok, output });
            true
        }
        _ => false,
    }
}

fn extract_text_from_blocks(v: Option<&Value>) -> Option<String> {
    let arr = v?.as_array()?;
    let mut out = String::new();
    for block in arr {
        if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}
