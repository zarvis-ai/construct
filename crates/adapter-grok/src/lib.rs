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

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use construct_adapter_common::{
    drive_turn, grok_transcript_path, next_native_seq, spawn_stderr_log, TurnOutcome,
};
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

fn command_override() -> construct_protocol::adapter::CommandOverride {
    construct_protocol::adapter::resolve_command_override(
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
    find_session_id_excluding_in(&sessions_dir, excluded)
}

fn find_session_id_excluding_in(
    sessions_dir: &Path,
    excluded: &HashSet<String>,
) -> Option<String> {
    if !sessions_dir.exists() {
        return None;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if name.len() == 36
                        && !excluded.contains(&name)
                        && !grok_session_is_fork(&sessions_dir.join(&name))
                    {
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

/// Native Grok session ids that already exist for `cwd`.
///
/// A resumed construct session is already bound to one of these ids. All of
/// the others are historical or belong to sibling construct sessions, so they
/// must never be considered evidence that this session was cleared. A real
/// `/clear` creates a fresh UUID after the adapter starts.
fn existing_session_ids(cwd: &Path) -> HashSet<String> {
    let Some(sessions_dir) =
        grok_home().map(|home| home.join("sessions").join(url_encode_path(cwd)))
    else {
        return HashSet::new();
    };
    existing_session_ids_in(&sessions_dir)
}

fn existing_session_ids_in(sessions_dir: &Path) -> HashSet<String> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return HashSet::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().map(|ty| ty.is_dir()).unwrap_or(false))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.len() == 36)
        .collect()
}

/// Whether a grok session dir was created by `--fork-session`
/// (`summary.json` stamps `session_kind: "fork"` plus the source's
/// `parent_session_id`). Forks are named up front and bound directly by
/// their own watcher — newest-dir DISCOVERY must never rebind another
/// session (typically the fork's parent, sharing this cwd) onto them.
fn grok_session_is_fork(session_dir: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(session_dir.join("summary.json")) else {
        return false;
    };
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("session_kind")
                .and_then(|k| k.as_str())
                .map(|k| k == "fork")
        })
        .unwrap_or(false)
}

/// Other construct sessions' grok native ids, for sessions that share this
/// one's `cwd` (read from each sibling's own `grok_session_id.txt`, next to
/// its `meta.json`, both written by the daemon under
/// `<data_dir>/sessions/<construct_id>/`).
///
/// Grok organizes its own on-disk sessions per-cwd
/// (`~/.grok/sessions/<cwd>/<uuid>/`), not per-construct-session, so two
/// construct sessions started in the same `cwd` share one folder there.
/// `find_session_id_excluding`'s newest-mtime discovery — meant to notice a
/// harness-native `/clear` creating a fresh dir — can't otherwise tell that
/// apart from a sibling's own routine `summary.json` rewrite (grok restamps
/// it, atomically, on every turn, which touches the containing dir's mtime).
/// Feeding every live sibling's current native id into the `excluded` set
/// closes that: a sibling merely taking a turn can no longer look like
/// *this* session's own conversation being reset and forked-and-archived.
fn sibling_native_ids(own_cwd: &Path) -> HashSet<String> {
    let mut ids = HashSet::new();
    let Some(own_dir) = session_data_dir() else {
        return ids;
    };
    let Some(sessions_root) = own_dir.parent() else {
        return ids;
    };
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return ids;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own_dir {
            continue;
        }
        let Ok(meta_text) = std::fs::read_to_string(path.join("meta.json")) else {
            continue;
        };
        let Ok(meta) = serde_json::from_str::<Value>(&meta_text) else {
            continue;
        };
        if meta.get("harness").and_then(|h| h.as_str()) != Some("grok") {
            continue;
        }
        let same_cwd = meta
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(|c| Path::new(c) == own_cwd)
            .unwrap_or(false);
        if !same_cwd {
            continue;
        }
        if let Ok(native_id) = std::fs::read_to_string(path.join("grok_session_id.txt")) {
            let native_id = native_id.trim();
            if !native_id.is_empty() {
                ids.insert(native_id.to_string());
            }
        }
    }
    ids
}

fn grok_session_dir(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(
        grok_home()?
            .join("sessions")
            .join(url_encode_path(cwd))
            .join(session_id),
    )
}

