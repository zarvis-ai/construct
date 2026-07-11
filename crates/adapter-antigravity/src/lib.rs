//! Google Antigravity CLI adapter (`agy`).
//!
//! Antigravity is the successor to the Gemini CLI, built on the Cascade
//! agent engine. Its CLI surface differs from gemini's enough that this
//! is a distinct adapter rather than a rename:
//!
//!   * Non-interactive (`--print`/`-p`) emits **plain text** on stdout —
//!     there is no `--output-format stream-json`. The structured trace
//!     (tool calls, results, planner responses) is written to a
//!     per-conversation transcript file instead.
//!   * Conversations are identified by an auto-assigned UUID. There's no
//!     `--session-id` to pre-mint one, but `--conversation <id>` resumes
//!     an existing conversation. We discover the id by parsing the
//!     `Created conversation <uuid>` line agy writes to its `--log-file`.
//!   * Tool auto-approval is `--dangerously-skip-permissions` (used only
//!     in headless mode — interactive sessions approve in the real TUI).
//!
//! Two modes, picked like the other adapters:
//!
//!   * **interactive (default with a PTY)** — spawn `agy` under a PTY so
//!     the right pane is the real Antigravity TUI. A background task
//!     captures the conversation id from the log so a daemon restart can
//!     `--conversation <id>` back into the same thread.
//!   * **headless (no PTY)** — per turn, run
//!     `agy -p <text> --dangerously-skip-permissions --log-file <log>`
//!     (plus `--conversation <id>` after the first turn), then parse the
//!     conversation transcript and emit the *new* steps as structured
//!     `Message` / `ToolUse` / `ToolResult` events.
//!
//! ## Transcript layout
//!
//! After a run, structured steps live at
//! `<antigravity_home>/brain/<conversation_id>/.system_generated/logs/transcript.jsonl`,
//! one JSON object per line:
//!
//! ```text
//! {"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"..."}
//! {"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{...}}]}
//! {"step_index":3,"source":"MODEL","type":"RUN_COMMAND","content":"...output..."}
//! {"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","content":"final answer"}
//! ```
//!
//! `<antigravity_home>` defaults to `$HOME/.gemini/antigravity-cli`
//! (antigravity currently nests under the gemini dir); override with
//! `CONSTRUCT_ANTIGRAVITY_HOME`.
//!
//! Env overrides: `CONSTRUCT_ANTIGRAVITY_CMD` (full command prefix),
//! `CONSTRUCT_ANTIGRAVITY_BIN` (binary, default `agy`),
//! `CONSTRUCT_ANTIGRAVITY_MODE` (`interactive`|`headless`).

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use construct_adapter_common::{drive_turn, next_native_seq, TurnOutcome};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "agy".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities {
            supports_input: true,
            supports_interrupt: true,
            // agy exposes no token/cost data in print mode or its logs.
            supports_cost: false,
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
    if let Ok(m) = std::env::var("CONSTRUCT_ANTIGRAVITY_MODE") {
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

fn command_override() -> construct_protocol::adapter::CommandOverride {
    construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_ANTIGRAVITY_CMD",
        "CONSTRUCT_ANTIGRAVITY_BIN",
        "agy",
    )
}

// The daemon's auto-approval policy (`CONSTRUCT_AUTO_APPROVE_PATHS`, see
// `construct_protocol::adapter::policy`) is set, but `agy` only exposes a
// global `--dangerously-skip-permissions` and no path-scoped allow-list, so
// there's no native translation to apply in interactive mode. Headless mode
// already auto-approves via that global flag, which is why widget writes
// don't prompt there. A finer-grained translation would need an upstream
// agy feature or for construct to intercept its tool calls.

fn session_data_dir() -> Option<PathBuf> {
    std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(PathBuf::from)
}

/// Antigravity's home dir, where per-conversation `brain/<id>` trees
/// (and their `.system_generated/logs/transcript.jsonl`) live. Defaults
/// to `$HOME/.gemini/antigravity-cli`; override with
/// `CONSTRUCT_ANTIGRAVITY_HOME`.
fn antigravity_home() -> Option<PathBuf> {
    if let Ok(h) = std::env::var("CONSTRUCT_ANTIGRAVITY_HOME") {
        return Some(PathBuf::from(h));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".gemini").join("antigravity-cli"))
}

