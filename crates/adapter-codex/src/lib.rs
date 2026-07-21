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

use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use construct_adapter_common::{
    codex_sessions_root, drive_turn, next_native_seq, spawn_stderr_log, TurnOutcome,
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
    let command = construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_CODEX_CMD",
        "CONSTRUCT_CODEX_BIN",
        "codex",
    );
    let mut args = command.args.clone();
    args.extend(params.args.clone());
    // The daemon's auto-approval policy (`CONSTRUCT_AUTO_APPROVE_PATHS`, see
    // `construct_protocol::adapter::policy`) is set, but the upstream codex CLI
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
    let fork_from = (!resuming)
        .then(|| {
            std::env::var("CONSTRUCT_CODEX_FORK_FROM")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .flatten();
    if let Some(parent) = fork_from.clone() {
        // Same-harness fork: `codex fork <parent-uuid>` starts a NEW codex
        // conversation inheriting the parent's exact context (the daemon
        // read the parent's captured id — spec 0031/0078). The forked
        // rollout copies the parent's meta (originator included) and stamps
        // `forked_from_id`, which is how the watcher below identifies it.
        args.insert(0, "fork".into());
        args.insert(1, parent);
    }
    if let Some(m) = params.model.as_ref() {
        args.push("-m".into());
        args.push(m.clone());
    }
    // Auto-inject agentd MCP server via codex's `-c` override (codex has no
    // `--mcp-config` flag — MCP servers live in `[mcp_servers.<name>]`).
    // Opt out with CONSTRUCT_INJECT_MCP=0.
    for a in construct_protocol::adapter::maybe_inject_codex_mcp_args(&ctx.session_id) {
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
    // in the same cwd belongs to which construct session.
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
            fork_from.clone(),
            params.model.clone(),
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
    expected_fork_parent: Option<String>,
    initial_model: Option<String>,
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
        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
        let mut reported_usage = UsageTotals::default();
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
                expected_fork_parent.as_deref(),
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
                    let existing_sid = std::fs::read_to_string(&sid_file)
                        .ok()
                        .map(|s| s.trim().to_string());
                    let should_write = existing_sid.as_deref() != Some(uuid.as_str());
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
                    // Usage reporting follows the same split: a resumed
                    // session's historical usage is already in the daemon's
                    // tally, so baseline off the file's last snapshot; a
                    // fresh conversation starts from zero.
                    if first_select && skip_existing {
                        next_line = count_jsonl_lines(&path);
                        reported_usage = last_rollout_usage_totals(&path);
                    } else {
                        next_line = 0;
                        reported_usage = UsageTotals::default();
                    };
                    if !first_select {
                        emit.log(format!(
                            "codex: native session id rebinding to {uuid} (from {})",
                            path.display()
                        ));
                        if let Some(prior) = existing_sid.filter(|s| !s.is_empty()) {
                            emit.emit(SessionEvent::NativeIdChanged {
                                prior_native_id: prior,
                                new_native_id: uuid.clone(),
                            });
                        }
                    }
                    selected = Some((name.clone(), path.clone()));
                    selected_mtime = mtime;
                }
            }

            let Some((_, path)) = selected.as_ref() else {
                continue;
            };
            emit_new_codex_rollout_lines(
                path,
                &mut next_line,
                &emit,
                &mut last_model,
                &mut last_effort,
                &mut reported_usage,
            );

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
    expected_fork_parent: Option<&str>,
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
        // A fork child COPIES its parent's originator into its meta
        // (`codex fork`), so an originator hit alone isn't ours when the
        // rollout says it was forked from somewhere — otherwise a fork of
        // this session would read as our own /clear rebind and steal the
        // parent's identity.
        let originator_matches = meta.originator.as_deref() == Some(expected_originator)
            && meta.parent_thread_id.is_none()
            && meta.forked_from_id.is_none();
        let uuid_matches = expected_uuid.is_some_and(|want| uuid.as_deref() == Some(want));
        // A session spawned as `codex fork <parent>` binds to the rollout
        // that names that parent — its own originator was copied from the
        // parent, so the tag can't identify it. (Two simultaneous forks of
        // one parent are ambiguous; newest wins, same as codex's own
        // `--last`.)
        let fork_matches = expected_fork_parent
            .is_some_and(|parent| meta.forked_from_id.as_deref() == Some(parent));
        if !originator_matches && !uuid_matches && !fork_matches {
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

fn emit_new_codex_rollout_lines(
    path: &Path,
    next_line: &mut usize,
    emit: &EventEmitter,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
    reported_usage: &mut UsageTotals,
) {
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
            Ok(v) => emit_codex_rollout_event(emit, &v, last_model, last_effort, reported_usage),
            Err(e) => emit.log(format!(
                "codex transcript: failed to parse {} line {}: {e}",
                path.display(),
                idx + 1
            )),
        }
    }
    *next_line = seen;
}