fn grok_updates_path(cwd: &Path, session_id: &str) -> Option<PathBuf> {
    Some(grok_session_dir(cwd, session_id)?.join("updates.jsonl"))
}

/// Context gauge from a session dir's `signals.json` (spec 0104): grok
/// maintains `contextTokensUsed` / `contextWindowTokens` there — the only
/// per-session token figures its files expose (its chat/updates streams
/// carry no billing usage split). Returns `(used, window)` when the file
/// parses and reports non-zero usage.
fn grok_context_usage(cwd: &Path, session_id: &str) -> Option<(u64, Option<u64>)> {
    let path = grok_session_dir(cwd, session_id)?.join("signals.json");
    let text = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let used = v.get("contextTokensUsed").and_then(Value::as_u64)?;
    if used == 0 {
        return None;
    }
    let window = v
        .get("contextWindowTokens")
        .and_then(Value::as_u64)
        .filter(|w| *w > 0);
    Some((used, window))
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

fn emit_new_grok_transcript_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
) {
    for value in read_new_grok_jsonl_lines(path, next_line, emit) {
        emit_event_from_json(emit, value, last_model, last_effort);
    }
}

/// The root session's active model, if `v` is an assistant turn carrying a
/// `model_id` that differs from what we last saw — grok stamps `model_id` on
/// every assistant line in `chat_history.jsonl`, so this establishes the
/// initial model and, in principle, catches a mid-session `/model` switch.
///
/// Verified against a real session (2026-07-12): running `/model` and
/// picking a different one did not change `model_id` on the next turn, nor
/// `summary.json`'s `current_model_id` — and grok's own status bar kept
/// showing the old model too, so the switch never actually took effect at
/// the grok CLI level. That's an external grok issue, not something this
/// diff-against-`last_model` logic can work around: there's nothing on disk
/// to observe when grok itself never persists the change. Initial-model
/// capture is unaffected and works reliably.
fn grok_model_change(v: &Value, last_model: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let model = v.get("model_id").and_then(|m| m.as_str())?;
    (last_model.as_deref() != Some(model)).then(|| model.to_string())
}

/// Same signal as `grok_model_change`, for `reasoning_effort` (e.g.
/// `"high"`/`"medium"`/`"low"`) on the same assistant line. Carries the same
/// caveat: scanning 32 real sessions on this machine found 0 with more than
/// one distinct value, the identical frozen-per-session pattern already
/// confirmed for `model_id` — best-effort initial capture, unreliable for a
/// live mid-session change.
fn grok_effort_change(v: &Value, last_effort: &Option<String>) -> Option<String> {
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return None;
    }
    let effort = v.get("reasoning_effort").and_then(|e| e.as_str())?;
    (last_effort.as_deref() != Some(effort)).then(|| effort.to_string())
}

