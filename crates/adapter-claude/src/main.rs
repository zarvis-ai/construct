//! Claude Code adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns
//!   `claude` (no `-p`) under a PTY so the right pane is the real Claude TUI.
//!   You drive it with the keyboard exactly like `claude` standalone:
//!   `/resume`, slash commands, etc. all work.
//!
//! - **headless (opt-in)** — multi-turn structured mode using
//!   `claude -p --input-format stream-json --output-format stream-json --verbose`
//!   plus `--resume <session_id>` for follow-up turns. Emits structured
//!   `Message` / `ToolUse` / `Cost` events.
//!
//! Pick mode via `--mode interactive|headless` on `agent new`, or via
//! `AGENTD_CLAUDE_MODE=interactive|headless`. Default is interactive when the
//! client supplies a PTY size (the TUI always does); otherwise headless.
//!
//! Honors `AGENTD_CLAUDE_CMD` for a full command prefix, falling back to
//! `AGENTD_CLAUDE_BIN` for a binary path.

use agentd_protocol::adapter::pty::{PtySpec, run_session as run_pty};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter, run};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "claude".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_cost: true,
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
    if let Ok(m) = std::env::var("AGENTD_CLAUDE_MODE") {
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
    let command = agentd_protocol::adapter::resolve_command_override(
        "AGENTD_CLAUDE_CMD",
        "AGENTD_CLAUDE_BIN",
        "claude",
    );
    let mut args = command.args.clone();
    args.extend(params.args.clone());
    if let Some(m) = params.model.as_ref() {
        args.push("--model".into());
        args.push(m.clone());
    }
    // Auto-inject the agentd MCP server so the agent inside this session
    // can drive the daemon (list other sessions, send input, spawn helpers,
    // etc.). Opt out with AGENTD_INJECT_MCP=0.
    if let Some(cfg) = agentd_protocol::adapter::maybe_inject_mcp_config(&ctx.session_id) {
        args.push("--mcp-config".into());
        args.push(cfg.to_string_lossy().to_string());
    }
    // Translate the daemon-defined auto-approval policy into Claude's native
    // `--allowed-tools` patterns. Single policy in agentd; each adapter
    // applies it in its harness's native mechanism.
    args.extend(
        agentd_protocol::adapter::policy::AutoApprovePolicy::from_env().claude_allowed_tools_args(),
    );
    // Resume support: stash our own UUID under
    // $AGENTD_SESSION_DATA_DIR/claude_session_id.txt at first spawn (passed
    // to claude as --session-id), then pass it back as --resume when the
    // daemon respawns us after a restart. claude's own session-persistence
    // makes the conversation pick up where it left off.
    let resuming = std::env::var("AGENTD_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("AGENTD_SESSION_DATA_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("claude_session_id.txt"));
    let claude_session_id = match (resuming, sid_file.as_ref()) {
        (true, Some(p)) if p.exists() => std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let watch_session_id = if let Some(sid) = &claude_session_id {
        args.push("--resume".into());
        args.push(sid.clone());
        Some(sid.clone())
    } else if let Some(p) = &sid_file {
        // First spawn (or no prior id): mint our own and pass --session-id.
        let new_id = uuid::Uuid::new_v4().to_string();
        let _ = std::fs::write(p, &new_id);
        args.push("--session-id".into());
        args.push(new_id.clone());
        Some(new_id)
    } else {
        None
    };
    // Skip the initial prompt on resume — it's already in the claude
    // conversation we're rejoining.
    if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.clone());
        }
    }
    // Surface the session id to the child's env so agents that aren't using
    // MCP (or the user, via `echo $AGENTD_SESSION_ID`) can still tell.
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("AGENTD_SESSION_ID".into(), ctx.session_id.clone()));
    if let Some(session_id) = watch_session_id {
        spawn_interactive_transcript_watcher(
            session_id,
            PathBuf::from(&params.cwd),
            ctx.emit.clone(),
            resuming,
        );
    }
    let label = command.argv_preview();
    let bin = command.bin;
    let spec = PtySpec {
        bin,
        args,
        cwd: std::path::PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
    };
    let _ = run_pty(spec, ctx).await;
}

fn spawn_interactive_transcript_watcher(
    session_id: String,
    cwd: PathBuf,
    emit: EventEmitter,
    skip_existing: bool,
) {
    let Some(path) = claude_transcript_path(&cwd, &session_id) else {
        emit.log("claude: no CLAUDE_HOME or HOME — cannot watch native transcript");
        return;
    };
    tokio::spawn(async move {
        let mut next_line = if skip_existing {
            count_jsonl_lines(&path)
        } else {
            0
        };
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            emit_new_claude_transcript_lines(&path, &mut next_line, &emit);
        }
    });
}

fn claude_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("AGENTD_CLAUDE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("CLAUDE_HOME").ok().filter(|s| !s.is_empty()))
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.claude")))?;
    Some(
        PathBuf::from(home)
            .join("projects")
            .join(claude_project_slug(cwd))
            .join(format!("{session_id}.jsonl")),
    )
}

fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn count_jsonl_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn emit_new_claude_transcript_lines(path: &Path, next_line: &mut usize, emit: &EventEmitter) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let mut seen = 0usize;
    for (idx, line) in text.lines().enumerate() {
        seen = idx + 1;
        if idx < *next_line {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => emit_event_from_json(emit, v),
            Err(e) => emit.log(format!(
                "claude transcript: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = agentd_protocol::adapter::resolve_command_override(
        "AGENTD_CLAUDE_CMD",
        "AGENTD_CLAUDE_BIN",
        "claude",
    );
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut session_id: Option<String> = None;
    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = params.prompt.clone() {
        if !p.trim().is_empty() {
            pending.push_back(p);
        }
    }

    let exit_code = loop {
        // Pick next user message, or wait for one.
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

        // Build the per-turn child command args.
        let mut child_args: Vec<String> = command_override.args.clone();
        child_args.push("-p".into());
        child_args.push("--input-format".into());
        child_args.push("stream-json".into());
        child_args.push("--output-format".into());
        child_args.push("stream-json".into());
        child_args.push("--verbose".into());
        if let Some(cfg) = agentd_protocol::adapter::maybe_inject_mcp_config(&agentd_session_id) {
            child_args.push("--mcp-config".into());
            child_args.push(cfg.to_string_lossy().to_string());
        }
        child_args.extend(
            agentd_protocol::adapter::policy::AutoApprovePolicy::from_env()
                .claude_allowed_tools_args(),
        );
        if let Some(sid) = &session_id {
            child_args.push("--resume".into());
            child_args.push(sid.clone());
        }
        if let Some(m) = &model {
            child_args.push("--model".into());
            child_args.push(m.clone());
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }
        let mut command = Command::new(&command_override.bin);
        for a in &child_args {
            command.arg(a);
        }
        command
            .current_dir(&cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in &env {
            command.env(k, v);
        }
        command.env("AGENTD_SESSION_ID", &agentd_session_id);

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: agentd_protocol::adapter::missing_bin_hint(
                        &command_override.argv_preview(),
                        &e,
                    ),
                });
                break 127;
            }
        };

        let child_stdin = child.stdin.take().expect("piped");
        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        // Write the user message, then close stdin so claude knows we're done.
        let writer_task = spawn_writer(child_stdin, user_text.clone());
        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let parser_task = spawn_parser(child_stdout, emit.clone(), captured_sid.clone());

        // Drive the child: queue mid-turn inputs, honor stop/interrupt.
        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = writer_task.await;
        let _ = parser_task.await;
        let _ = stderr_task.await;
        // Make sure the child is fully reaped.
        let _ = child.wait().await;

        if session_id.is_none() {
            session_id = captured_sid.lock().unwrap().clone();
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
                        // daemon channel closed
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
                        // headless claude doesn't gate tools; ignore.
                    }
                }
            }
            _ = child.wait() => {
                return TurnOutcome::Completed;
            }
        }
    }
}

fn spawn_writer(
    mut stdin: tokio::process::ChildStdin,
    user_text: String,
) -> tokio::task::JoinHandle<()> {
    let msg = serde_json::json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": user_text }]
        }
    });
    tokio::spawn(async move {
        let line = match serde_json::to_string(&msg) {
            Ok(s) => s,
            Err(_) => return,
        };
        let _ = stdin.write_all(line.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
        let _ = stdin.shutdown().await;
    })
}

fn spawn_parser<R>(
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
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                        let mut g = captured_sid.lock().unwrap();
                        if g.is_none() {
                            *g = Some(sid.to_string());
                        }
                    }
                    emit_event_from_json(&emit, v);
                }
                Err(_) => emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: line,
                }),
            }
        }
    })
}

fn spawn_stderr_log<R>(reader: R, emit: EventEmitter) -> tokio::task::JoinHandle<()>
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

fn emit_event_from_json(emit: &EventEmitter, v: Value) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "assistant" => {
            let text = extract_message_text(v.get("message"));
            if !text.is_empty() {
                emit.emit(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text,
                });
            }
            forward_tool_uses(emit, v.get("message"));
        }
        "user" => {
            // The CLI echoes tool_result blocks here. The actual user text is
            // already in the transcript (daemon emits it when sending input).
            forward_tool_results(emit, v.get("message"));
        }
        "result" => {
            let usd = v
                .get("total_cost_usd")
                .and_then(|n| n.as_f64())
                .unwrap_or(0.0);
            let tin = v
                .get("usage")
                .and_then(|u| u.get("input_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            let tout = v
                .get("usage")
                .and_then(|u| u.get("output_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            if usd > 0.0 || tin > 0 || tout > 0 {
                emit.emit(SessionEvent::Cost {
                    usd,
                    tokens_in: tin,
                    tokens_out: tout,
                });
            }
            // The `result` text duplicates the assistant's final message; skip it.
        }
        "system" => {
            emit.log(format!(
                "system: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
        "rate_limit_event" => {
            emit.log(format!(
                "rate_limit: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
        other => {
            emit.log(format!(
                "claude event[{other}]: {}",
                serde_json::to_string(&v).unwrap_or_default()
            ));
        }
    }
}

fn extract_message_text(msg: Option<&Value>) -> String {
    let Some(m) = msg else {
        return String::new();
    };
    if let Some(s) = m.get("content").and_then(|c| c.as_str()) {
        return s.to_string();
    }
    if let Some(arr) = m.get("content").and_then(|c| c.as_array()) {
        let mut out = String::new();
        for block in arr {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|s| s.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        return out;
    }
    String::new()
}

fn forward_tool_uses(emit: &EventEmitter, msg: Option<&Value>) {
    let Some(arr) = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            emit.emit(SessionEvent::ToolUse {
                tool: name,
                args: input,
            });
        }
    }
}

fn forward_tool_results(emit: &EventEmitter, msg: Option<&Value>) {
    let Some(arr) = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return;
    };
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            let tool = block
                .get("tool_use_id")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !block
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let output = match block.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                None => String::new(),
            };
            emit.emit(SessionEvent::ToolResult { tool, ok, output });
        }
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "..."
    }
}