fn emit_codex_rollout_event(
    emit: &EventEmitter,
    v: &Value,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
    reported_usage: &mut UsageTotals,
) {
    if let Some(model) = codex_model_change(v, last_model) {
        *last_model = Some(model.clone());
        emit.emit(SessionEvent::ModelChanged { model });
    }
    if let Some(effort) = codex_effort_change(v, last_effort) {
        *last_effort = Some(effort.clone());
        emit.emit(SessionEvent::EffortChanged { effort });
    }
    for event in codex_usage_events(v, reported_usage) {
        emit.emit(event);
    }
    for event in codex_rollout_events(v) {
        emit.emit(event);
    }
}

/// Cumulative token totals already reported for the bound rollout, so each
/// `token_count` record contributes only its delta (spec 0103).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct UsageTotals {
    input: u64,
    output: u64,
    cached: u64,
}

/// Token usage from a rollout's `event_msg`/`token_count` record. Codex
/// stamps a cumulative `total_token_usage` (input includes cached; a
/// snapshot may repeat unchanged), so emitting the delta against what was
/// already reported both splits the stream into per-call Cost events and
/// makes duplicates harmless. This supersedes the headless footer parse
/// for interactive sessions — the footer only exists in `codex exec`
/// output, which has no rollout watcher, so the two never overlap.
///
/// A fresh delta also refreshes the context gauge (spec 0104): the same
/// record's `last_token_usage.input_tokens` is the prompt side of the most
/// recent call — exactly what filled the window — and codex states the
/// window itself in `model_context_window`. Gated on the delta so repeated
/// identical snapshots don't respam an unchanged gauge.
fn codex_usage_events(v: &Value, reported: &mut UsageTotals) -> Vec<SessionEvent> {
    if v.get("type").and_then(Value::as_str) != Some("event_msg") {
        return Vec::new();
    }
    let Some(payload) = v.get("payload") else {
        return Vec::new();
    };
    if payload.get("type").and_then(Value::as_str) != Some("token_count") {
        return Vec::new();
    }
    let Some(total) = rollout_usage_totals(payload) else {
        return Vec::new();
    };
    let d_in = total.input.saturating_sub(reported.input);
    let d_out = total.output.saturating_sub(reported.output);
    let d_cached = total.cached.saturating_sub(reported.cached);
    if d_in == 0 && d_out == 0 && d_cached == 0 {
        return Vec::new();
    }
    reported.input = reported.input.max(total.input);
    reported.output = reported.output.max(total.output);
    reported.cached = reported.cached.max(total.cached);
    let mut out = vec![SessionEvent::Cost {
        usd: 0.0,
        tokens_in: d_in,
        tokens_out: d_out,
        tokens_cached: d_cached,
    }];
    let last_input = payload
        .pointer("/info/last_token_usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if last_input > 0 {
        out.push(SessionEvent::ContextUsage {
            used_tokens: last_input,
            window_tokens: payload
                .pointer("/info/model_context_window")
                .and_then(Value::as_u64)
                .filter(|w| *w > 0),
        });
    }
    out
}

/// The cumulative `total_token_usage` from a `token_count` payload.
fn rollout_usage_totals(payload: &Value) -> Option<UsageTotals> {
    let total = payload.pointer("/info/total_token_usage")?;
    let field = |k: &str| total.get(k).and_then(Value::as_u64).unwrap_or(0);
    Some(UsageTotals {
        input: field("input_tokens"),
        output: field("output_tokens"),
        cached: field("cached_input_tokens"),
    })
}

