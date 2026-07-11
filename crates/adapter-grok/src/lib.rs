//! Grok CLI adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns `grok`
//!   under a PTY, giving the user the real Grok TUI experience.
//!
//! - **headless (opt-in)** — multi-turn structured mode that spawns
//!   `grok -p <prompt> --output-format streaming-json` per turn.
//!
//! Pick mode via `--mode interactive|headless` on `construct new`, or via
//! `CONSTRUCT_GROK_MODE=interactive|headless`. Honors `CONSTRUCT_GROK_CMD` for a
//! full command prefix, falling back to `CONSTRUCT_GROK_BIN` for a binary path.

use agentd_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use agentd_protocol::adapter::{run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use construct_adapter_common::{drive_turn, next_native_seq, spawn_stderr_log, TurnOutcome};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "grok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            supports_pty: true,
            ..Default::default()
        },
    };
    adapter_run(metadata, |params, ctx| async move {
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
    if let Ok(m) = std::env::var("CONSTRUCT_GROK_MODE") {
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

fn command_override() -> agentd_protocol::adapter::CommandOverride {
    agentd_protocol::adapter::resolve_command_override(
        "CONSTRUCT_GROK_CMD",
        "CONSTRUCT_GROK_BIN",
        "grok",
    )
}

fn session_data_dir() -> Option<PathBuf> {
    std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(PathBuf::from)
}

fn conv_id_file() -> Option<PathBuf> {
    Some(session_data_dir()?.join("grok_session_id.txt"))
}

fn read_conv_id() -> Option<String> {
    let p = conv_id_file()?;
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn write_conv_id(id: &str) {
    if let Some(p) = conv_id_file() {
        let _ = std::fs::write(p, id);
    }
}

fn grok_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CONSTRUCT_GROK_HOME") {
        return Some(PathBuf::from(h));
    }
    if let Ok(h) = std::env::var("GROK_HOME") {
        return Some(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".grok"))
}

fn url_encode_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    let mut encoded = String::new();
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' | '~' => {
                encoded.push(c);
            }
            '/' => {
                encoded.push_str("%2F");
            }
            _ => {
                for byte in c.to_string().bytes() {
                    encoded.push_str(&format!("%{:02X}", byte));
                }
            }
        }
    }
    encoded
}

#[cfg(test)]
fn find_session_id(cwd: &Path) -> Option<String> {
    find_session_id_excluding(cwd, &HashSet::new())
}