fn transcript_path(conversation_id: &str) -> Option<PathBuf> {
    Some(
        antigravity_home()?
            .join("brain")
            .join(conversation_id)
            .join(".system_generated")
            .join("logs")
            .join("transcript.jsonl"),
    )
}

/// File where we stash the conversation id so a daemon restart can
/// resume the same antigravity conversation via `--conversation <id>`.
fn conv_id_file() -> Option<PathBuf> {
    Some(session_data_dir()?.join("agy_conversation_id.txt"))
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

/// Parse the `Created conversation <uuid>` line agy writes to its
/// `--log-file`. Returns the **last** match so a mid-session clear that
/// creates a second conversation supersedes the original. (`Conversation
/// using project ID: <uuid>` is a *different* id — the project, not the
/// conversation — so we anchor on the exact "Created conversation"
/// wording.)
fn parse_conversation_id(log_text: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in log_text.lines() {
        if let Some(idx) = line.find("Created conversation ") {
            let rest = &line[idx + "Created conversation ".len()..];
            let id: String = rest
                .trim()
                .chars()
                .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
                .collect();
            if id.len() == 36 {
                last = Some(id);
            }
        }
    }
    last
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = command_override();
    let mut args = command.args.clone();
    args.extend(params.args.clone());

    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let log_path = session_data_dir().map(|d| d.join("agy.log"));
    if let Some(lp) = &log_path {
        args.push("--log-file".into());
        args.push(lp.to_string_lossy().to_string());
    }

    // Resume into the prior conversation when the daemon respawns us.
    let existing = if resuming { read_conv_id() } else { None };
    if let Some(id) = &existing {
        args.push("--conversation".into());
        args.push(id.clone());
    }

    // Initial prompt → `-i` (run prompt, then stay interactive). Skip on
    // resume; that turn is already in the conversation we're rejoining.
    if !resuming {
        if let Some(prompt) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            args.push("-i".into());
            args.push(prompt.clone());
        }
    }

    // Mirror Antigravity's native transcript into agentd semantic events
    // while keeping the PTY as the interactive surface.
    spawn_interactive_transcript_watcher(
        existing.clone(),
        log_path.clone(),
        ctx.emit.clone(),
        existing.is_some(),
    );

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
        cwd: PathBuf::from(&params.cwd),
        env,
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} (interactive)")),
        // Full-screen TUI: holds the foreground group; use daemon quiescence.
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty(spec, ctx).await;
}

