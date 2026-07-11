//! OpenAI Codex CLI adapter.
//!
//! Two modes:
//!
//! - **interactive (default when a PTY size is provided)** — spawns `codex`
//!   under a PTY, giving the user the real Codex TUI experience.
//!
//! - **headless (opt-in)** — multi-turn structured mode that spawns
//!   `codex exec <prompt>` per turn. Best-effort: if your codex build
//!   supports session resumption, set `CONSTRUCT_CODEX_RESUME_FLAG` to the flag
//!   name (e.g. `--session-id`) and the adapter will pass any captured
//!   `session_id` back in for subsequent turns.
//!
//! Pick mode via `--mode interactive|headless` on `construct new`, or via
//! `CONSTRUCT_CODEX_MODE=interactive|headless`. Honors `CONSTRUCT_CODEX_CMD` for a
//! full command prefix, falling back to `CONSTRUCT_CODEX_BIN` for a binary path.

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
        name: "codex".into(),
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
    if let Ok(m) = std::env::var("CONSTRUCT_CODEX_MODE") {
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
        "CONSTRUCT_CODEX_CMD",
        "CONSTRUCT_CODEX_BIN",
        "codex",
    );
    let mut args = command.args.clone();
    args.extend(params.args.clone());
    // The daemon's auto-approval policy (`CONSTRUCT_AUTO_APPROVE_PATHS`, see
    // `agentd_protocol::adapter::policy`) is set, but the upstream codex CLI
    // does not currently expose a path-scoped allow-list flag, so there's no
    // native translation to apply here. Either upstream gains the knob or we
    // wrap codex's IO to intercept tool calls.
    // Resume support: codex doesn't let the client assign a session id, so
    // we tag each spawn with a unique `originator` (via codex's internal
    // env override) and watch its rollouts dir for one bearing that tag.
    // When we see it, we persist codex's UUID to
    // `<session-dir>/codex_session_id.txt`; on daemon-restart respawn we
    // pass it back as `codex resume <uuid>`. The explicit override
    // `CONSTRUCT_CODEX_RESUME_ID` still wins if set.
    //
    // We deliberately do NOT fall back to `codex resume --last` when no id
    // was captured: `--last` resolves globally across every codex session
    // on the machine, so two agentd codex sessions both falling through
    // would attach to the same upstream codex and from that moment paint
    // identical PTY content. Starting a fresh codex loses one session's
    // conversation but never conflates two of them.
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let sid_file = std::env::var("CONSTRUCT_SESSION_DATA_DIR")
        .ok()
        .map(|d| std::path::PathBuf::from(d).join("codex_session_id.txt"));
    let mut captured_id: Option<String> = None;
    if resuming {
        let explicit = std::env::var("CONSTRUCT_CODEX_RESUME_ID").ok();
        let from_file = sid_file.as_ref().and_then(|p| {
            std::fs::read_to_string(p)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        });
        captured_id = explicit.or(from_file);
        if let Some(id) = captured_id.as_ref() {
            args.insert(0, "resume".into());
            args.insert(1, id.clone());
        } else {
            ctx.emit.log(
                "codex respawn: no captured session id (codex_session_id.txt missing); \
                 starting a fresh codex conversation to avoid `--last` conflating sessions",
            );
        }
    }
    if let Some(m) = params.model.as_ref() {
        args.push("-m".into());
        args.push(m.clone());
    }
    // Auto-inject agentd MCP server via codex's `-c` override (codex has no
    // `--mcp-config` flag — MCP servers live in `[mcp_servers.<name>]`).
    // Opt out with CONSTRUCT_INJECT_MCP=0.
    for a in agentd_protocol::adapter::maybe_inject_codex_mcp_args(&ctx.session_id) {
        args.push(a);
    }
    // Skip the initial prompt only when we're actually resuming an
    // existing codex session; a respawn that fell through to a fresh
    // codex should still pass the original prompt.
    let resuming_existing = resuming && captured_id.is_some();
    if !resuming_existing {
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
    // Tag this codex's rollout with a unique originator we can grep for.
    // Codex stamps `payload.originator` in the rollout's session_meta line
    // from this internal env var (found by string-grep on the binary; not
    // a public flag but stable across recent codex releases). Without the
    // tag we'd have to guess which of several concurrent codex rollouts
    // in the same cwd belongs to which agentd session.
    let originator_tag = format!("agentd:{}", ctx.session_id);
    env.push((
        "CODEX_INTERNAL_ORIGINATOR_OVERRIDE".into(),
        originator_tag.clone(),
    ));
    // Watch the native rollout JSONL for this interactive Codex TUI and
    // mirror its semantic messages/tool events into agentd's transcript.
    // The PTY remains the interactive surface; these events make web chat
    // mode readable without scraping terminal escape sequences.
    if let Some(sid_path) = sid_file.clone() {
        spawn_interactive_transcript_watcher(
            sid_path,
            originator_tag,
            params.env.clone(),
            ctx.emit.clone(),
            resuming_existing,
            captured_id.clone(),
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

/// Watch codex's sessions directory for the rollout file tagged with our
/// originator marker, then persist its UUID to
/// `<session-dir>/codex_session_id.txt` so a future daemon restart can
/// resume the same upstream codex conversation by id.
///
/// Polls for the entire session lifetime (the spawn dies when the
/// adapter process exits). No timeout because codex flushes its rollout
/// lazily — sometimes within a second, sometimes only after the first
/// turn completes minutes later. To keep the work cheap, files that
/// don't bear our originator are remembered and not re-read.
///
/// After the first match we keep scanning: `/clear` / `/new` mint a new
/// codex session id under the same originator tag, and resume/fork must
/// follow the newest matching rollout — not the first one we ever saw.
fn spawn_interactive_transcript_watcher(
    sid_file: PathBuf,
    expected_originator: String,
    session_env: HashMap<String, String>,
    emit: EventEmitter,
    skip_existing: bool,
    expected_uuid: Option<String>,
) {
    let Some(sessions_root) = codex_sessions_root(&session_env) else {
        emit.log("codex: no CODEX_HOME or HOME — cannot watch native transcript");
        return;
    };
    tokio::spawn(async move {
        // Files we've already inspected and determined are not ours —
        // skip them on later ticks so a deep `~/.codex/sessions/` tree
        // stays cheap to poll.
        let mut not_ours: HashSet<String> = HashSet::new();
        let mut selected: Option<(String, PathBuf)> = None;
        let mut selected_mtime: Option<std::time::SystemTime> = None;
        let mut next_line: usize = 0;
        let mut known: HashMap<String, (PathBuf, SessionMeta)> = HashMap::new();
        let mut child_lines: HashMap<String, usize> = HashMap::new();
        let mut child_seq: HashMap<String, u64> = HashMap::new();
        let mut child_states: HashMap<String, SessionState> = HashMap::new();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;

            for (name, path) in list_rollouts(&sessions_root) {
                if known.contains_key(&name) {
                    continue;
                }
                if let Some(meta) = read_session_meta(&path) {
                    known.insert(name, (path, meta));
                }
            }

            // Prefer the newest matching rollout. On first attach this
            // picks the live session; after /clear|/new a fresher file
            // appears with the same originator and we rebind.
            if let Some((name, path, uuid, mtime)) = find_best_matching_rollout(
                &sessions_root,
                &expected_originator,
                expected_uuid.as_deref(),
                &mut not_ours,
            ) {
                let is_new = selected
                    .as_ref()
                    .map(|(cur, _)| cur != &name)
                    .unwrap_or(true);
                let is_newer = match (selected_mtime, mtime) {
                    (Some(prev), Some(cur)) => cur > prev,
                    (None, _) => true,
                    (Some(_), None) => is_new,
                };
                if is_new && (selected.is_none() || is_newer) {
                    let first_select = selected.is_none();
                    let should_write = std::fs::read_to_string(&sid_file)
                        .ok()
                        .map(|s| s.trim() != uuid)
                        .unwrap_or(true);
                    if should_write {
                        if let Err(e) = std::fs::write(&sid_file, &uuid) {
                            emit.log(format!(
                                "codex: failed to write {}: {e}",
                                sid_file.display()
                            ));
                        } else {
                            emit.log(format!(
                                "codex: captured session id {uuid} (from {})",
                                path.display()
                            ));
                        }
                    }
                    // Skip existing lines only on the initial attach of a
                    // resumed session. Mid-session rebinds (after /clear)
                    // start at line 0 so chat mode sees the new conversation.
                    next_line = if first_select && skip_existing {
                        count_jsonl_lines(&path)
                    } else {
                        0
                    };
                    if !first_select {
                        emit.log(format!(
                            "codex: native session id rebinding to {uuid} (from {})",
                            path.display()
                        ));
                    }
                    selected = Some((name.clone(), path.clone()));
                    selected_mtime = mtime;
                }
            }

            let Some((_, path)) = selected.as_ref() else {
                continue;
            };
            emit_new_codex_rollout_lines(path, &mut next_line, &emit);

            let Some(root_id) = selected
                .as_ref()
                .and_then(|(name, _)| known.get(name))
                .and_then(|(_, meta)| meta.id.clone())
            else {
                continue;
            };
            let mut related = HashSet::from([root_id.clone()]);
            loop {
                let before = related.len();
                for (_, meta) in known.values() {
                    if meta
                        .parent_thread_id
                        .as_ref()
                        .is_some_and(|parent| related.contains(parent))
                    {
                        if let Some(id) = meta.id.as_ref() {
                            related.insert(id.clone());
                        }
                    }
                }
                if related.len() == before {
                    break;
                }
            }
            for (child_path, meta) in known.values() {
                let (Some(child_id), Some(parent_id)) =
                    (meta.id.as_ref(), meta.parent_thread_id.as_ref())
                else {
                    continue;
                };
                if !related.contains(child_id) || !related.contains(parent_id) {
                    continue;
                }
                let first_seen = !child_lines.contains_key(child_id);
                // Child files are ALWAYS read from the top — pre-existing
                // history backfills into the mirror instead of being skipped
                // on resume/restart. Every emission derived from the file
                // carries a deterministic per-child ordinal; the daemon
                // drops ordinals below the mirror's high-water mark, so
                // re-scans never duplicate.
                let line = child_lines.entry(child_id.clone()).or_insert(0);
                let ord = child_seq.entry(child_id.clone()).or_insert(0);
                let mut state = child_states
                    .get(child_id)
                    .copied()
                    .unwrap_or(SessionState::Running);
                if first_seen {
                    emit.emit(SessionEvent::NativeSubagent {
                        id: child_id.clone(),
                        parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                        title: Some(format!("Codex subagent {}", short_codex_id(child_id))),
                        state,
                        event: None,
                        seq: Some(next_native_seq(ord)),
                    });
                }
                for value in read_new_codex_values(child_path, line, &emit) {
                    if let Some(next_state) = codex_native_state(&value) {
                        state = next_state;
                        child_states.insert(child_id.clone(), state);
                    }
                    let events = codex_rollout_events(&value);
                    if events.is_empty() && codex_native_state(&value).is_some() {
                        emit.emit(SessionEvent::NativeSubagent {
                            id: child_id.clone(),
                            parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                            title: None,
                            state,
                            event: None,
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                    for event in events {
                        let title = match &event {
                            SessionEvent::Message {
                                role: MessageRole::User,
                                text,
                            } => Some(short_title(text)),
                            _ => None,
                        };
                        emit.emit(SessionEvent::NativeSubagent {
                            id: child_id.clone(),
                            parent_id: (parent_id != &root_id).then(|| parent_id.clone()),
                            title,
                            state,
                            event: Some(Box::new(event)),
                            seq: Some(next_native_seq(ord)),
                        });
                    }
                }
            }
        }
    });
}

/// Scan rollouts under `sessions_root` and return the best match for this
/// construct session: originator tag match, or (on resume) an exact uuid
/// match. Among matches, the newest mtime wins so /clear's fresh rollout
/// supersedes the pre-clear one.
fn find_best_matching_rollout(
    sessions_root: &Path,
    expected_originator: &str,
    expected_uuid: Option<&str>,
    not_ours: &mut HashSet<String>,
) -> Option<(String, PathBuf, String, Option<std::time::SystemTime>)> {
    let mut best: Option<(String, PathBuf, String, Option<std::time::SystemTime>)> = None;
    for (name, path) in list_rollouts(sessions_root) {
        if not_ours.contains(&name) {
            continue;
        }
        let Some(meta) = read_session_meta(&path) else {
            // File exists but session_meta isn't readable yet.
            // Don't blacklist — codex may still be writing.
            continue;
        };
        let uuid = meta.id.clone().or_else(|| uuid_from_rollout_name(&name));
        let originator_matches = meta.originator.as_deref() == Some(expected_originator)
            && meta.parent_thread_id.is_none();
        let uuid_matches = expected_uuid.is_some_and(|want| uuid.as_deref() == Some(want));
        if !originator_matches && !uuid_matches {
            not_ours.insert(name);
            continue;
        }
        let Some(uuid) = uuid else {
            not_ours.insert(name);
            continue;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let take = match best.as_ref().and_then(|(_, _, _, t)| *t) {
            Some(prev) => mtime.is_some_and(|cur| cur >= prev),
            None => true,
        };
        if take {
            best = Some((name, path, uuid, mtime));
        }
    }
    best
}

fn count_jsonl_lines(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn emit_new_codex_rollout_lines(path: &Path, next_line: &mut usize, emit: &EventEmitter) {
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
            Ok(v) => emit_codex_rollout_event(emit, &v),
            Err(e) => emit.log(format!(
                "codex transcript: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
}

fn emit_codex_rollout_event(emit: &EventEmitter, v: &Value) {
    for event in codex_rollout_events(v) {
        emit.emit(event);
    }
}

fn codex_rollout_events(v: &Value) -> Vec<SessionEvent> {
    if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
        return Vec::new();
    }
    let Some(payload) = v.get("payload") else {
        return Vec::new();
    };
    match payload.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "message" => {
            let role = match payload.get("role").and_then(|r| r.as_str()) {
                Some("user") => MessageRole::User,
                _ => MessageRole::Assistant,
            };
            if let Some(text) = extract_text_from_blocks(payload.get("content")) {
                if !text.trim().is_empty() {
                    return vec![SessionEvent::Message { role, text }];
                }
            }
            Vec::new()
        }
        "function_call" => {
            let tool = payload
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let args = payload
                .get("arguments")
                .and_then(|a| a.as_str())
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .or_else(|| payload.get("arguments").cloned())
                .unwrap_or(Value::Null);
            let call_id = payload
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            vec![SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }]
        }
        "function_call_output" => {
            let tool = payload
                .get("call_id")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let output = match payload.get("output") {
                Some(Value::String(s)) => s.clone(),
                Some(v) => serde_json::to_string(v).unwrap_or_default(),
                None => String::new(),
            };
            // No tool name is available here; `tool` keeps the call_id and
            // `call_id` carries the explicit correlation key.
            let call_id = Some(tool.clone());
            vec![SessionEvent::ToolResult {
                tool,
                ok: true,
                output,
                call_id,
            }]
        }
        _ => Vec::new(),
    }
}

/// Where codex stores its rollout files. Honors `$CODEX_HOME` (checked
/// in the session's env first, then the adapter's own env), falling
/// back to `$HOME/.codex/sessions`.
fn codex_sessions_root(session_env: &HashMap<String, String>) -> Option<PathBuf> {
    if let Some(home) = session_env.get("CODEX_HOME").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(home).join("sessions"));
    }
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join("sessions"));
        }
    }
    let home = std::env::var("HOME").ok().filter(|s| !s.is_empty())?;
    Some(PathBuf::from(home).join(".codex").join("sessions"))
}

/// Recursively list every `rollout-*.jsonl` file under `root`. Returns
/// `(filename, full_path)` pairs. Empty Vec if `root` doesn't exist
/// yet — that's the "first ever codex run" case.
fn list_rollouts(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                stack.push(path);
            } else if ft.is_file() {
                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n,
                    None => continue,
                };
                if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                    out.push((name.to_string(), path));
                }
            }
        }
    }
    out
}