fn find_session_id_excluding(cwd: &Path, excluded: &HashSet<String>) -> Option<String> {
    let sessions_dir = grok_home()?.join("sessions").join(url_encode_path(cwd));
    if !sessions_dir.exists() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    if let Ok(entries) = std::fs::read_dir(sessions_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.len() == 36 && !excluded.contains(&name) {
                        if let Ok(metadata) = entry.metadata() {
                            if let Ok(modified) = metadata.modified() {
                                if best.is_none() || modified > best.as_ref().unwrap().0 {
                                    best = Some((modified, name));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    best.map(|(_, name)| name)
}

fn grok_session_dir(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(
        grok_home()?
            .join("sessions")
            .join(url_encode_path(cwd))
            .join(session_id),
    )
}

fn grok_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(grok_session_dir(cwd, session_id)?.join("chat_history.jsonl"))
}

fn grok_updates_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(grok_session_dir(cwd, session_id)?.join("updates.jsonl"))
}

fn count_jsonl_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn read_new_grok_jsonl_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = 0usize;
    let mut values = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        seen = idx + 1;
        if idx < *next_line {
            continue;
        }
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => values.push(v),
            Err(e) => emit.log(format!(
                "grok: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
    values
}

fn emit_new_grok_transcript_lines(path: &Path, next_line: &mut usize, emit: &EventEmitter) {
    for value in read_new_grok_jsonl_lines(path, next_line, emit) {
        emit_event_from_json(emit, value);
    }
}

fn emit_event_from_json(emit: &EventEmitter, v: Value) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "assistant" | "tool_result" => {
            for event in grok_events_from_json(&v) {
                emit.emit(event);
            }
        }
        "reasoning" => {
            if let Some(summary) = v.get("summary").and_then(|s| s.as_array()) {
                for item in summary {
                    if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                        emit.log(format!("grok reasoning: {}", t));
                    }
                }
            }
        }
        _ => {}
    }
}

fn grok_events_from_json(v: &Value) -> Vec<SessionEvent> {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "user" => grok_content_text(v)
            .filter(|content| {
                !content.is_empty() && !content.trim_start().starts_with("<system-reminder>")
            })
            .map(|text| {
                vec![SessionEvent::Message {
                    role: MessageRole::User,
                    text,
                }]
            })
            .unwrap_or_default(),
        "assistant" => {
            let mut out = Vec::new();
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                if !content.is_empty() {
                    out.push(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: content.to_string(),
                    });
                }
            }
            if let Some(tool_calls) = v.get("tool_calls").and_then(|tc| tc.as_array()) {
                for call in tool_calls {
                    let name = call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = call
                        .get("arguments")
                        .and_then(|a| {
                            if let Some(s) = a.as_str() {
                                serde_json::from_str::<Value>(s).ok()
                            } else {
                                Some(a.clone())
                            }
                        })
                        .unwrap_or(Value::Null);
                    let call_id = call.get("id").and_then(|i| i.as_str()).map(String::from);
                    out.push(SessionEvent::ToolUse {
                        tool: name,
                        args,
                        call_id,
                    });
                }
            }
            out
        }
        "tool_result" => {
            let call_id = v
                .get("tool_call_id")
                .and_then(|i| i.as_str())
                .map(String::from);
            let output = v
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("")
                .to_string();
            let is_failed = output.contains("User cancelled") || output.contains("failed");
            vec![SessionEvent::ToolResult {
                tool: "".to_string(),
                ok: !is_failed,
                output,
                call_id,
            }]
        }
        _ => Vec::new(),
    }
}