fn spawn_interactive_transcript_watcher(
    existing_id: Option<String>,
    log_path: Option<PathBuf>,
    emit: EventEmitter,
    skip_existing: bool,
) {
    tokio::spawn(async move {
        let mut conv_id = existing_id;
        let mut last_step = -1;
        // First attach of a resumed id can skip prior transcript steps;
        // every later rebind (post-/clear) starts fresh.
        let mut skip_next_transcript = skip_existing && conv_id.is_some();
        let mut children: HashMap<String, AntigravityNativeChild> = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;

            // Always re-scan the log. Agy appends another
            // "Created conversation" after /clear; parse returns the last
            // match so we track the active native id, not the first.
            if let Some(lp) = &log_path {
                if let Ok(text) = std::fs::read_to_string(lp) {
                    if let Some(id) = parse_conversation_id(&text) {
                        if conv_id.as_ref() != Some(&id) {
                            if conv_id.is_some() {
                                emit.log(format!(
                                    "antigravity: native conversation id changed {:?} -> {id}; \
                                     rebinding transcript watcher",
                                    conv_id
                                ));
                            }
                            write_conv_id(&id);
                            conv_id = Some(id);
                            last_step = -1;
                            children.clear();
                            // Only the initial resume attach skips history.
                            skip_next_transcript = false;
                        }
                    }
                }
            }

            let Some(id) = conv_id.as_ref() else {
                continue;
            };
            let Some(tp) = transcript_path(id) else {
                continue;
            };
            if skip_next_transcript {
                last_step = max_step_index(&tp);
                let (_, values) = read_new_transcript_steps(&tp, -1);
                apply_antigravity_native_updates(
                    values.iter().map(|value| (None, value)),
                    &mut children,
                    None,
                );
                skip_next_transcript = false;
            }
            let (next_step, root_values) = read_new_transcript_steps(&tp, last_step);
            last_step = next_step;
            for value in &root_values {
                emit_step(value, &emit);
            }

            let mut lifecycle_values: Vec<(Option<String>, Value)> =
                root_values.into_iter().map(|value| (None, value)).collect();
            let child_ids: Vec<String> = children.keys().cloned().collect();
            for child_id in child_ids {
                let Some(child) = children.get_mut(&child_id) else {
                    continue;
                };
                let Some(child_path) = transcript_path(&child_id) else {
                    continue;
                };
                let (next_step, values) = read_new_transcript_steps(&child_path, child.last_step);
                child.last_step = next_step;
                for value in values {
                    for event in antigravity_events_from_step(&value) {
                        // File-derived: ordinal-tagged so the daemon drops
                        // replays of already-projected history.
                        let ord = child_seq.entry(child_id.clone()).or_insert(0);
                        emit.emit(SessionEvent::NativeSubagent {
                            id: child_id.clone(),
                            parent_id: child.parent_id.clone(),
                            title: None,
                            state: child.state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                    lifecycle_values.push((Some(child_id.clone()), value));
                }
            }
            apply_antigravity_native_updates(
                lifecycle_values
                    .iter()
                    .map(|(parent_id, value)| (parent_id.as_deref(), value)),
                &mut children,
                Some(&emit),
            );
        }
    });
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = command_override();
    let cwd = PathBuf::from(&params.cwd);
    let extra_args = params.args.clone();
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), session_id.clone()));

    // Resume bookkeeping: known conversation id + how many transcript
    // steps we've already emitted (so we only forward NEW steps each
    // turn, and don't re-emit the whole thread on a daemon restart).
    let mut conv_id: Option<String> = read_conv_id();
    let mut last_step: i64 = -1;
    if let Some(id) = &conv_id {
        // On resume, skip everything already in the transcript.
        if let Some(tp) = transcript_path(id) {
            last_step = max_step_index(&tp);
        }
    }

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
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
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

        let log_path = session_data_dir()
            .map(|d| d.join("agy-headless.log"))
            .unwrap_or_else(|| PathBuf::from("agy-headless.log"));

        let mut child_args: Vec<String> = command_override.args.clone();
        child_args.push("-p".into());
        child_args.push(user_text.clone());
        child_args.push("--dangerously-skip-permissions".into());
        child_args.push("--log-file".into());
        child_args.push(log_path.to_string_lossy().to_string());
        if let Some(id) = &conv_id {
            child_args.push("--conversation".into());
            child_args.push(id.clone());
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

        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                emit.emit(SessionEvent::Error {
                    message: construct_protocol::adapter::missing_bin_hint(
                        &command_override.argv_preview(),
                        &e,
                    ),
                });
                break 127;
            }
        };

        // Drain stdout/stderr so the pipe doesn't fill and block the
        // child. We don't forward stdout as Message events — the
        // structured transcript is the source of truth — but we keep
        // stderr as logs for debugging.
        if let Some(out) = child.stdout.take() {
            tokio::spawn(drain_to_void(out));
        }
        if let Some(err) = child.stderr.take() {
            let emit_log = emit.clone();
            tokio::spawn(drain_to_log(err, emit_log));
        }

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;
        let _ = child.wait().await;

        // Learn / refresh the conversation id from this turn's log. A
        // mid-run clear can mint a new id; subsequent turns must resume
        // that one, not the first we ever saw.
        if let Ok(text) = std::fs::read_to_string(&log_path) {
            if let Some(id) = parse_conversation_id(&text) {
                if conv_id.as_ref() != Some(&id) {
                    write_conv_id(&id);
                    // New conversation → re-emit from the start of its transcript.
                    last_step = -1;
                    conv_id = Some(id);
                }
            }
        }
        if let Some(id) = &conv_id {
            if let Some(tp) = transcript_path(id) {
                last_step = emit_new_transcript_steps(&tp, last_step, &emit);
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

async fn drain_to_void<R>(reader: R)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::AsyncReadExt;
    let mut reader = reader;
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

async fn drain_to_log<R>(reader: R, emit: EventEmitter)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            emit.log(format!("stderr: {line}"));
        }
    }
}