/// Subset of fields we care about from codex's `session_meta` JSONL line.
#[derive(Default)]
struct SessionMeta {
    id: Option<String>,
    originator: Option<String>,
    parent_thread_id: Option<String>,
}

/// Read the rollout's first JSONL line and pull `payload.id` and
/// `payload.originator`. Returns `None` if the file is empty / mid-write
/// / not parseable — caller should re-check on a later tick.
fn read_session_meta(path: &Path) -> Option<SessionMeta> {
    let text = std::fs::read_to_string(path).ok()?;
    let first = text.lines().next()?;
    let v: Value = serde_json::from_str(first).ok()?;
    let payload = v.get("payload")?;
    Some(SessionMeta {
        id: payload.get("id").and_then(|s| s.as_str()).map(String::from),
        originator: payload
            .get("originator")
            .and_then(|s| s.as_str())
            .map(String::from),
        parent_thread_id: payload
            .get("parent_thread_id")
            .and_then(|s| s.as_str())
            .map(String::from),
    })
}

fn read_new_codex_values(path: &Path, next_line: &mut usize, emit: &EventEmitter) -> Vec<Value> {
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
        match serde_json::from_str(line) {
            Ok(value) => values.push(value),
            Err(error) => emit.log(format!(
                "codex subagent transcript: failed to parse {} line {}: {error}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
    values
}

fn codex_native_state(value: &Value) -> Option<SessionState> {
    if value.get("type").and_then(Value::as_str) != Some("event_msg") {
        return None;
    }
    match value.pointer("/payload/type").and_then(Value::as_str) {
        Some("task_started") => Some(SessionState::Running),
        Some("task_complete") => Some(SessionState::Done),
        Some("task_failed") => Some(SessionState::Errored),
        _ => None,
    }
}

fn short_codex_id(id: &str) -> &str {
    id.get(..id.len().min(8)).unwrap_or(id)
}

fn short_title(text: &str) -> String {
    let title: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if title.chars().count() <= 72 {
        title
    } else {
        format!("{}…", title.chars().take(71).collect::<String>())
    }
}

/// Extract the trailing UUID from a `rollout-<ts>-<uuid>.jsonl` filename.
/// Returns `None` if the trailing 36 chars don't look UUID-shaped.
fn uuid_from_rollout_name(name: &str) -> Option<String> {
    let stem = name.strip_prefix("rollout-")?.strip_suffix(".jsonl")?;
    if stem.len() < 36 {
        return None;
    }
    let uuid = &stem[stem.len() - 36..];
    // 8-4-4-4-12 hex digits
    let parts: Vec<&str> = uuid.split('-').collect();
    if parts.len() != 5 {
        return None;
    }
    let lens = [8usize, 4, 4, 4, 12];
    for (p, want) in parts.iter().zip(lens.iter()) {
        if p.len() != *want || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
    }
    Some(uuid.to_string())
}

async fn run_session(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id: agentd_session_id,
        emit,
        mut inbox,
    } = ctx;

    let command_override = agentd_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CODEX_CMD",
        "CONSTRUCT_CODEX_BIN",
        "codex",
    );
    let resume_flag = std::env::var("CONSTRUCT_CODEX_RESUME_FLAG").ok();
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

        let mut child_args: Vec<String> = command_override.args.clone();
        child_args.push("exec".into());
        if let (Some(flag), Some(sid)) = (resume_flag.as_ref(), codex_session_id.as_ref()) {
            child_args.push(flag.clone());
            child_args.push(sid.clone());
        }
        if let Some(m) = &model {
            child_args.push("-m".into());
            child_args.push(m.clone());
        }
        for a in agentd_protocol::adapter::maybe_inject_codex_mcp_args(&agentd_session_id) {
            child_args.push(a);
        }
        for a in &extra_args {
            child_args.push(a.clone());
        }
        child_args.push(user_text.clone());
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
        let captured_sid = Arc::new(StdMutex::new(None::<String>));
        let stdout_task = spawn_stdout(child_stdout, emit.clone(), captured_sid.clone());
        let stderr_task = spawn_stderr_log(child_stderr, emit.clone());

        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        let _ = child.wait().await;

        // Always adopt the latest native id so a mid-run reset is honored
        // on subsequent turns (and written for daemon resume).
        if let Some(sid) = captured_sid.lock().unwrap().clone() {
            if codex_session_id.as_ref() != Some(&sid) {
                if let Ok(dir) = std::env::var("CONSTRUCT_SESSION_DATA_DIR") {
                    let p = PathBuf::from(dir).join("codex_session_id.txt");
                    let _ = std::fs::write(p, &sid);
                }
                codex_session_id = Some(sid);
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
        // codex CLI prints a per-turn footer like:
        //   tokens used
        //   2,280
        // on two consecutive lines. Track whether we just saw the header.
        let mut expecting_token_count = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            // Stateful token-footer parse, BEFORE any emit, so the footer
            // never leaks to the transcript as assistant prose.
            if expecting_token_count {
                expecting_token_count = false;
                if let Some(n) = parse_token_count(&line) {
                    emit.emit(SessionEvent::Cost {
                        usd: 0.0,
                        // codex reports a single "total tokens used" per turn
                        // without splitting in/out. Stored in tokens_in as a
                        // conservative proxy (the prompt/context dominates).
                        tokens_in: n,
                        tokens_out: 0,
                        tokens_cached: 0,
                    });
                    continue;
                }
                // Fall through if the line wasn't a number — treat it normally.
            }
            if line.trim().eq_ignore_ascii_case("tokens used") {
                expecting_token_count = true;
                continue;
            }
            // Best-effort JSON parse; if not JSON, emit as plain assistant text.
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if let Some(sid) = v.get("session_id").and_then(|s| s.as_str()) {
                    let mut g = captured_sid.lock().unwrap();
                    // Keep the most recently observed id (not only the first).
                    *g = Some(sid.to_string());
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

/// Parse codex's "2,280" style total-tokens line. Strips commas/whitespace.
/// Returns None if the line isn't a pure integer (modulo separators).
fn parse_token_count(line: &str) -> Option<u64> {
    let cleaned: String = line
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ',' && *c != '_')
        .collect();
    if cleaned.is_empty() {
        return None;
    }
    cleaned.parse::<u64>().ok()
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
            let call_id = v.get("id").and_then(|n| n.as_str()).map(str::to_string);
            emit.emit(SessionEvent::ToolUse {
                tool: name,
                args,
                call_id,
            });
            true
        }
        "tool_result" => {
            let tool = v
                .get("tool_use_id")
                .or_else(|| v.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let ok = !v.get("is_error").and_then(|b| b.as_bool()).unwrap_or(false);
            let output = match v.get("output").or_else(|| v.get("content")) {
                Some(Value::String(s)) => s.clone(),
                Some(other) => serde_json::to_string(other).unwrap_or_default(),
                None => String::new(),
            };
            // No tool name is available here; `tool` keeps the id and `call_id`
            // carries the explicit correlation key.
            let call_id = Some(tool.clone());
            emit.emit(SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_token_count_handles_codex_formats() {
        // Plain integer, comma-separated, underscore-separated, with whitespace.
        assert_eq!(parse_token_count("2280"), Some(2280));
        assert_eq!(parse_token_count("2,280"), Some(2280));
        assert_eq!(parse_token_count("  4,448 "), Some(4448));
        assert_eq!(parse_token_count("1_234_567"), Some(1234567));
        // Non-numbers reject.
        assert_eq!(parse_token_count("hello"), None);
        assert_eq!(parse_token_count(""), None);
        assert_eq!(parse_token_count("12abc"), None);
    }

    #[test]
    fn uuid_from_rollout_name_parses_real_codex_filename() {
        let name = "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl";
        assert_eq!(
            uuid_from_rollout_name(name).as_deref(),
            Some("019e32aa-014a-7ff0-9a3f-7ae773961a37"),
        );
    }

    #[test]
    fn uuid_from_rollout_name_rejects_garbage() {
        assert!(uuid_from_rollout_name("rollout-foo.jsonl").is_none());
        assert!(uuid_from_rollout_name("not-a-rollout.jsonl").is_none());
        assert!(uuid_from_rollout_name(
            "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.txt"
        )
        .is_none());
        // Right length, non-hex characters.
        assert!(
            uuid_from_rollout_name("rollout-zzz-zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz.jsonl")
                .is_none()
        );
    }

    #[test]
    fn read_session_meta_extracts_id_and_originator() {
        let tmp =
            std::env::temp_dir().join(format!("agentd-codex-meta-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let mine = tmp.join("rollout-mine.jsonl");
        std::fs::write(
            &mine,
            "{\"timestamp\":\"x\",\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"019e32aa-014a-7ff0-9a3f-7ae773961a37\",\
             \"cwd\":\"/work/me\",\"originator\":\"agentd:sess-abc\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&mine).unwrap();
        assert_eq!(
            meta.id.as_deref(),
            Some("019e32aa-014a-7ff0-9a3f-7ae773961a37")
        );
        assert_eq!(meta.originator.as_deref(), Some("agentd:sess-abc"));
        assert_eq!(meta.parent_thread_id, None);

        // Default codex originator stays distinct.
        let other = tmp.join("rollout-other.jsonl");
        std::fs::write(
            &other,
            "{\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"u\",\"originator\":\"codex-tui\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&other).unwrap();
        assert_eq!(meta.originator.as_deref(), Some("codex-tui"));

        // Empty / mid-write file: caller will re-check later.
        let blank = tmp.join("rollout-blank.jsonl");
        std::fs::write(&blank, "").unwrap();
        assert!(read_session_meta(&blank).is_none());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_best_matching_rollout_prefers_newest_after_clear() {
        // Simulate /clear: two rollouts share our originator; the newer
        // mtime (post-clear) must win so resume/fork follow the active id.
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-best-match-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let old_name = "rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl";
        let new_name = "rollout-2026-05-16T15-00-00-019e32bb-014a-7ff0-9a3f-7ae773961a99.jsonl";
        let old_path = tmp.join(old_name);
        let new_path = tmp.join(new_name);
        let originator = "agentd:sess-clear-test";
        let meta = |id: &str| {
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{id}\",\"originator\":\"{originator}\"}}}}\n"
            )
        };
        std::fs::write(&old_path, meta("019e32aa-014a-7ff0-9a3f-7ae773961a37")).unwrap();
        // Ensure distinct mtimes so "newest" is well-defined.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new_path, meta("019e32bb-014a-7ff0-9a3f-7ae773961a99")).unwrap();

        // Unrelated originator must be ignored.
        std::fs::write(
            tmp.join("rollout-2026-05-16T16-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":\
             {\"id\":\"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\",\
             \"originator\":\"codex-tui\"}}\n",
        )
        .unwrap();

        let mut not_ours = HashSet::new();
        let best = find_best_matching_rollout(&tmp, originator, None, &mut not_ours)
            .expect("should find a match");
        assert_eq!(best.0, new_name);
        assert_eq!(best.2, "019e32bb-014a-7ff0-9a3f-7ae773961a99");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn read_session_meta_extracts_native_parent() {
        let tmp = std::env::temp_dir().join(format!(
            "construct-codex-native-parent-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let rollout = tmp.join("rollout-child.jsonl");
        std::fs::write(
            &rollout,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"child\",\"parent_thread_id\":\"parent\",\"thread_source\":\"subagent\"}}\n",
        )
        .unwrap();
        let meta = read_session_meta(&rollout).unwrap();
        assert_eq!(meta.id.as_deref(), Some("child"));
        assert_eq!(meta.parent_thread_id.as_deref(), Some("parent"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn native_state_uses_child_task_lifecycle() {
        assert_eq!(
            codex_native_state(&serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_started"}
            })),
            Some(SessionState::Running)
        );
        assert_eq!(
            codex_native_state(&serde_json::json!({
                "type": "event_msg",
                "payload": {"type": "task_complete"}
            })),
            Some(SessionState::Done)
        );
    }

    #[test]
    fn codex_sessions_root_prefers_session_env_then_process_env_then_home() {
        let mut session_env = HashMap::new();
        session_env.insert("CODEX_HOME".into(), "/sess/codex".into());
        assert_eq!(
            codex_sessions_root(&session_env),
            Some(PathBuf::from("/sess/codex/sessions"))
        );
        // Empty value in session env falls through.
        session_env.insert("CODEX_HOME".into(), "".into());
        let got = codex_sessions_root(&session_env);
        // Result depends on the test runner's env; we just assert that
        // an empty session-env value doesn't masquerade as a real one.
        if let Some(p) = got {
            assert_ne!(p, PathBuf::from("/sessions"));
        }
    }

    #[test]
    fn rollout_message_records_become_chat_messages() {
        let user = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            }
        });
        let assistant = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [
                    { "type": "output_text", "text": "one" },
                    { "type": "output_text", "text": "two" }
                ]
            }
        });

        match codex_rollout_events(&user).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::User));
                assert_eq!(text, "hello");
            }
            other => panic!("unexpected user events: {other:?}"),
        }
        match codex_rollout_events(&assistant).as_slice() {
            [SessionEvent::Message { role, text }] => {
                assert!(matches!(role, MessageRole::Assistant));
                assert_eq!(text, "one\ntwo");
            }
            other => panic!("unexpected assistant events: {other:?}"),
        }
    }

    #[test]
    fn rollout_function_records_become_tool_events() {
        let call = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "name": "exec_command",
                "arguments": "{\"cmd\":\"cargo test\"}",
                "call_id": "call_1"
            }
        });
        let output = serde_json::json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "ok"
            }
        });

        match codex_rollout_events(&call).as_slice() {
            [SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            }] => {
                assert_eq!(tool, "exec_command");
                assert_eq!(args["cmd"], "cargo test");
                assert_eq!(call_id.as_deref(), Some("call_1"));
            }
            other => panic!("unexpected tool-use events: {other:?}"),
        }
        match codex_rollout_events(&output).as_slice() {
            [SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            }] => {
                assert_eq!(tool, "call_1");
                assert!(*ok);
                assert_eq!(output, "ok");
                assert_eq!(call_id.as_deref(), Some("call_1"));
            }
            other => panic!("unexpected tool-result events: {other:?}"),
        }
    }
}