/// The last cumulative usage snapshot already present in `path` — the
/// baseline for a resumed session's watcher. Without this, the first live
/// `token_count` after a resume would report the WHOLE conversation's
/// historical usage as one giant delta on top of the totals the daemon
/// already recounted from its own transcript.
fn last_rollout_usage_totals(path: &Path) -> UsageTotals {
    let Ok(text) = std::fs::read_to_string(path) else {
        return UsageTotals::default();
    };
    let mut latest = UsageTotals::default();
    for line in text.lines() {
        if !line.contains("token_count") {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        if let Some(t) = v.get("payload").and_then(rollout_usage_totals) {
            latest = t;
        }
    }
    latest
}

/// The root session's active model, if `v` is a `turn_context` rollout
/// record carrying `payload.model` that differs from what we last saw.
/// Scoped to `emit_codex_rollout_event` (root-only — the subagent mirroring
/// path calls `codex_rollout_events` directly, bypassing this) rather than
/// inside `codex_rollout_events` itself, which gates on `response_item` and
/// would never see a `turn_context` record anyway.
///
/// Verified against 1849 real codex rollouts on this machine: `turn_context`
/// repeats every turn (up to 737 times in one file) and 19 rollouts had more
/// than one distinct `payload.model` value within a single session,
/// confirming this field tracks a live model switch (unlike grok's
/// `model_id`, which turned out to be frozen per session).
fn codex_model_change(v: &Value, last_model: &Option<String>) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some("turn_context") {
        return None;
    }
    let model = v.pointer("/payload/model")?.as_str()?;
    (last_model.as_deref() != Some(model)).then(|| model.to_string())
}