fn emit_event_from_json(
    emit: &EventEmitter,
    v: Value,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
) {
    if let Some(model) = grok_model_change(&v, last_model) {
        *last_model = Some(model.clone());
        emit.emit(SessionEvent::ModelChanged { model });
    }
    if let Some(effort) = grok_effort_change(&v, last_effort) {
        *last_effort = Some(effort.clone());
        emit.emit(SessionEvent::EffortChanged { effort });
    }
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
    let policy = construct_protocol::adapter::policy::AutoApprovePolicy::from_env();
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
    never_rebind_onto: HashSet<String>,
    initial_model: Option<String>,
) {
    if grok_home().is_none() {
        emit.log("grok: no GROK_HOME or HOME — cannot watch native transcript");
        return;
    }
    tokio::spawn(async move {
        let mut current_id = initial_id;
        let mut path: Option<PathBuf> = current_id
            .as_ref()
            .and_then(|id| grok_transcript_path(&cwd, id, &HashMap::new()));
        let mut updates_path: Option<PathBuf> = current_id
            .as_ref()
            .and_then(|id| grok_updates_path(&cwd, id));
        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
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
        // Re-scanning every sibling's meta.json on every 500ms tick is
        // needless I/O churn — sibling composition and cwd are static for a
        // session's lifetime, only their native ids rotate occasionally.
        // Refreshing every 5s (10 ticks) still closes the false-rebind
        // window far faster than that window can recur in practice.
        let mut ticks_since_sibling_refresh: u32 = 0;
        let mut sibling_ids: HashSet<String> = sibling_native_ids(&cwd);
        // Last context gauge reported (spec 0104) — poll `signals.json`
        // each tick but only emit when the numbers actually move.
        let mut last_context: Option<(u64, Option<u64>)> = None;
        loop {
            tick.tick().await;
            ticks_since_sibling_refresh += 1;
            if ticks_since_sibling_refresh >= 10 {
                ticks_since_sibling_refresh = 0;
                sibling_ids = sibling_native_ids(&cwd);
            }
            if let Some(path) = path.as_deref().filter(|path| path.exists()) {
                emit_new_grok_transcript_lines(
                    path,
                    &mut next_line,
                    &emit,
                    &mut last_model,
                    &mut last_effort,
                );
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
            if let Some(root_id) = current_id.as_deref() {
                let observed = grok_context_usage(&cwd, root_id);
                if let Some((used, window)) = observed {
                    if last_context != Some((used, window)) {
                        last_context = Some((used, window));
                        emit.emit(SessionEvent::ContextUsage {
                            used_tokens: used,
                            window_tokens: window,
                        });
                    }
                }
            }
            for (id, child) in &mut children {
                let Some(child_path) = grok_transcript_path(&cwd, id, &HashMap::new()) else {
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

            // Prefer the newest non-child, non-sibling session dir under this
            // cwd. First spawn discovers the id; after /clear a fresher root
            // dir appears and we rebind both transcript streams.
            let mut excluded: HashSet<String> = children.keys().cloned().collect();
            excluded.extend(never_rebind_onto.iter().cloned());
            excluded.extend(sibling_ids.iter().cloned());
            if let Some(id) = find_session_id_excluding(&cwd, &excluded) {
                if current_id.as_ref() != Some(&id) {
                    if let (Some(new_path), Some(new_updates_path)) = (
                        grok_transcript_path(&cwd, &id, &HashMap::new()),
                        grok_updates_path(&cwd, &id),
                    ) {
                        if let Some(prior) = current_id.as_ref() {
                            emit.log(format!(
                                "grok: native session id changed {:?} -> {id}; \
                                 rebinding transcript watcher",
                                current_id
                            ));
                            emit.emit(SessionEvent::NativeIdChanged {
                                prior_native_id: prior.clone(),
                                new_native_id: id.clone(),
                            });
                        }
                        write_conv_id(&id);
                        current_id = Some(id);
                        path = Some(new_path);
                        updates_path = Some(new_updates_path);
                        next_line = 0;
                        next_update_line = 0;
                        last_context = None;
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
    let mut grok_session_id = if resuming { read_conv_id() } else { None };
    let mut fork_parent_id: Option<String> = None;

    if let Some(sid) = &grok_session_id {
        args.push("-r".into());
        args.push(sid.clone());
    } else if !resuming {
        if let Some(parent) = std::env::var("CONSTRUCT_GROK_FORK_FROM")
            .ok()
            .filter(|s| !s.is_empty())
        {
            // Same-harness fork: resume the parent's native session AS A
            // NEW one (`--fork-session`), named up front (`--session-id`)
            // so this session's own id file is correct immediately (the
            // daemon read the parent's id — spec 0031/0078).
            let new_id = uuid::Uuid::new_v4().to_string();
            args.push("-r".into());
            args.push(parent.clone());
            args.push("--fork-session".into());
            args.push("--session-id".into());
            args.push(new_id.clone());
            write_conv_id(&new_id);
            grok_session_id = Some(new_id);
            fork_parent_id = Some(parent);
        }
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
    let attached_to_existing = grok_session_id.is_some();
    let mut never_rebind_onto: HashSet<String> = fork_parent_id.into_iter().collect();
    if attached_to_existing {
        // Snapshot synchronously, before spawning the PTY. On bulk daemon
        // restart every Grok process shares the cwd-level native-session
        // directory, and resume itself can make a sibling directory newest.
        // Persisted sibling metadata is a useful live safeguard, but the
        // stronger restart invariant is that no directory which predates
        // this resumed adapter can represent its future `/clear`.
        never_rebind_onto.extend(existing_session_ids(&cwd));
        if let Some(own_id) = grok_session_id.as_ref() {
            never_rebind_onto.remove(own_id);
        }
    }
    spawn_interactive_transcript_watcher(
        grok_session_id,
        cwd,
        ctx.emit.clone(),
        attached_to_existing,
        never_rebind_onto,
        params.model.clone(),
    );

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
                    message: construct_protocol::adapter::missing_bin_hint(
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
    #[test]
    fn newest_dir_discovery_skips_fork_sessions() {
        // A fork (`--fork-session`) creates a NEW session dir in the same
        // cwd, stamped `session_kind: "fork"` in its summary. The
        // newest-dir discovery another session's watcher runs (typically
        // the fork's parent) must never rebind onto it — forks are named
        // up front and bound directly by their own watcher.
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-fork-skip-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = std::path::Path::new("/tmp/proj");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        let parent = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        let fork = "019e32bb-014a-7ff0-9a3f-7ae773961a99";
        std::fs::create_dir_all(sessions.join(parent)).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(fork)).unwrap();
        std::fs::write(
            sessions.join(fork).join("summary.json"),
            format!(
                "{{\"info\":{{\"id\":\"{fork}\"}},\
                 \"session_kind\":\"fork\",\
                 \"parent_session_id\":\"{parent}\"}}"
            ),
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        let found = find_session_id_excluding(cwd, &HashSet::new());
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        assert_eq!(
            found.as_deref(),
            Some(parent),
            "the newer fork dir must be invisible to discovery"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    /// The other direction of the same race: a FORK's own watcher, in the
    /// window before grok has created the fork's own session dir, must
    /// never rebind onto its PARENT's dir just because the parent's is
    /// the only (and therefore "newest") one present. Passing the parent's
    /// id in the exclusion set — as `run_interactive` now does whenever
    /// `CONSTRUCT_GROK_FORK_FROM` is set — closes this regardless of
    /// timing, rather than relying on the fork's own dir winning a race.
    #[test]
    fn newest_dir_discovery_excludes_the_forks_own_parent() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-fork-parent-exclude-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = std::path::Path::new("/tmp/proj");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        let parent = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        // Only the parent's dir exists — simulating the window right after
        // the fork process is spawned but before grok has created its own
        // session directory.
        std::fs::create_dir_all(sessions.join(parent)).unwrap();

        std::env::set_var("CONSTRUCT_GROK_HOME", &home);
        let mut excluded = HashSet::new();
        excluded.insert(parent.to_string());
        let found = find_session_id_excluding(cwd, &excluded);
        std::env::remove_var("CONSTRUCT_GROK_HOME");
        assert_eq!(
            found, None,
            "with the parent excluded, discovery must find nothing rather \
             than silently rebinding the fork's persisted id onto its \
             parent's conversation"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

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

    #[test]
    fn resumed_session_excludes_every_preexisting_native_sibling() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-resume-baseline-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let cwd = Path::new("/tmp/agentd-grok-resume-baseline-test");
        let sessions = home.join("sessions").join(url_encode_path(cwd));
        std::fs::create_dir_all(&sessions).unwrap();

        let own_id = "aaaaaaaa-bbbb-cccc-dddd-000000000001";
        let sibling_id = "aaaaaaaa-bbbb-cccc-dddd-000000000002";
        let cleared_id = "aaaaaaaa-bbbb-cccc-dddd-000000000003";
        std::fs::create_dir_all(sessions.join(own_id)).unwrap();
        std::fs::create_dir_all(sessions.join(sibling_id)).unwrap();

        let mut baseline = existing_session_ids_in(&sessions);
        baseline.remove(own_id);
        assert_eq!(
            find_session_id_excluding_in(&sessions, &baseline).as_deref(),
            Some(own_id)
        );

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::create_dir_all(sessions.join(cleared_id)).unwrap();
        assert_eq!(
            find_session_id_excluding_in(&sessions, &baseline).as_deref(),
            Some(cleared_id)
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sibling_native_ids_reads_only_grok_siblings_in_the_same_cwd() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-siblings-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions_root = home.join("sessions");
        let own_dir = sessions_root.join("sOWN");
        std::fs::create_dir_all(&own_dir).unwrap();

        let cwd = Path::new("/tmp/agentd-shared-cwd");

        // A real sibling: same cwd, same harness — must be picked up.
        let sibling_dir = sessions_root.join("sSIBLING");
        std::fs::create_dir_all(&sibling_dir).unwrap();
        std::fs::write(
            sibling_dir.join("meta.json"),
            format!(r#"{{"harness":"grok","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();
        std::fs::write(sibling_dir.join("grok_session_id.txt"), "sibling-native-id").unwrap();

        // A different-cwd grok session — must be excluded from the result.
        let other_cwd_dir = sessions_root.join("sOTHERCWD");
        std::fs::create_dir_all(&other_cwd_dir).unwrap();
        std::fs::write(
            other_cwd_dir.join("meta.json"),
            r#"{"harness":"grok","cwd":"/tmp/somewhere-else"}"#,
        )
        .unwrap();
        std::fs::write(
            other_cwd_dir.join("grok_session_id.txt"),
            "other-cwd-native-id",
        )
        .unwrap();

        // A same-cwd, different-harness session — must be excluded.
        let other_harness_dir = sessions_root.join("sOTHERHARNESS");
        std::fs::create_dir_all(&other_harness_dir).unwrap();
        std::fs::write(
            other_harness_dir.join("meta.json"),
            format!(r#"{{"harness":"claude","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();
        std::fs::write(
            other_harness_dir.join("grok_session_id.txt"),
            "wrong-harness-id",
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_SESSION_DATA_DIR", &own_dir);
        let ids = sibling_native_ids(cwd);
        std::env::remove_var("CONSTRUCT_SESSION_DATA_DIR");

        assert_eq!(ids, HashSet::from(["sibling-native-id".to_string()]));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn sibling_native_ids_skips_dirs_missing_meta_or_native_id() {
        let home = std::env::temp_dir().join(format!(
            "agentd-grok-siblings-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let sessions_root = home.join("sessions");
        let own_dir = sessions_root.join("sOWN");
        std::fs::create_dir_all(&own_dir).unwrap();

        let cwd = Path::new("/tmp/agentd-shared-cwd");

        // No meta.json at all (e.g. a session mid-creation) — skipped, not a panic.
        let no_meta_dir = sessions_root.join("sNOMETA");
        std::fs::create_dir_all(&no_meta_dir).unwrap();

        // meta.json present but no grok_session_id.txt yet — skipped.
        let no_native_id_dir = sessions_root.join("sNONATIVEID");
        std::fs::create_dir_all(&no_native_id_dir).unwrap();
        std::fs::write(
            no_native_id_dir.join("meta.json"),
            format!(r#"{{"harness":"grok","cwd":"{}"}}"#, cwd.display()),
        )
        .unwrap();

        std::env::set_var("CONSTRUCT_SESSION_DATA_DIR", &own_dir);
        let ids = sibling_native_ids(cwd);
        std::env::remove_var("CONSTRUCT_SESSION_DATA_DIR");

        assert!(ids.is_empty());

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn model_change_ignored_for_non_assistant_lines() {
        let v: Value = serde_json::json!({"type": "user", "model_id": "grok-4.5"});
        assert_eq!(grok_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_ignored_when_field_absent() {
        let v: Value = serde_json::json!({"type": "assistant", "content": "hi"});
        assert_eq!(grok_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_fires_on_first_observation() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5"});
        assert_eq!(grok_model_change(&v, &None).as_deref(), Some("grok-4.5"));
    }

    #[test]
    fn model_change_silent_when_unchanged() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5"});
        assert_eq!(
            grok_model_change(&v, &Some("grok-4.5".to_string())),
            None
        );
    }

    #[test]
    fn model_change_fires_on_switch() {
        let v: Value = serde_json::json!({"type": "assistant", "model_id": "grok-4.5-fast"});
        assert_eq!(
            grok_model_change(&v, &Some("grok-4.5".to_string())).as_deref(),
            Some("grok-4.5-fast")
        );
    }

    #[test]
    fn effort_change_ignored_for_non_assistant_lines() {
        let v: Value = serde_json::json!({"type": "user", "reasoning_effort": "high"});
        assert_eq!(grok_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_ignored_when_field_absent() {
        let v: Value = serde_json::json!({"type": "assistant", "content": "hi"});
        assert_eq!(grok_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_fires_on_first_observation() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "high"});
        assert_eq!(grok_effort_change(&v, &None).as_deref(), Some("high"));
    }

    #[test]
    fn effort_change_silent_when_unchanged() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "high"});
        assert_eq!(
            grok_effort_change(&v, &Some("high".to_string())),
            None
        );
    }

    #[test]
    fn effort_change_fires_on_switch() {
        let v: Value = serde_json::json!({"type": "assistant", "reasoning_effort": "low"});
        assert_eq!(
            grok_effort_change(&v, &Some("high".to_string())).as_deref(),
            Some("low")
        );
    }
}