fn grok_content_text(v: &Value) -> Option<String> {
    let content = v.get("content")?.as_str()?;
    let Ok(items) = serde_json::from_str::<Value>(content) else {
        return Some(content.to_string());
    };
    let text = items
        .as_array()?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GrokNativeSubagentUpdate {
    Spawned {
        id: String,
        parent_id: Option<String>,
        title: Option<String>,
    },
    Finished {
        id: String,
        state: SessionState,
    },
}

fn grok_native_subagent_update(
    value: &Value,
    owner_native_id: &str,
) -> Option<GrokNativeSubagentUpdate> {
    if value.get("method").and_then(Value::as_str) != Some("_x.ai/session/update") {
        return None;
    }
    let update = value.pointer("/params/update")?;
    match update.get("sessionUpdate").and_then(Value::as_str)? {
        "subagent_spawned" => {
            let id = update
                .get("child_session_id")
                .or_else(|| update.get("subagent_id"))
                .and_then(Value::as_str)?
                .to_string();
            let parent_id = update
                .get("parent_session_id")
                .and_then(Value::as_str)
                .filter(|parent| *parent != owner_native_id)
                .map(str::to_string);
            let title = update
                .get("description")
                .and_then(Value::as_str)
                .filter(|description| !description.trim().is_empty())
                .map(str::to_string);
            Some(GrokNativeSubagentUpdate::Spawned {
                id,
                parent_id,
                title,
            })
        }
        "subagent_finished" => {
            let id = update
                .get("child_session_id")
                .or_else(|| update.get("subagent_id"))
                .and_then(Value::as_str)?
                .to_string();
            let state = match update.get("status").and_then(Value::as_str).unwrap_or("") {
                "failed" | "error" | "errored" | "cancelled" => SessionState::Errored,
                "completed" | "done" | "success" => SessionState::Done,
                _ => SessionState::Running,
            };
            Some(GrokNativeSubagentUpdate::Finished { id, state })
        }
        _ => None,
    }
}

#[derive(Debug)]
struct GrokNativeChild {
    parent_id: Option<String>,
    state: SessionState,
    next_transcript_line: usize,
}

fn apply_grok_native_update(
    update: GrokNativeSubagentUpdate,
    children: &mut HashMap<String, GrokNativeChild>,
    emit: Option<&EventEmitter>,
) {
    match update {
        GrokNativeSubagentUpdate::Spawned {
            id,
            parent_id,
            title,
        } => {
            let next_transcript_line = children
                .get(&id)
                .map(|child| child.next_transcript_line)
                .unwrap_or(0);
            children.insert(
                id.clone(),
                GrokNativeChild {
                    parent_id: parent_id.clone(),
                    state: SessionState::Running,
                    next_transcript_line,
                },
            );
            if let Some(emit) = emit {
                emit.emit(SessionEvent::NativeSubagent {
                    id,
                    parent_id,
                    title,
                    state: SessionState::Running,
                    event: None,
                    seq: None,
                });
            }
        }
        GrokNativeSubagentUpdate::Finished { id, state } => {
            let child = children
                .entry(id.clone())
                .or_insert_with(|| GrokNativeChild {
                    parent_id: None,
                    state,
                    next_transcript_line: 0,
                });
            child.state = state;
            if let Some(emit) = emit {
                emit.emit(SessionEvent::NativeSubagent {
                    id,
                    parent_id: child.parent_id.clone(),
                    title: None,
                    state,
                    event: None,
                    seq: None,
                });
            }
        }
    }
}

fn grok_allow_args() -> Vec<String> {
    let policy = agentd_protocol::adapter::policy::AutoApprovePolicy::from_env();
    let mut out = Vec::new();
    for root in policy.allow_paths() {
        let glob = format!("{}/**", root.display());
        for tool in ["Write", "Edit", "MultiEdit"] {
            out.push("--allow".into());
            out.push(format!("{tool}({glob})"));
        }
    }
    out
}

fn spawn_interactive_transcript_watcher(
    initial_id: Option<String>,
    cwd: PathBuf,
    emit: EventEmitter,
    skip_existing: bool,
) {
    if grok_home().is_none() {
        emit.log("grok: no GROK_HOME or HOME — cannot watch native transcript");
        return;
    }
    tokio::spawn(async move {
        let mut current_id = initial_id;
        let mut path: Option<PathBuf> = current_id
            .as_ref()
            .and_then(|id| grok_transcript_path(&cwd, id));
        let mut updates_path: Option<PathBuf> = current_id
            .as_ref()
            .and_then(|id| grok_updates_path(&cwd, id));
        // Only the initial resume attach skips prior history; mid-session
        // rebinds (after /clear) start at the top of the new transcript.
        let mut next_line = if skip_existing {
            path.as_ref().map(|p| count_jsonl_lines(p)).unwrap_or(0)
        } else {
            0
        };
        let mut next_update_line = if skip_existing {
            updates_path.as_deref().map(count_jsonl_lines).unwrap_or(0)
        } else {
            0
        };
        let mut children = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        if skip_existing {
            if let (Some(root_id), Some(updates_path)) =
                (current_id.as_deref(), updates_path.as_deref())
            {
                let mut replay_line = 0;
                for value in read_new_grok_jsonl_lines(updates_path, &mut replay_line, &emit) {
                    if let Some(update) = grok_native_subagent_update(&value, root_id) {
                        apply_grok_native_update(update, &mut children, None);
                    }
                }
            }
        }
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Some(path) = path.as_deref().filter(|path| path.exists()) {
                emit_new_grok_transcript_lines(path, &mut next_line, &emit);
            }
            if let (Some(root_id), Some(updates_path)) =
                (current_id.as_deref(), updates_path.as_deref())
            {
                for value in read_new_grok_jsonl_lines(updates_path, &mut next_update_line, &emit) {
                    if let Some(update) = grok_native_subagent_update(&value, root_id) {
                        apply_grok_native_update(update, &mut children, Some(&emit));
                    }
                }
            }
            for (id, child) in &mut children {
                let Some(child_path) = grok_transcript_path(&cwd, id) else {
                    continue;
                };
                for value in
                    read_new_grok_jsonl_lines(&child_path, &mut child.next_transcript_line, &emit)
                {
                    for event in grok_events_from_json(&value) {
                        // File-derived: ordinal-tagged so the daemon drops
                        // replays of already-projected history.
                        let ord = child_seq.entry(id.clone()).or_insert(0);
                        emit.emit(SessionEvent::NativeSubagent {
                            id: id.clone(),
                            parent_id: child.parent_id.clone(),
                            title: None,
                            state: child.state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                }
            }

            // Prefer the newest non-child session dir under this cwd. First
            // spawn discovers the id; after /clear a fresher root dir appears
            // and we rebind both transcript streams.
            let child_ids: HashSet<String> = children.keys().cloned().collect();
            if let Some(id) = find_session_id_excluding(&cwd, &child_ids) {
                if current_id.as_ref() != Some(&id) {
                    if let (Some(new_path), Some(new_updates_path)) = (
                        grok_transcript_path(&cwd, &id),
                        grok_updates_path(&cwd, &id),
                    ) {
                        if current_id.is_some() {
                            emit.log(format!(
                                "grok: native session id changed {:?} -> {id}; \
                                 rebinding transcript watcher",
                                current_id
                            ));
                        }
                        write_conv_id(&id);
                        current_id = Some(id);
                        path = Some(new_path);
                        updates_path = Some(new_updates_path);
                        next_line = 0;
                        next_update_line = 0;
                    }
                }
            }
        }
    });
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = command_override();
    let mut args = command.args.clone();
    args.extend(params.args.clone());

    if let Some(m) = params.model.as_ref() {
        args.push("--model".into());
        args.push(m.clone());
    }

    args.extend(grok_allow_args());

    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let grok_session_id = if resuming { read_conv_id() } else { None };

    if let Some(sid) = &grok_session_id {
        args.push("-r".into());
        args.push(sid.clone());
    } else if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push(prompt.clone());
        }
    }

    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), ctx.session_id.clone()));

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
        // Full-screen TUI: holds the foreground group; use daemon quiescence.
        detect_prompt_via_pgroup: false,
    };

    let cwd = PathBuf::from(&params.cwd);
    // Continuous discovery: Grok has no originator tag, so we track the
    // newest session dir under this cwd. That covers first spawn *and*
    // mid-session /clear (a fresh dir with a newer mtime).
    spawn_interactive_transcript_watcher(grok_session_id, cwd, ctx.emit.clone(), resuming);

    let _ = run_pty(spec, ctx).await;
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = command_override();
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut session_id = read_conv_id();
    let mut pending = VecDeque::new();
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
                    _ => continue,
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

        let mut child_args = command_override.args.clone();
        child_args.push("-p".into());
        child_args.push(user_text.clone());
        child_args.push("--output-format".into());
        child_args.push("streaming-json".into());

        child_args.extend(grok_allow_args());

        if let Some(sid) = &session_id {
            child_args.push("-r".into());
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
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        for (k, v) in &env {
            command.env(k, v);
        }
        command.env("CONSTRUCT_SESSION_ID", &agentd_session_id);

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

        let child_stdout = child.stdout.take().expect("piped");
        let child_stderr = child.stderr.take().expect("piped");

        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let parser_task = spawn_parser(child_stdout, emit.clone(), captured_sid.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = parser_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        // Always adopt the latest native id so a mid-run reset is honored
        // on subsequent turns (and written for daemon resume).
        if let Some(sid) = captured_sid.lock().unwrap().clone() {
            if session_id.as_ref() != Some(&sid) {
                write_conv_id(&sid);
                session_id = Some(sid);
            }
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
                    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
                    match ty {
                        "text" => {
                            if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                emit.emit(SessionEvent::Message {
                                    role: MessageRole::Assistant,
                                    text: data.to_string(),
                                });
                            }
                        }
                        "end" => {
                            if let Some(sid) = v.get("sessionId").and_then(|s| s.as_str()) {
                                let mut g = captured_sid.lock().unwrap();
                                // Keep the most recently observed id (not only the first).
                                *g = Some(sid.to_string());
                            }
                        }
                        "thought" => {
                            if let Some(data) = v.get("data").and_then(|d| d.as_str()) {
                                emit.log(format!("thought: {}", data));
                            }
                        }
                        _ => {}
                    }
                }
                Err(_) => {
                    emit.emit(SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: line,
                    });
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_message() {
        let v: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"Creating file","tool_calls":[]}"#,
        )
        .unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::Message { role, text } => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "Creating file");
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn parses_assistant_tool_calls() {
        let v: Value = serde_json::from_str(
            r#"{"type":"assistant","content":"","tool_calls":[{"id":"call-1","name":"Write","arguments":"{\"path\":\"hello.txt\"}"}]}"#
        ).unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            } => {
                assert_eq!(tool, "Write");
                assert_eq!(args["path"], "hello.txt");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool use event"),
        }
    }

    #[test]
    fn parses_tool_result() {
        let v: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"file written"}"#,
        )
        .unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            } => {
                assert_eq!(tool, "");
                assert!(*ok);
                assert_eq!(output, "file written");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool result event"),
        }
    }

    #[test]
    fn parses_cancelled_tool_result() {
        let v: Value = serde_json::from_str(
            r#"{"type":"tool_result","tool_call_id":"call-1","content":"User cancelled the execution"}"#
        ).unwrap();
        let events = grok_events_from_json(&v);
        assert_eq!(events.len(), 1);
        match &events[0] {
            SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            } => {
                assert_eq!(tool, "");
                assert!(!*ok);
                assert_eq!(output, "User cancelled the execution");
                assert_eq!(call_id.as_deref(), Some("call-1"));
            }
            _ => panic!("expected tool result event"),
        }
    }

    #[test]
    fn parses_native_subagent_spawn_and_finish_updates() {
        let owner = "019f4ae5-dc29-7142-9c7c-34dac1017cbc";
        let child = "019f4ae5-f3f4-7550-8182-39671f0959af";
        let spawned = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_spawned",
                "subagent_id": child,
                "parent_session_id": owner,
                "child_session_id": child,
                "description": "Print hello world"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&spawned, owner),
            Some(GrokNativeSubagentUpdate::Spawned {
                id: child.into(),
                parent_id: None,
                title: Some("Print hello world".into()),
            })
        );

        let finished = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_finished",
                "child_session_id": child,
                "status": "completed",
                "output": "hello world"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&finished, owner),
            Some(GrokNativeSubagentUpdate::Finished {
                id: child.into(),
                state: SessionState::Done,
            })
        );
    }

    #[test]
    fn native_subagent_spawn_preserves_nested_parent() {
        let spawned = serde_json::json!({
            "method": "_x.ai/session/update",
            "params": {"update": {
                "sessionUpdate": "subagent_spawned",
                "child_session_id": "grandchild",
                "parent_session_id": "child"
            }}
        });
        assert_eq!(
            grok_native_subagent_update(&spawned, "owner"),
            Some(GrokNativeSubagentUpdate::Spawned {
                id: "grandchild".into(),
                parent_id: Some("child".into()),
                title: None,
            })
        );
    }

    #[test]
    fn parses_json_encoded_user_content_for_child_transcript() {
        let value = serde_json::json!({
            "type": "user",
            "content": r#"[{"type":"text","text":"Print hello world"}]"#
        });
        match grok_events_from_json(&value).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::User));
                assert_eq!(text, "Print hello world");
            }
            other => panic!("unexpected child user events: {other:?}"),
        }
    }

    #[test]
    fn omits_internal_reminders_from_child_transcript() {
        let value = serde_json::json!({
            "type": "user",
            "content": r#"[{"type":"text","text":"\n<system-reminder>internal context</system-reminder>"}]"#
        });
        assert!(grok_events_from_json(&value).is_empty());
    }

    #[test]
    fn url_encodes_paths_correctly() {
        let path = Path::new("/Users/moon/agentd");
        assert_eq!(url_encode_path(path), "%2FUsers%2Fmoon%2Fagentd");
    }

    #[test]
    fn find_session_id_prefers_newest_mtime() {
        // Simulate /clear: two UUID session dirs under the same project
        // path; the newer mtime must win so resume tracks the active id.
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-home-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = Path::new("/tmp/agentd-grok-clear-test");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        std::fs::create_dir_all(&sessions).unwrap();

        let old_id = "aaaaaaaa-bbbb-cccc-dddd-000000000001";
        let new_id = "aaaaaaaa-bbbb-cccc-dddd-000000000002";
        std::fs::create_dir_all(sessions.join(old_id)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(new_id)).unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        assert_eq!(find_session_id(cwd).as_deref(), Some(new_id));
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
