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
//! Pick mode via `--mode interactive|headless` on `construct new`, or via
//! `CONSTRUCT_CLAUDE_MODE=interactive|headless`. Default is interactive when the
//! client supplies a PTY size (the TUI always does); otherwise headless.
//!
//! Honors `CONSTRUCT_CLAUDE_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_CLAUDE_BIN` for a binary path.

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use construct_adapter_common::{drive_turn, next_native_seq, spawn_stderr_log, TurnOutcome};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

const MAX_CLAUDE_INITIAL_PROMPT_ARG_BYTES: usize = 3500;

pub async fn run() -> anyhow::Result<()> {
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
    if let Ok(m) = std::env::var("CONSTRUCT_CLAUDE_MODE") {
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

/// Generate a minimal Claude `--settings` file for adapter bookkeeping hooks
/// and return its path. Always includes a `SessionStart` hook that rewrites
/// `claude_session_id.txt` whenever Claude mints a new native id (startup,
/// resume, `/clear`, compact). Optionally also registers the AskUserQuestion
/// chat-gate `PreToolUse` hook (shells out to `construct ask-gate`) unless
/// disabled via `CONSTRUCT_CLAUDE_ASKGATE=0`.
///
/// Returns `None` when the session data dir can't be located. Verified that
/// `--settings` *merges* with the user's existing settings/hooks, so this
/// never clobbers their setup.
fn adapter_settings_path() -> Option<PathBuf> {
    let dir = std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)?;
    std::fs::create_dir_all(&dir).ok()?;

    let sid_file = dir.join("claude_session_id.txt");
    let capture_script = dir.join("capture-claude-session-id.sh");
    write_session_id_capture_script(&capture_script, &sid_file)?;

    let mut hooks = serde_json::Map::new();
    // No matcher → fires on every SessionStart (startup | resume | clear | compact).
    // After `/clear` Claude assigns a fresh native id; we must persist it so
    // daemon restart / same-harness fork resume the active conversation, not
    // the pre-clear one.
    hooks.insert(
        "SessionStart".into(),
        serde_json::json!([{
            "hooks": [{
                "type": "command",
                "command": capture_script.to_string_lossy(),
            }],
        }]),
    );

    if std::env::var("CONSTRUCT_CLAUDE_ASKGATE").as_deref() != Ok("0") {
        if let Some(client) = construct_protocol::paths::locate_sibling_binary("construct") {
            hooks.insert(
                "PreToolUse".into(),
                serde_json::json!([{
                    "matcher": "AskUserQuestion",
                    "hooks": [{
                        "type": "command",
                        "command": format!("\"{}\" ask-gate", client.display()),
                    }],
                }]),
            );
        }
    }

    let path = dir.join("agentd-adapter-settings.json");
    let settings = serde_json::json!({ "hooks": hooks });
    std::fs::write(&path, serde_json::to_vec_pretty(&settings).ok()?).ok()?;
    Some(path)
}

/// Write a tiny POSIX shell script that extracts `session_id` from Claude's
/// SessionStart hook JSON on stdin and persists it to `sid_file`. Avoids a
/// python/jq dependency so the hook works in minimal environments.
fn write_session_id_capture_script(script: &Path, sid_file: &Path) -> Option<()> {
    let body = format!(
        r#"#!/bin/sh
# Written by construct's claude adapter. Persists the active Claude native
# session id so resume/fork track /clear and /branch switches.
set -eu
SID_FILE="{sid}"
tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT
cat >"$tmp"
sid=$(sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp" | head -n1)
if [ -n "$sid" ]; then
  printf '%s' "$sid" >"$SID_FILE"
fi
"#,
        sid = sid_file.display()
    );
    std::fs::write(script, body).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(script).ok()?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(script, perms).ok()?;
    }
    Some(())
}

async fn run_interactive(params: SessionStartParams, ctx: AdapterContext) {
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CLAUDE_CMD",
        "CONSTRUCT_CLAUDE_BIN",
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
    // etc.). Opt out with CONSTRUCT_INJECT_MCP=0.
    if let Some(cfg) = construct_protocol::adapter::maybe_inject_mcp_config(&ctx.session_id) {
        args.push("--mcp-config".into());
        args.push(cfg.to_string_lossy().to_string());
    }
    // Inject adapter bookkeeping hooks (native session-id capture after
    // /clear etc., plus the optional AskUserQuestion chat-gate). Merges
    // with the user's settings.
    if let Some(p) = adapter_settings_path() {
        args.push("--settings".into());
        args.push(p.to_string_lossy().to_string());
    }
    // Translate the daemon-defined auto-approval policy into Claude's native
    // `--allowed-tools` patterns. Single policy in agentd; each adapter
    // applies it in its harness's native mechanism.
    args.extend(
        construct_protocol::adapter::policy::AutoApprovePolicy::from_env().claude_allowed_tools_args(),
    );
    // Resume support: stash our own UUID under
    // $CONSTRUCT_SESSION_DATA_DIR/claude_session_id.txt at first spawn (passed
    // to claude as --session-id), then pass it back as --resume when the
    // daemon respawns us after a restart. claude's own session-persistence
    // makes the conversation pick up where it left off.
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("claude_session_id.txt"));
    let fork_from = std::env::var("CONSTRUCT_CLAUDE_FORK_FROM")
        .ok()
        .filter(|s| !s.is_empty());
    let claude_session_id = match (resuming, sid_file.as_ref()) {
        (true, Some(p)) if p.exists() => std::fs::read_to_string(p)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let watch_session_id = if let Some(parent) = &fork_from {
        // Claude creates a new native conversation inheriting the parent's
        // exact context; keep a fresh id file for future daemon resumes.
        args.push("--resume".into());
        args.push(parent.clone());
        args.push("--fork-session".into());
        if let Some(p) = &sid_file {
            let id = uuid::Uuid::new_v4().to_string();
            let _ = std::fs::write(p, &id);
            Some(id)
        } else {
            None
        }
    } else if let Some(sid) = &claude_session_id {
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
            args.push(interactive_initial_prompt_arg(prompt));
        }
    }
    // Surface the session id to the child's env so agents that aren't using
    // MCP (or the user, via `echo $CONSTRUCT_SESSION_ID`) can still tell.
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    env.push(("CONSTRUCT_SESSION_ID".into(), ctx.session_id.clone()));
    if let Some(session_id) = watch_session_id {
        spawn_interactive_transcript_watcher(
            session_id,
            sid_file.clone(),
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
        // Full-screen TUI: holds the foreground group; use daemon quiescence.
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty(spec, ctx).await;
}

fn interactive_initial_prompt_arg(prompt: &str) -> String {
    if prompt.len() <= MAX_CLAUDE_INITIAL_PROMPT_ARG_BYTES {
        return prompt.to_string();
    }

    match write_oversize_initial_prompt(prompt) {
        Some(path) => format!(
            "Read the initial prompt and any forked session context from `{}` before taking \
             action. Continue from that context.",
            path.display()
        ),
        None => truncate_initial_prompt_arg(prompt),
    }
}

fn write_oversize_initial_prompt(prompt: &str) -> Option<PathBuf> {
    let dir = std::env::var_os("CONSTRUCT_SESSION_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("claude-initial-prompt.md");
    std::fs::write(&path, prompt).ok()?;
    Some(path)
}

fn truncate_initial_prompt_arg(prompt: &str) -> String {
    let suffix = "\n\n[Initial prompt truncated before launch because it exceeded Claude's terminal metadata limit.]";
    let budget = MAX_CLAUDE_INITIAL_PROMPT_ARG_BYTES.saturating_sub(suffix.len());
    let mut end = budget.min(prompt.len());
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &prompt[..end], suffix)
}

fn spawn_interactive_transcript_watcher(
    session_id: String,
    sid_file: Option<PathBuf>,
    cwd: PathBuf,
    emit: EventEmitter,
    skip_existing: bool,
) {
    let Some(initial_path) = claude_transcript_path(&cwd, &session_id) else {
        emit.log("claude: no CLAUDE_HOME or HOME — cannot watch native transcript");
        return;
    };
    tokio::spawn(async move {
        let mut current_id = session_id;
        let mut path = initial_path;
        // On resume we skip history already in the transcript; after a mid-
        // session native id change (/clear, /branch, /resume) we always start
        // at the top of the new file so chat mode sees the fresh conversation.
        let mut next_line = if skip_existing {
            count_jsonl_lines(&path)
        } else {
            0
        };
        let mut subagents_dir = path.parent().map(|p| {
            p.join(path.file_stem().unwrap_or_default())
                .join("subagents")
        });
        let mut child_lines: HashMap<String, usize> = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        let mut child_states: HashMap<String, SessionState> = HashMap::new();
        let mut child_parents: HashMap<String, String> = HashMap::new();
        let mut last_snapshot: Option<Vec<String>> = None;
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if let Some(new_id) = read_updated_session_id(sid_file.as_deref(), &current_id) {
                if let Some(new_path) = claude_transcript_path(&cwd, &new_id) {
                    emit.log(format!(
                        "claude: native session id changed {current_id} -> {new_id}; \
                         rebinding transcript watcher"
                    ));
                    current_id = new_id;
                    path = new_path;
                    next_line = 0;
                    subagents_dir = path.parent().map(|p| {
                        p.join(path.file_stem().unwrap_or_default())
                            .join("subagents")
                    });
                    child_lines.clear();
                    child_seq.clear();
                    child_states.clear();
                    child_parents.clear();
                }
            }
            let root_values = emit_new_claude_transcript_lines(&path, &mut next_line, &emit);
            for value in root_values {
                if let Some((id, state, title)) = claude_native_subagent_update(&value) {
                    child_states.insert(id.clone(), state);
                    emit.emit(SessionEvent::NativeSubagent {
                        id: id.clone(),
                        parent_id: None,
                        title,
                        state,
                        event: None,
                        seq: None,
                    });
                    if matches!(state, SessionState::Done | SessionState::Errored) {
                        emit.emit(SessionEvent::NativeSubagentRemoved { id });
                    }
                }
            }
            let Some(dir) = subagents_dir.as_ref() else {
                continue;
            };
            let entries = match std::fs::read_dir(dir) {
                Ok(entries) => entries,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if last_snapshot.as_ref().is_some_and(|ids| !ids.is_empty()) {
                        emit.emit(SessionEvent::NativeSubagentSnapshot { ids: Vec::new() });
                        last_snapshot = Some(Vec::new());
                    }
                    continue;
                }
                Err(_) => continue,
            };
            let mut retained_native_ids = Vec::new();
            for entry in entries.flatten() {
                let child_path = entry.path();
                let Some(native_id) = claude_subagent_id_from_path(&child_path) else {
                    continue;
                };
                retained_native_ids.push(native_id.clone());
                let first_seen = !child_lines.contains_key(&native_id);
                // Child files are ALWAYS read from the top — pre-existing
                // history backfills into the mirror instead of being skipped
                // on resume/restart. Every emission derived from a file
                // carries a deterministic ordinal in its TARGET child's
                // stream; the daemon drops ordinals below the mirror's
                // high-water mark, so re-scans never duplicate.
                let next = child_lines.entry(native_id.clone()).or_insert(0);
                let state = child_states
                    .get(&native_id)
                    .copied()
                    .unwrap_or(SessionState::Running);
                if first_seen {
                    let ord = child_seq.entry(native_id.clone()).or_insert(0);
                    emit.emit(SessionEvent::NativeSubagent {
                        id: native_id.clone(),
                        parent_id: child_parents.get(&native_id).cloned(),
                        title: Some(format!("Claude subagent {}", short_native_id(&native_id))),
                        state,
                        event: None,
                        seq: Some(next_native_seq(ord)),
                    });
                }
                for value in read_new_claude_values(&child_path, next, &emit) {
                    if let Some((nested_id, nested_state, title)) =
                        claude_native_subagent_update(&value)
                    {
                        child_states.insert(nested_id.clone(), nested_state);
                        child_parents.insert(nested_id.clone(), native_id.clone());
                        let ord = child_seq.entry(nested_id.clone()).or_insert(0);
                        emit.emit(SessionEvent::NativeSubagent {
                            id: nested_id.clone(),
                            parent_id: Some(native_id.clone()),
                            title,
                            state: nested_state,
                            event: None,
                            seq: Some(next_native_seq(ord)),
                        });
                        if matches!(nested_state, SessionState::Done | SessionState::Errored) {
                            emit.emit(SessionEvent::NativeSubagentRemoved { id: nested_id });
                        }
                    }
                    for event in claude_events_from_json(&value) {
                        let ord = child_seq.entry(native_id.clone()).or_insert(0);
                        emit.emit(SessionEvent::NativeSubagent {
                            id: native_id.clone(),
                            parent_id: child_parents.get(&native_id).cloned(),
                            title: None,
                            state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                }
            }
            retained_native_ids.sort();
            retained_native_ids.dedup();
            if last_snapshot.as_ref() != Some(&retained_native_ids) {
                emit.emit(SessionEvent::NativeSubagentSnapshot {
                    ids: retained_native_ids.clone(),
                });
                last_snapshot = Some(retained_native_ids);
            }
        }
    });
}

/// If `sid_file` holds a non-empty id different from `current`, return it.
fn read_updated_session_id(sid_file: Option<&Path>, current: &str) -> Option<String> {
    let path = sid_file?;
    let raw = std::fs::read_to_string(path).ok()?;
    let next = raw.trim();
    if next.is_empty() || next == current {
        return None;
    }
    Some(next.to_string())
}

fn claude_transcript_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    let home = std::env::var("CONSTRUCT_CLAUDE_HOME")
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

fn emit_new_claude_transcript_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut values = Vec::new();
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
            Ok(v) => {
                emit_event_from_json(emit, v.clone());
                values.push(v);
            }
            Err(e) => emit.log(format!(
                "claude transcript: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
    values
}

fn read_new_claude_values(path: &Path, next_line: &mut usize, emit: &EventEmitter) -> Vec<Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = 0usize;
    let mut values = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        seen = idx + 1;
        if idx < *next_line || line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(value) => values.push(value),
            Err(error) => emit.log(format!(
                "claude subagent transcript: failed to parse {} line {}: {error}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
    values
}

fn claude_native_subagent_update(value: &Value) -> Option<(String, SessionState, Option<String>)> {
    let raw = serde_json::to_string(value).ok()?;
    let id = value
        .pointer("/toolUseResult/agentId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| xml_tag(&raw, "task-id"))
        .or_else(|| agent_id_from_text(&raw))?;
    let id = normalize_claude_agent_id(&id);
    let status = value
        .pointer("/toolUseResult/status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| xml_tag(&raw, "status"))
        .unwrap_or_else(|| "running".into());
    let state = match status.as_str() {
        "completed" => SessionState::Done,
        "failed" | "errored" => SessionState::Errored,
        "running" => SessionState::Running,
        _ => return None,
    };
    let title = xml_tag(&raw, "summary");
    Some((id, state, title))
}

fn normalize_claude_agent_id(id: &str) -> String {
    id.trim()
        .strip_prefix("agent-")
        .unwrap_or(id.trim())
        .to_string()
}

fn claude_subagent_id_from_path(path: &Path) -> Option<String> {
    if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    stem.starts_with("agent-")
        .then(|| normalize_claude_agent_id(stem))
}

fn agent_id_from_text(raw: &str) -> Option<String> {
    let marker = "agentId: ";
    let start = raw.find(marker)? + marker.len();
    let id: String = raw[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    (!id.is_empty()).then_some(id)
}

fn xml_tag(raw: &str, tag: &str) -> Option<String> {
    let start_tag = format!("<{tag}>");
    let end_tag = format!("</{tag}>");
    let start = raw.find(&start_tag)? + start_tag.len();
    let end = raw[start..].find(&end_tag)? + start;
    Some(raw[start..end].replace("\\n", " ").trim().to_string())
}

fn short_native_id(id: &str) -> &str {
    id.get(..id.len().min(8)).unwrap_or(id)
}

/// Seed the headless run queue from the session's initial prompt.
///
/// A non-empty seed prompt becomes the first queued turn, so the run loop
/// pops and runs it immediately (emitting `Running`) instead of idling in
/// `AwaitingInput` — i.e. a created session with a prompt *starts its turn*
/// rather than buffering the prompt unsubmitted. A blank or absent prompt
/// seeds nothing, so the session correctly waits for the first `send_input`.
/// See `specs/0046-session-create-initial-prompt-submits.md`.
fn initial_pending(prompt: &Option<String>) -> VecDeque<String> {
    let mut pending = VecDeque::new();
    if let Some(p) = prompt {
        if !p.trim().is_empty() {
            pending.push_back(p.clone());
        }
    }
    pending
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CLAUDE_CMD",
        "CONSTRUCT_CLAUDE_BIN",
        "claude",
    );
    let cwd = PathBuf::from(&params.cwd);
    let model = params.model.clone();
    let extra_args = params.args.clone();
    let env = params.env.clone();

    let mut session_id: Option<String> = None;
    let mut pending: VecDeque<String> = initial_pending(&params.prompt);

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

        // Build the per-turn child command args.
        let mut child_args: Vec<String> = command_override.args.clone();
        child_args.push("-p".into());
        child_args.push("--input-format".into());
        child_args.push("stream-json".into());
        child_args.push("--output-format".into());
        child_args.push("stream-json".into());
        child_args.push("--verbose".into());
        if let Some(cfg) = construct_protocol::adapter::maybe_inject_mcp_config(&agentd_session_id) {
            child_args.push("--mcp-config".into());
            child_args.push(cfg.to_string_lossy().to_string());
        }
        if let Some(p) = adapter_settings_path() {
            child_args.push("--settings".into());
            child_args.push(p.to_string_lossy().to_string());
        }
        child_args.extend(
            construct_protocol::adapter::policy::AutoApprovePolicy::from_env()
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
        command.env("CONSTRUCT_SESSION_ID", &agentd_session_id);

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

        // Always adopt the latest native id. A turn can report a new id (e.g.
        // after the interactive equivalent of a context reset); subsequent
        // turns must --resume that id, not the first one we ever saw.
        if let Some(sid) = captured_sid.lock().unwrap().clone() {
            if session_id.as_ref() != Some(&sid) {
                if let Ok(dir) = std::env::var("CONSTRUCT_SESSION_DATA_DIR") {
                    let p = PathBuf::from(dir).join("claude_session_id.txt");
                    let _ = std::fs::write(p, &sid);
                }
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
                        // Keep the most recently observed id (not only the first).
                        *g = Some(sid.to_string());
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

fn emit_event_from_json(emit: &EventEmitter, v: Value) {
    let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "assistant" | "user" | "result" => {
            for event in claude_events_from_json(&v) {
                emit.emit(event);
            }
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

fn claude_events_from_json(v: &Value) -> Vec<SessionEvent> {
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "assistant" => {
            let mut out = Vec::new();
            let text = extract_message_text(v.get("message"));
            if !text.is_empty() {
                out.push(SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text,
                });
            }
            out.extend(tool_uses_from_message(v.get("message")));
            out
        }
        "user" => {
            // The CLI echoes tool_result blocks here. The actual user text is
            // already in the transcript (daemon emits it when sending input).
            tool_results_from_message(v.get("message"))
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
            let tcached = v
                .get("usage")
                .and_then(|u| u.get("cache_read_input_tokens"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            if usd > 0.0 || tin > 0 || tout > 0 {
                vec![SessionEvent::Cost {
                    usd,
                    tokens_in: tin,
                    tokens_out: tout,
                    tokens_cached: tcached,
                }]
            } else {
                Vec::new()
            }
            // The `result` text duplicates the assistant's final message; skip it.
        }
        _ => Vec::new(),
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

fn tool_uses_from_message(msg: Option<&Value>) -> Vec<SessionEvent> {
    let Some(arr) = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for block in arr {
        if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
            let name = block
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            let call_id = block.get("id").and_then(|v| v.as_str()).map(str::to_string);
            out.push(SessionEvent::ToolUse {
                tool: name,
                args: input,
                call_id,
            });
        }
    }
    out
}

fn tool_results_from_message(msg: Option<&Value>) -> Vec<SessionEvent> {
    let Some(arr) = msg
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
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
            // No tool name is available in a tool_result block; `tool` keeps the
            // tool_use_id and `call_id` carries the explicit correlation key.
            let call_id = Some(tool.clone());
            out.push(SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_subagent_updates_parse_launch_and_completion() {
        let launch = serde_json::json!({
            "type": "user",
            "message": {"content": "Async agent launched successfully.\nagentId: abc123"}
        });
        assert_eq!(
            claude_native_subagent_update(&launch),
            Some(("abc123".into(), SessionState::Running, None))
        );

        let complete = serde_json::json!({
            "type": "queue-operation",
            "content": "<task-notification><task-id>abc123</task-id><status>completed</status><summary>Inspect parser</summary></task-notification>"
        });
        assert_eq!(
            claude_native_subagent_update(&complete),
            Some((
                "abc123".into(),
                SessionState::Done,
                Some("Inspect parser".into())
            ))
        );

        let foreground_complete = serde_json::json!({
            "type": "user",
            "toolUseResult": {"status": "completed", "agentId": "agent-def456"}
        });
        assert_eq!(
            claude_native_subagent_update(&foreground_complete),
            Some(("def456".into(), SessionState::Done, None))
        );
        assert_eq!(normalize_claude_agent_id("abc123"), "abc123");
        assert_eq!(normalize_claude_agent_id("agent-abc123"), "abc123");
        assert_eq!(
            claude_subagent_id_from_path(Path::new("agent-abc123.jsonl")),
            Some("abc123".into())
        );
        assert_eq!(
            claude_subagent_id_from_path(Path::new("agent-abc123.meta.json")),
            None
        );

        let prefixed_complete = serde_json::json!({
            "content": "<task-notification><task-id>agent-abc123</task-id><status>completed</status></task-notification>"
        });
        assert_eq!(
            claude_native_subagent_update(&prefixed_complete).map(|(id, _, _)| id),
            Some("abc123".into())
        );
    }

    #[test]
    fn claude_project_slug_matches_project_dir_encoding() {
        assert_eq!(
            claude_project_slug(Path::new("/Users/moon/agentd/.claude/worktrees/test")),
            "-Users-moon-agentd--claude-worktrees-test"
        );
    }

    #[test]
    fn read_updated_session_id_detects_clear_style_rewrite() {
        let dir =
            std::env::temp_dir().join(format!("construct-claude-sid-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let sid_file = dir.join("claude_session_id.txt");
        std::fs::write(&sid_file, "old-id").expect("write old");
        assert_eq!(
            read_updated_session_id(Some(&sid_file), "old-id"),
            None,
            "same id is not an update"
        );
        std::fs::write(&sid_file, "new-id-after-clear").expect("write new");
        assert_eq!(
            read_updated_session_id(Some(&sid_file), "old-id").as_deref(),
            Some("new-id-after-clear")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_id_capture_script_extracts_hook_json() {
        let dir =
            std::env::temp_dir().join(format!("construct-claude-capture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let sid_file = dir.join("claude_session_id.txt");
        let script = dir.join("capture.sh");
        write_session_id_capture_script(&script, &sid_file).expect("write script");

        let hook_json = r#"{"session_id":"a1b2c3d4-e5f6-7890-abcd-ef1234567890","cwd":"/tmp","source":"clear"}"#;
        let status = std::process::Command::new("sh")
            .arg(&script)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                if let Some(mut stdin) = child.stdin.take() {
                    stdin.write_all(hook_json.as_bytes())?;
                }
                child.wait()
            })
            .expect("run capture script");
        assert!(status.success(), "capture script exit: {status}");
        assert_eq!(
            std::fs::read_to_string(&sid_file).expect("read sid").trim(),
            "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn initial_prompt_seeds_run_queue_so_first_turn_runs() {
        // A created session with a non-empty prompt must run it on the first
        // loop iteration (submitted), not sit idle in AwaitingInput waiting
        // for input (buffered) — this is the headless side of the
        // create-starts-the-turn contract.
        let q = initial_pending(&Some("do the task".to_string()));
        assert_eq!(q.len(), 1);
        assert_eq!(q.front().map(String::as_str), Some("do the task"));

        // A blank or absent prompt seeds nothing, so the session correctly
        // idles in AwaitingInput until the first send_input.
        assert!(initial_pending(&None).is_empty());
        assert!(initial_pending(&Some("   ".to_string())).is_empty());
    }

    #[test]
    fn short_initial_prompt_stays_inline() {
        let prompt = "continue from here";
        assert_eq!(interactive_initial_prompt_arg(prompt), prompt);
    }

    #[test]
    fn oversize_initial_prompt_spills_to_session_file() {
        let dir = std::env::temp_dir().join(format!(
            "construct-claude-prompt-spill-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        std::env::set_var("CONSTRUCT_SESSION_DATA_DIR", &dir);

        let prompt = format!(
            "objective\n{}",
            "x".repeat(MAX_CLAUDE_INITIAL_PROMPT_ARG_BYTES + 1)
        );
        let arg = interactive_initial_prompt_arg(&prompt);

        assert!(
            arg.len() < MAX_CLAUDE_INITIAL_PROMPT_ARG_BYTES,
            "launch arg should stay below Claude metadata limit: {}",
            arg.len()
        );
        assert!(arg.contains("claude-initial-prompt.md"));
        let path = dir.join("claude-initial-prompt.md");
        assert_eq!(
            std::fs::read_to_string(&path).expect("spilled prompt"),
            prompt
        );

        std::env::remove_var("CONSTRUCT_SESSION_DATA_DIR");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn assistant_transcript_record_emits_message_and_tool_use() {
        let v = serde_json::json!({
            "type": "assistant",
            "message": {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "I will inspect it." },
                    {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "Bash",
                        "input": { "command": "cargo test" }
                    }
                ]
            }
        });

        let events = claude_events_from_json(&v);
        assert_eq!(events.len(), 2);
        match &events[0] {
            SessionEvent::Message { role, text } => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "I will inspect it.");
            }
            other => panic!("unexpected message event: {other:?}"),
        }
        match &events[1] {
            SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            } => {
                assert_eq!(tool, "Bash");
                assert_eq!(args["command"], "cargo test");
                assert_eq!(call_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("unexpected tool-use event: {other:?}"),
        }
    }

    #[test]
    fn user_tool_result_record_emits_tool_result() {
        let v = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_1",
                        "content": "finished",
                        "is_error": false
                    }
                ]
            }
        });

        match claude_events_from_json(&v).as_slice() {
            [SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            }] => {
                assert_eq!(tool, "toolu_1");
                assert!(*ok);
                assert_eq!(output, "finished");
                assert_eq!(call_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("unexpected tool-result events: {other:?}"),
        }
    }

    #[test]
    fn result_record_emits_cost_without_duplicate_message() {
        let v = serde_json::json!({
            "type": "result",
            "result": "final text that should not become another message",
            "total_cost_usd": 0.25,
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20
            }
        });

        match claude_events_from_json(&v).as_slice() {
            [SessionEvent::Cost {
                usd,
                tokens_in,
                tokens_out,
                ..
            }] => {
                assert_eq!(*usd, 0.25);
                assert_eq!(*tokens_in, 10);
                assert_eq!(*tokens_out, 20);
            }
            other => panic!("unexpected cost events: {other:?}"),
        }
    }
}