#[derive(Debug)]
struct AntigravityNativeChild {
    parent_id: Option<String>,
    state: SessionState,
    last_step: i64,
}

fn antigravity_spawned_subagent_ids(value: &Value) -> Vec<String> {
    if value.get("type").and_then(Value::as_str) != Some("INVOKE_SUBAGENT") {
        return Vec::new();
    }
    let Some(content) = value.get("content").and_then(Value::as_str) else {
        return Vec::new();
    };
    json_objects_in_text(content)
        .into_iter()
        .filter_map(|object| {
            object
                .get("conversationId")
                .and_then(Value::as_str)
                .filter(|id| id.len() == 36)
                .map(str::to_string)
        })
        .collect()
}

fn antigravity_message_sender(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("SYSTEM_MESSAGE") {
        return None;
    }
    let content = value.get("content").and_then(Value::as_str)?;
    let sender = content.split("sender=").nth(1)?;
    let id: String = sender
        .chars()
        .take_while(|character| character.is_ascii_hexdigit() || *character == '-')
        .collect();
    (id.len() == 36).then_some(id)
}

fn json_objects_in_text(text: &str) -> Vec<Value> {
    let mut values = Vec::new();
    let mut start = None;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' if start.is_some() => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start.take() {
                        if let Ok(value) = serde_json::from_str(&text[start..=index]) {
                            values.push(value);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    values
}

fn apply_antigravity_native_updates<'a>(
    values: impl IntoIterator<Item = (Option<&'a str>, &'a Value)>,
    children: &mut HashMap<String, AntigravityNativeChild>,
    emit: Option<&EventEmitter>,
) {
    for (native_parent_id, value) in values {
        for id in antigravity_spawned_subagent_ids(value) {
            let parent_id = native_parent_id.map(str::to_string);
            let is_new = !children.contains_key(&id);
            let child = children
                .entry(id.clone())
                .or_insert(AntigravityNativeChild {
                    parent_id: parent_id.clone(),
                    state: SessionState::Running,
                    last_step: -1,
                });
            child.parent_id = parent_id.clone();
            child.state = SessionState::Running;
            if is_new {
                if let Some(emit) = emit {
                    emit.emit(SessionEvent::NativeSubagent {
                        id: id.clone(),
                        parent_id,
                        title: Some(format!("Antigravity subagent {}", &id[..8])),
                        state: SessionState::Running,
                        event: None,
                        seq: None,
                    });
                }
            }
        }

        let Some(id) = antigravity_message_sender(value) else {
            continue;
        };
        let Some(child) = children.get_mut(&id) else {
            continue;
        };
        if child.state == SessionState::Done {
            continue;
        }
        child.state = SessionState::Done;
        if let Some(emit) = emit {
            emit.emit(SessionEvent::NativeSubagent {
                id,
                parent_id: child.parent_id.clone(),
                title: None,
                state: SessionState::Done,
                event: None,
                seq: None,
            });
        }
    }
}

/// Highest `step_index` currently in a transcript file, or -1 if the
/// file is missing/empty/unparseable.
fn max_step_index(path: &Path) -> i64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return -1;
    };
    let mut max = -1i64;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(line) {
            if let Some(i) = v.get("step_index").and_then(|n| n.as_i64()) {
                max = max.max(i);
            }
        }
    }
    max
}

/// Read `path` and emit every step with `step_index > after` as the
/// appropriate `SessionEvent`. Returns the new high-water step index.
fn emit_new_transcript_steps(path: &Path, after: i64, emit: &EventEmitter) -> i64 {
    let (high, values) = read_new_transcript_steps(path, after);
    for value in values {
        emit_step(&value, emit);
    }
    high
}