/// Same signal as `codex_model_change`, for `payload.effort` (e.g.
/// `"high"`/`"medium"`/`"low"`) in the same `turn_context` record. Verified
/// against the same 1849 real rollouts: 14 had more than one distinct effort
/// value within a session, confirming it's live like `model`, not frozen
/// like grok's fields.
fn codex_effort_change(v: &Value, last_effort: &Option<String>) -> Option<String> {
    if v.get("type").and_then(Value::as_str) != Some("turn_context") {
        return None;
    }
    let effort = v.pointer("/payload/effort")?.as_str()?;
    (last_effort.as_deref() != Some(effort)).then(|| effort.to_string())
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
    /// `codex fork` stamps the source session's uuid here — the
    /// discriminator between "my own /clear rebind" and "a fork of me"
    /// (forks COPY the parent's originator into their meta).
    forked_from_id: Option<String>,
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
        forked_from_id: payload
            .get("forked_from_id")
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

    let command_override = construct_protocol::adapter::resolve_command_override(
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
        for a in construct_protocol::adapter::maybe_inject_codex_mcp_args(&agentd_session_id) {
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
    fn token_count_records_emit_delta_costs_and_context_gauge() {
        // Real rollout shape: cumulative `total_token_usage` where `input_
        // tokens` already includes the cached reads, plus the most recent
        // call's own usage and the model window. Two snapshots → first
        // emits its full totals, second emits only the delta, a repeated
        // (unchanged) snapshot emits nothing — and each fresh delta rides
        // with a ContextUsage gauge from `last_token_usage` (spec 0104).
        let snapshot = |input: u64, cached: u64, output: u64, last_input: u64| {
            serde_json::json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": input,
                            "cached_input_tokens": cached,
                            "output_tokens": output,
                            "reasoning_output_tokens": 3,
                            "total_tokens": input + output
                        },
                        "last_token_usage": { "input_tokens": last_input },
                        "model_context_window": 258_400
                    }
                }
            })
        };
        let mut reported = UsageTotals::default();
        match codex_usage_events(&snapshot(19_094, 9_984, 184, 19_094), &mut reported).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage {
                used_tokens,
                window_tokens,
            }] => {
                assert_eq!(*tokens_in, 19_094);
                assert_eq!(*tokens_out, 184);
                assert_eq!(*tokens_cached, 9_984);
                assert_eq!(*used_tokens, 19_094);
                assert_eq!(*window_tokens, Some(258_400));
            }
            other => panic!("expected Cost + ContextUsage: {other:?}"),
        }
        match codex_usage_events(&snapshot(48_890, 19_968, 257, 29_796), &mut reported).as_slice() {
            [SessionEvent::Cost {
                tokens_in,
                tokens_out,
                tokens_cached,
                ..
            }, SessionEvent::ContextUsage { used_tokens, .. }] => {
                assert_eq!(*tokens_in, 29_796);
                assert_eq!(*tokens_out, 73);
                assert_eq!(*tokens_cached, 9_984);
                assert_eq!(*used_tokens, 29_796);
            }
            other => panic!("expected delta Cost + ContextUsage: {other:?}"),
        }
        assert!(
            codex_usage_events(&snapshot(48_890, 19_968, 257, 29_796), &mut reported).is_empty(),
            "an unchanged snapshot must not re-report usage or respam the gauge"
        );
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
        let best = find_best_matching_rollout(&tmp, originator, None, None, &mut not_ours)
            .expect("should find a match");
        assert_eq!(best.0, new_name);
        assert_eq!(best.2, "019e32bb-014a-7ff0-9a3f-7ae773961a99");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fork_children_never_steal_the_parents_originator_match() {
        // `codex fork` COPIES the parent's meta — originator included — and
        // stamps `forked_from_id`. The parent's watcher must not treat the
        // fork's (newer) rollout as its own /clear rebind, and the fork's
        // watcher finds its rollout via the parent linkage instead.
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-fork-match-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let originator = "agentd:parent-sess";
        let parent_uuid = "019e32aa-014a-7ff0-9a3f-7ae773961a37";
        let fork_uuid = "019e32bb-014a-7ff0-9a3f-7ae773961a99";
        std::fs::write(
            tmp.join("rollout-2026-05-16T14-21-02-019e32aa-014a-7ff0-9a3f-7ae773961a37.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{parent_uuid}\",\"originator\":\"{originator}\"}}}}\n"
            ),
        )
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        // The fork's rollout: newer, same originator, forked_from_id set.
        std::fs::write(
            tmp.join("rollout-2026-05-16T15-00-00-019e32bb-014a-7ff0-9a3f-7ae773961a99.jsonl"),
            format!(
                "{{\"type\":\"session_meta\",\"payload\":\
                 {{\"id\":\"{fork_uuid}\",\"originator\":\"{originator}\",\
                 \"forked_from_id\":\"{parent_uuid}\"}}}}\n"
            ),
        )
        .unwrap();

        // Parent's view: the fork rollout is newer but must NOT win.
        let mut not_ours = HashSet::new();
        let best = find_best_matching_rollout(&tmp, originator, None, None, &mut not_ours)
            .expect("parent still matches its own rollout");
        assert_eq!(best.2, parent_uuid);

        // Fork's view: its originator tag was copied from the parent, so it
        // identifies its rollout by the fork linkage.
        let mut not_ours = HashSet::new();
        let best = find_best_matching_rollout(
            &tmp,
            "agentd:fork-sess",
            None,
            Some(parent_uuid),
            &mut not_ours,
        )
        .expect("fork matches via forked_from_id");
        assert_eq!(best.2, fork_uuid);

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

    #[test]
    fn model_change_ignored_for_non_turn_context_records() {
        let v = serde_json::json!({"type": "response_item", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(codex_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_ignored_when_payload_model_absent() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(codex_model_change(&v, &None), None);
    }

    #[test]
    fn model_change_fires_on_first_observation() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(
            codex_model_change(&v, &None).as_deref(),
            Some("gpt-5.6-terra")
        );
    }

    #[test]
    fn model_change_silent_when_unchanged() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(
            codex_model_change(&v, &Some("gpt-5.6-terra".to_string())),
            None
        );
    }

    #[test]
    fn model_change_fires_on_switch() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.3-codex-spark"}});
        assert_eq!(
            codex_model_change(&v, &Some("gpt-5.6-terra".to_string())).as_deref(),
            Some("gpt-5.3-codex-spark")
        );
    }

    #[test]
    fn effort_change_ignored_for_non_turn_context_records() {
        let v = serde_json::json!({"type": "response_item", "payload": {"effort": "high"}});
        assert_eq!(codex_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_ignored_when_payload_effort_absent() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"model": "gpt-5.6-terra"}});
        assert_eq!(codex_effort_change(&v, &None), None);
    }

    #[test]
    fn effort_change_fires_on_first_observation() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(codex_effort_change(&v, &None).as_deref(), Some("medium"));
    }

    #[test]
    fn effort_change_silent_when_unchanged() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "medium"}});
        assert_eq!(
            codex_effort_change(&v, &Some("medium".to_string())),
            None
        );
    }

    #[test]
    fn effort_change_fires_on_switch() {
        let v = serde_json::json!({"type": "turn_context", "payload": {"effort": "high"}});
        assert_eq!(
            codex_effort_change(&v, &Some("medium".to_string())).as_deref(),
            Some("high")
        );
    }
}