fn read_new_transcript_steps(path: &Path, after: i64) -> (i64, Vec<Value>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (after, Vec::new());
    };
    let mut high = after;
    let mut values = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let idx = match v.get("step_index").and_then(|n| n.as_i64()) {
            Some(i) => i,
            None => continue,
        };
        if idx <= after {
            continue;
        }
        high = high.max(idx);
        values.push(v);
    }
    (high, values)
}

/// Map one transcript step to a `SessionEvent`.
fn emit_step(v: &Value, emit: &EventEmitter) {
    for event in antigravity_events_from_step(v) {
        emit.emit(event);
    }
}

fn antigravity_events_from_step(v: &Value) -> Vec<SessionEvent> {
    let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
    match ty {
        // Conversation-history records are structural, but user input is a
        // real chat message. Keeping it gives Antigravity sessions (and their
        // native subagent mirrors) the same message-count lineage stats as
        // every other harness.
        "CONVERSATION_HISTORY" => Vec::new(),
        "USER_INPUT" => v
            .get("content")
            .and_then(|s| s.as_str())
            .filter(|content| !content.is_empty())
            .map(|content| {
                vec![SessionEvent::Message {
                    role: MessageRole::User,
                    text: content.to_string(),
                }]
            })
            .unwrap_or_default(),
        "PLANNER_RESPONSE" => {
            // Either a tool-call decision or assistant prose.
            if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
                let mut out = Vec::new();
                for c in calls {
                    let name = c
                        .get("name")
                        .and_then(|s| s.as_str())
                        .unwrap_or("?")
                        .to_string();
                    let args = c.get("args").cloned().unwrap_or(Value::Null);
                    // Antigravity tool_call objects may carry an `id`; if not,
                    // there is no stable correlation key in this transcript
                    // format, so leave it None.
                    let call_id = c.get("id").and_then(|v| v.as_str()).map(str::to_string);
                    out.push(SessionEvent::ToolUse {
                        tool: name,
                        args,
                        call_id,
                    });
                }
                out
            } else if let Some(content) = v.get("content").and_then(|s| s.as_str()) {
                if !content.is_empty() {
                    vec![SessionEvent::Message {
                        role: MessageRole::Assistant,
                        text: content.to_string(),
                    }]
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        }
        // Any other step type is a tool-result step named after the tool
        // action (RUN_COMMAND, VIEW_FILE, EDIT_FILE, …). The status field
        // tells us ok/err; content carries the output.
        _ => {
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let ok = matches!(status, "DONE" | "SUCCESS" | "COMPLETED");
            let output = v
                .get("content")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            // The result step is named after the tool action (`ty`), which is
            // the real tool name, not an id; this transcript format has no
            // stable correlation key, so leave `call_id` None.
            vec![SessionEvent::ToolResult {
                tool: ty.to_string(),
                ok,
                output,
                call_id: None,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_conversation_id_from_log_line() {
        let log = "I0522 01:17:15.555 server.go:726] Conversation using project ID: fd861473-c03d-48d4-993b-7cf1e55cc70f\n\
                   I0522 01:17:15.555 server.go:747] Created conversation b6eae99e-b76c-4417-837f-ea8adae0a2ba\n";
        assert_eq!(
            parse_conversation_id(log).as_deref(),
            Some("b6eae99e-b76c-4417-837f-ea8adae0a2ba")
        );
    }

    #[test]
    fn parse_conversation_id_prefers_last_after_clear() {
        // Agy appends another "Created conversation" when the user clears;
        // resume must follow the newest id, not the original spawn.
        let log = "I0522 01:17:15.555 server.go:747] Created conversation b6eae99e-b76c-4417-837f-ea8adae0a2ba\n\
                   I0522 01:20:00.000 server.go:747] Created conversation aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n";
        assert_eq!(
            parse_conversation_id(log).as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
    }

    #[test]
    fn parse_conversation_id_none_when_absent() {
        assert_eq!(parse_conversation_id("no id here\nanother line"), None);
    }

    #[test]
    fn max_step_index_reads_highest() {
        let dir = std::env::temp_dir().join(format!(
            "agy-test-maxstep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.jsonl");
        std::fs::write(
            &p,
            "{\"step_index\":0,\"type\":\"USER_INPUT\"}\n\
             {\"step_index\":3,\"type\":\"PLANNER_RESPONSE\",\"content\":\"hi\"}\n",
        )
        .unwrap();
        assert_eq!(max_step_index(&p), 3);
        assert_eq!(max_step_index(&dir.join("missing.jsonl")), -1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conversation_history_steps_do_not_emit_chat_events() {
        let v: Value =
            serde_json::from_str(r#"{"type":"CONVERSATION_HISTORY","content":"x"}"#).unwrap();
        assert!(antigravity_events_from_step(&v).is_empty());
    }

    #[test]
    fn user_input_emits_user_message_for_lineage_counts() {
        let v: Value = serde_json::from_str(r#"{"type":"USER_INPUT","content":"x"}"#).unwrap();
        match antigravity_events_from_step(&v).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::User));
                assert_eq!(text, "x");
            }
            other => panic!("unexpected message events: {other:?}"),
        }
    }

    #[test]
    fn planner_response_content_emits_assistant_message() {
        let v: Value =
            serde_json::from_str(r#"{"type":"PLANNER_RESPONSE","content":"final"}"#).unwrap();
        match antigravity_events_from_step(&v).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "final");
            }
            other => panic!("unexpected message events: {other:?}"),
        }
    }

    #[test]
    fn planner_response_tool_calls_emit_tool_uses() {
        let v: Value = serde_json::from_str(
            r#"{"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"cmd":"ls"}}]}"#,
        )
        .unwrap();
        match antigravity_events_from_step(&v).as_slice() {
            [SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }] => {
                assert_eq!(tool, "run_command");
                assert_eq!(args["cmd"], "ls");
                assert_eq!(*call_id, None);
            }
            other => panic!("unexpected tool-use events: {other:?}"),
        }
    }

    #[test]
    fn tool_step_emits_tool_result() {
        let v: Value =
            serde_json::from_str(r#"{"type":"RUN_COMMAND","status":"DONE","content":"out"}"#)
                .unwrap();
        match antigravity_events_from_step(&v).as_slice() {
            [SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            }] => {
                assert_eq!(tool, "RUN_COMMAND");
                assert!(*ok);
                assert_eq!(output, "out");
                assert_eq!(*call_id, None);
            }
            other => panic!("unexpected tool-result events: {other:?}"),
        }
    }

    #[test]
    fn parses_native_subagent_creation_result() {
        let value = serde_json::json!({
            "type": "INVOKE_SUBAGENT",
            "status": "DONE",
            "content": "Created the following subagents:\n{\n  \"conversationId\": \"825753eb-7780-4b85-a59b-f86fe4972dd9\",\n  \"workspaceUris\": [\"file:///tmp/worktree\"]\n}\nThe subagents will send a message when done."
        });
        assert_eq!(
            antigravity_spawned_subagent_ids(&value),
            vec!["825753eb-7780-4b85-a59b-f86fe4972dd9"]
        );
    }

    #[test]
    fn parses_native_subagent_completion_message() {
        let value = serde_json::json!({
            "type": "SYSTEM_MESSAGE",
            "status": "DONE",
            "content": "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-07-10T07:29:31Z sender=825753eb-7780-4b85-a59b-f86fe4972dd9 priority=MESSAGE_PRIORITY_HIGH content=hello world\n</SYSTEM_MESSAGE>"
        });
        assert_eq!(
            antigravity_message_sender(&value).as_deref(),
            Some("825753eb-7780-4b85-a59b-f86fe4972dd9")
        );
    }

    #[test]
    fn ignores_non_subagent_json_in_antigravity_output() {
        let value = serde_json::json!({
            "type": "RUN_COMMAND",
            "content": "command output: {\"conversationId\":\"825753eb-7780-4b85-a59b-f86fe4972dd9\"}"
        });
        assert!(antigravity_spawned_subagent_ids(&value).is_empty());
    }
}
