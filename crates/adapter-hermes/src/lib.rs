//! Hermes Agent CLI adapter.
//!
//! Runs Hermes' native terminal UI under construct's PTY or its `--oneshot`
//! command in headless mode. Hermes persists every conversation, message,
//! model selection, and usage counter in `$HERMES_HOME/state.db`; the adapter
//! tags its process with a construct-session-specific source and follows only
//! those rows. That gives interactive sessions structured transcript events,
//! exact token/cost deltas, live model changes, and reliable native resume
//! without scraping terminal text.
//!
//! Honors `CONSTRUCT_HERMES_CMD` for a full command prefix, falling back to
//! `CONSTRUCT_HERMES_BIN`, then `hermes` on `PATH`, then the standard installer
//! location at `~/.local/bin/hermes`. `CONSTRUCT_HERMES_HOME` can point the
//! adapter and child at a non-default Hermes home.

use construct_adapter_common::{drive_turn, spawn_stderr_log, TurnOutcome};
use construct_protocol::adapter::pty::{run_session as run_pty, PtySpec};
use construct_protocol::adapter::{
    run as adapter_run, AdapterContext, AdapterInboxMsg, EventEmitter,
};
use construct_protocol::{
    Capabilities, InitializeResult, MessageRole, PtySize, SessionEvent, SessionStartParams,
    SessionState,
};
use rusqlite::{params, Connection, OpenFlags};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

const SESSION_ID_FILE: &str = "hermes_session_id.txt";

pub async fn run() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "hermes".into(),
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
            Mode::Headless => run_headless(params, ctx).await,
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
    if let Ok(mode) = std::env::var("CONSTRUCT_HERMES_MODE") {
        match mode.as_str() {
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

fn hermes_home() -> Option<PathBuf> {
    std::env::var_os("CONSTRUCT_HERMES_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HERMES_HOME")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".hermes")))
}

fn session_id_path() -> Option<PathBuf> {
    std::env::var_os("CONSTRUCT_SESSION_DATA_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join(SESSION_ID_FILE))
}

fn read_native_id() -> Option<String> {
    std::fs::read_to_string(session_id_path()?)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| valid_session_id(value))
}

fn write_native_id(id: &str) {
    if let Some(path) = session_id_path() {
        let _ = std::fs::write(path, id);
    }
}

fn valid_session_id(id: &str) -> bool {
    let mut parts = id.split('_');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(date), Some(time), Some(suffix), None)
            if date.len() == 8
                && time.len() == 6
                && suffix.len() >= 6
                && date.chars().all(|c| c.is_ascii_digit())
                && time.chars().all(|c| c.is_ascii_digit())
                && suffix.chars().all(|c| c.is_ascii_hexdigit())
    )
}

fn default_command() -> construct_protocol::adapter::CommandOverride {
    let default_bin = construct_protocol::adapter::default_cli_bin_with_home_fallback(
        "hermes",
        Path::new(".local/bin/hermes"),
    );
    construct_protocol::adapter::resolve_command_override(
        "CONSTRUCT_HERMES_CMD",
        "CONSTRUCT_HERMES_BIN",
        &default_bin,
    )
}

fn source_tag(construct_session_id: &str) -> String {
    format!("construct:{construct_session_id}")
}

fn child_env(
    params: &SessionStartParams,
    construct_session_id: &str,
    home: Option<&Path>,
) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = params
        .env
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    env.retain(|(key, _)| {
        key != "HERMES_SESSION_SOURCE" && (home.is_none() || key != "HERMES_HOME")
    });
    env.push((
        "HERMES_SESSION_SOURCE".into(),
        source_tag(construct_session_id),
    ));
    env.push(("CONSTRUCT_SESSION_ID".into(), construct_session_id.into()));
    if let Some(home) = home {
        env.push(("HERMES_HOME".into(), home.to_string_lossy().into_owned()));
    }
    env
}

fn append_common_args(
    args: &mut Vec<String>,
    params: &SessionStartParams,
    native_id: Option<&str>,
) {
    if let Some(model) = params.model.as_deref() {
        args.extend(["--model".into(), model.into()]);
    }
    if let Some(id) = native_id {
        args.extend(["--resume".into(), id.into(), "--no-restore-cwd".into()]);
    }
    args.extend(params.args.clone());
}

async fn run_interactive(params: SessionStartParams, mut ctx: AdapterContext) {
    let command = default_command();
    let home = hermes_home();
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let native_id = resuming.then(read_native_id).flatten();
    if resuming && native_id.is_none() {
        ctx.emit
            .log("hermes respawn: no captured native session id; starting a fresh conversation");
    }

    let mut args = command.args.clone();
    args.push("chat".into());
    append_common_args(&mut args, &params, native_id.as_deref());

    let prompt = (!resuming)
        .then(|| params.prompt.clone().filter(|text| !text.trim().is_empty()))
        .flatten();
    let prompt_sender = prompt.map(|prompt| {
        let (tx, rx) = mpsc::channel(64);
        let mut real_inbox = std::mem::replace(&mut ctx.inbox, rx);
        let forward = tx.clone();
        tokio::spawn(async move {
            while let Some(message) = real_inbox.recv().await {
                if forward.send(message).await.is_err() {
                    break;
                }
            }
        });
        (prompt, tx)
    });

    if let Some(home) = home.as_ref() {
        spawn_db_watcher(
            WatcherSetup {
                db_path: home.join("state.db"),
                source: source_tag(&ctx.session_id),
                initial_id: native_id,
                initial_model: params.model.clone(),
            },
            ctx.emit.clone(),
        );
    } else {
        ctx.emit
            .log("hermes: no CONSTRUCT_HERMES_HOME/HERMES_HOME/HOME; native state unavailable");
    }
    if let Some((prompt, tx)) = prompt_sender {
        // Hermes creates its SQLite session row on the first submitted turn,
        // so prompt delivery cannot wait for the database watcher to bind.
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            if tx
                .send(AdapterInboxMsg::PtyInput(prompt.into_bytes()))
                .await
                .is_ok()
            {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let _ = tx.send(AdapterInboxMsg::PtyInput(vec![b'\r'])).await;
            }
        });
    }

    let label = command.argv_preview();
    let spec = PtySpec {
        bin: command.bin,
        args,
        cwd: PathBuf::from(&params.cwd),
        env: child_env(&params, &ctx.session_id, home.as_deref()),
        size: params.pty_size.unwrap_or(PtySize {
            cols: 100,
            rows: 30,
        }),
        status_detail: Some(format!("{label} chat (interactive)")),
        detect_prompt_via_pgroup: false,
    };
    let _ = run_pty(spec, ctx).await;
}

fn initial_pending(prompt: &Option<String>) -> VecDeque<String> {
    prompt
        .as_ref()
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .into_iter()
        .collect()
}

async fn run_headless(params: SessionStartParams, ctx: AdapterContext) {
    let AdapterContext {
        session_id,
        emit,
        mut inbox,
    } = ctx;
    let command = default_command();
    let home = hermes_home();
    let resuming = std::env::var("CONSTRUCT_RESUME").as_deref() == Ok("1");
    let mut native_id = resuming.then(read_native_id).flatten();
    let mut pending = initial_pending(&params.prompt);

    if let Some(home) = home.as_ref() {
        spawn_db_watcher(
            WatcherSetup {
                db_path: home.join("state.db"),
                source: source_tag(&session_id),
                initial_id: native_id.clone(),
                initial_model: params.model.clone(),
            },
            emit.clone(),
        );
    }

    let exit_code = loop {
        let prompt = match pending.pop_front() {
            Some(prompt) => prompt,
            None => {
                emit.emit(SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                });
                match inbox.recv().await {
                    Some(AdapterInboxMsg::Input(text)) => text,
                    Some(AdapterInboxMsg::Stop) | None => break 0,
                    _ => continue,
                }
            }
        };
        if prompt.trim().is_empty() {
            continue;
        }

        // The watcher may have captured the id since the previous turn.
        native_id = read_native_id().or(native_id);
        let usage_path = session_id_path()
            .map(|path| path.with_file_name("hermes_usage.json"))
            .unwrap_or_else(|| PathBuf::from("hermes_usage.json"));
        let mut args = command.args.clone();
        args.extend(["--oneshot".into(), prompt]);
        args.extend([
            "--usage-file".into(),
            usage_path.to_string_lossy().into_owned(),
        ]);
        append_common_args(&mut args, &params, native_id.as_deref());

        let mut child = Command::new(&command.bin);
        child
            .args(&args)
            .current_dir(&params.cwd)
            .envs(child_env(&params, &session_id, home.as_deref()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let Ok(mut child) = child.spawn() else {
            emit.emit(SessionEvent::Error {
                message: format!("failed to spawn {}", command.bin),
            });
            break 127;
        };
        let stdout = child.stdout.take();
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_log(stderr, emit.clone());
        }
        let stdout_task = tokio::spawn(async move {
            let mut bytes = Vec::new();
            if let Some(mut stdout) = stdout {
                let _ = stdout.read_to_end(&mut bytes).await;
            }
            bytes
        });

        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: Some(format!("{} --oneshot", command.argv_preview())),
        });
        let outcome = drive_turn(&mut child, &mut inbox, &emit, &mut pending).await;
        let output = stdout_task.await.unwrap_or_default();
        match outcome {
            TurnOutcome::Stopped => break 0,
            TurnOutcome::Interrupted => continue,
            TurnOutcome::Completed => {
                if let Ok(status) = child.wait().await {
                    if !status.success() {
                        let message = String::from_utf8_lossy(&output).trim().to_string();
                        if !message.is_empty() {
                            emit.emit(SessionEvent::Error { message });
                        }
                    }
                }
                // The authoritative assistant text comes from state.db. Give
                // Hermes' final flush and the watcher one poll interval before
                // accepting another turn.
                tokio::time::sleep(Duration::from_millis(600)).await;
                native_id = read_native_id().or(native_id);
            }
        }
    };
    emit.emit(SessionEvent::Done { exit_code });
}

struct WatcherSetup {
    db_path: PathBuf,
    source: String,
    initial_id: Option<String>,
    initial_model: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct UsageTotals {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    reasoning: u64,
    usd: f64,
}

#[derive(Debug, Clone)]
struct NativeSession {
    id: String,
    model: Option<String>,
    effort: Option<String>,
    usage: UsageTotals,
}

#[derive(Debug)]
struct HermesMessage {
    id: i64,
    role: String,
    content: Option<String>,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_name: Option<String>,
    effect_disposition: Option<String>,
    reasoning: Option<String>,
    reasoning_content: Option<String>,
}

fn open_db(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
}

fn read_session(conn: &Connection, source: &str, id: Option<&str>) -> Option<NativeSession> {
    let columns = "id, model, model_config, input_tokens, output_tokens, \
                   cache_read_tokens, cache_write_tokens, reasoning_tokens, \
                   COALESCE(actual_cost_usd, estimated_cost_usd, 0)";
    let sql = if id.is_some() {
        format!("SELECT {columns} FROM sessions WHERE id = ?1")
    } else {
        format!("SELECT {columns} FROM sessions WHERE source = ?1 ORDER BY started_at DESC LIMIT 1")
    };
    let key = id.unwrap_or(source);
    conn.query_row(&sql, params![key], |row| {
        let model_config: Option<String> = row.get(2)?;
        Ok(NativeSession {
            id: row.get(0)?,
            model: row.get(1)?,
            effort: model_config.as_deref().and_then(reasoning_effort),
            usage: UsageTotals {
                input: row.get::<_, Option<u64>>(3)?.unwrap_or(0),
                output: row.get::<_, Option<u64>>(4)?.unwrap_or(0),
                cache_read: row.get::<_, Option<u64>>(5)?.unwrap_or(0),
                cache_write: row.get::<_, Option<u64>>(6)?.unwrap_or(0),
                reasoning: row.get::<_, Option<u64>>(7)?.unwrap_or(0),
                usd: row.get::<_, Option<f64>>(8)?.unwrap_or(0.0),
            },
        })
    })
    .ok()
}

fn reasoning_effort(model_config: &str) -> Option<String> {
    let value: Value = serde_json::from_str(model_config).ok()?;
    let effort = value.get("reasoning_config")?;
    match effort {
        Value::String(value) if !value.is_empty() => Some(value.clone()),
        Value::Object(map) => map
            .get("effort")
            .or_else(|| map.get("level"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
        _ => None,
    }
}

fn read_messages(conn: &Connection, session_id: &str, after_id: i64) -> Vec<HermesMessage> {
    let mut statement = match conn.prepare(
        "SELECT id, role, content, tool_call_id, tool_calls, tool_name, \
         effect_disposition, reasoning, reasoning_content \
         FROM messages WHERE session_id = ?1 AND id > ?2 AND active = 1 ORDER BY id",
    ) {
        Ok(statement) => statement,
        Err(_) => return Vec::new(),
    };
    statement
        .query_map(params![session_id, after_id], |row| {
            Ok(HermesMessage {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                tool_call_id: row.get(3)?,
                tool_calls: row.get(4)?,
                tool_name: row.get(5)?,
                effect_disposition: row.get(6)?,
                reasoning: row.get(7)?,
                reasoning_content: row.get(8)?,
            })
        })
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

fn max_message_id(conn: &Connection, session_id: &str) -> i64 {
    conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

fn message_events(message: &HermesMessage) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    if message.role == "assistant" {
        let reasoning = message
            .reasoning
            .as_deref()
            .or(message.reasoning_content.as_deref())
            .filter(|text| !text.trim().is_empty());
        if let Some(text) = reasoning {
            events.push(SessionEvent::Reasoning { text: text.into() });
        }
        if let Some(text) = message
            .content
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            events.push(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: text.into(),
            });
        }
        if let Some(raw) = message.tool_calls.as_deref() {
            if let Ok(Value::Array(calls)) = serde_json::from_str::<Value>(raw) {
                for call in calls {
                    let function = call.get("function").unwrap_or(&call);
                    let Some(tool) = function.get("name").and_then(Value::as_str) else {
                        continue;
                    };
                    let args = function
                        .get("arguments")
                        .cloned()
                        .map(|value| match value {
                            Value::String(raw) => {
                                serde_json::from_str(&raw).unwrap_or(Value::String(raw))
                            }
                            value => value,
                        })
                        .unwrap_or(Value::Null);
                    events.push(SessionEvent::ToolUse {
                        tool: tool.into(),
                        args,
                        call_id: call.get("id").and_then(Value::as_str).map(str::to_string),
                    });
                }
            }
        }
    } else if message.role == "tool" {
        let disposition = message.effect_disposition.as_deref().unwrap_or("");
        events.push(SessionEvent::ToolResult {
            tool: message.tool_name.clone().unwrap_or_else(|| "tool".into()),
            ok: !matches!(disposition, "denied" | "blocked" | "error" | "failed"),
            output: message.content.clone().unwrap_or_default(),
            call_id: message.tool_call_id.clone(),
        });
    } else if message.role == "system" {
        if let Some(text) = message
            .content
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            events.push(SessionEvent::Message {
                role: MessageRole::System,
                text: text.into(),
            });
        }
    }
    events
}

fn usage_delta(current: &UsageTotals, prior: &UsageTotals) -> Option<SessionEvent> {
    let input = current.input.saturating_sub(prior.input);
    let cache_read = current.cache_read.saturating_sub(prior.cache_read);
    let cache_write = current.cache_write.saturating_sub(prior.cache_write);
    let output = current
        .output
        .saturating_sub(prior.output)
        .saturating_add(current.reasoning.saturating_sub(prior.reasoning));
    let usd = (current.usd - prior.usd).max(0.0);
    (input > 0 || cache_read > 0 || cache_write > 0 || output > 0 || usd > 0.0).then(|| {
        SessionEvent::Cost {
            usd,
            tokens_in: input.saturating_add(cache_read).saturating_add(cache_write),
            tokens_out: output,
            tokens_cached: cache_read,
        }
    })
}

fn spawn_db_watcher(setup: WatcherSetup, emit: EventEmitter) {
    tokio::spawn(async move {
        let WatcherSetup {
            db_path,
            source,
            initial_id,
            initial_model,
        } = setup;
        let resuming = initial_id.is_some();
        let mut current_id = initial_id;
        let mut last_model = initial_model;
        let mut last_effort: Option<String> = None;
        let mut usage = UsageTotals::default();
        let mut message_id = 0i64;
        let mut initialized = false;
        let mut timer = tokio::time::interval(Duration::from_millis(500));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            timer.tick().await;
            let Some(conn) = open_db(&db_path) else {
                continue;
            };
            let Some(session) = read_session(&conn, &source, current_id.as_deref()) else {
                // A `/new` rotates away from current_id. Once bound, also
                // check the unique source tag for a newer live row.
                if current_id.is_some() {
                    if let Some(next) = read_session(&conn, &source, None) {
                        if current_id.as_deref() != Some(next.id.as_str()) {
                            bind_session(
                                &conn,
                                next,
                                &mut current_id,
                                &mut last_model,
                                &mut last_effort,
                                &mut usage,
                                &mut message_id,
                                false,
                                &emit,
                            );
                        }
                    }
                }
                continue;
            };

            if current_id.as_deref() != Some(session.id.as_str()) || !initialized {
                bind_session(
                    &conn,
                    session,
                    &mut current_id,
                    &mut last_model,
                    &mut last_effort,
                    &mut usage,
                    &mut message_id,
                    resuming && !initialized,
                    &emit,
                );
                initialized = true;
                continue;
            }

            // Source lookup detects `/new` and compression id rotation even
            // while the prior row remains readable.
            if let Some(newest) = read_session(&conn, &source, None) {
                if newest.id != current_id.as_deref().unwrap_or_default() {
                    bind_session(
                        &conn,
                        newest,
                        &mut current_id,
                        &mut last_model,
                        &mut last_effort,
                        &mut usage,
                        &mut message_id,
                        false,
                        &emit,
                    );
                    continue;
                }
            }

            let Some(session) = read_session(&conn, &source, current_id.as_deref()) else {
                continue;
            };
            if let Some(model) = session
                .model
                .as_ref()
                .filter(|model| !model.is_empty() && last_model.as_deref() != Some(model.as_str()))
            {
                last_model = Some(model.clone());
                emit.emit(SessionEvent::ModelChanged {
                    model: model.clone(),
                });
            }
            if let Some(effort) = session.effort.as_ref().filter(|effort| {
                !effort.is_empty() && last_effort.as_deref() != Some(effort.as_str())
            }) {
                last_effort = Some(effort.clone());
                emit.emit(SessionEvent::EffortChanged {
                    effort: effort.clone(),
                });
            }
            for message in read_messages(&conn, &session.id, message_id) {
                message_id = message.id;
                for event in message_events(&message) {
                    emit.emit(event);
                }
            }
            if let Some(event) = usage_delta(&session.usage, &usage) {
                emit.emit(event);
            }
            usage = session.usage;
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn bind_session(
    conn: &Connection,
    session: NativeSession,
    current_id: &mut Option<String>,
    last_model: &mut Option<String>,
    last_effort: &mut Option<String>,
    usage: &mut UsageTotals,
    message_id: &mut i64,
    skip_existing: bool,
    emit: &EventEmitter,
) {
    if let Some(prior) = current_id.as_ref().filter(|prior| *prior != &session.id) {
        emit.emit(SessionEvent::NativeIdChanged {
            prior_native_id: prior.clone(),
            new_native_id: session.id.clone(),
        });
    }
    write_native_id(&session.id);
    *message_id = if skip_existing {
        max_message_id(conn, &session.id)
    } else {
        0
    };
    *usage = if skip_existing {
        session.usage.clone()
    } else {
        UsageTotals::default()
    };
    if let Some(model) = session
        .model
        .as_ref()
        .filter(|model| !model.is_empty() && last_model.as_deref() != Some(model.as_str()))
    {
        emit.emit(SessionEvent::ModelChanged {
            model: model.clone(),
        });
    }
    if let Some(effort) = session
        .effort
        .as_ref()
        .filter(|effort| !effort.is_empty() && last_effort.as_deref() != Some(effort.as_str()))
    {
        emit.emit(SessionEvent::EffortChanged {
            effort: effort.clone(),
        });
    }
    *last_model = session.model.clone().or_else(|| last_model.clone());
    *last_effort = session.effort.clone().or_else(|| last_effort.clone());
    *current_id = Some(session.id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_modes_using_the_shared_wrapper_contract() {
        let mut params = SessionStartParams {
            session_id: "s1".into(),
            cwd: "/tmp".into(),
            prompt: None,
            model: None,
            mode: None,
            pty_size: None,
            env: Default::default(),
            args: vec![],
        };
        assert_eq!(resolve_mode(&params), Mode::Headless);
        params.pty_size = Some(PtySize { cols: 80, rows: 24 });
        assert_eq!(resolve_mode(&params), Mode::Interactive);
        params.mode = Some("headless".into());
        assert_eq!(resolve_mode(&params), Mode::Headless);
    }

    #[test]
    fn validates_real_hermes_session_id_shape() {
        assert!(valid_session_id("20260721_200441_18c475"));
        assert!(!valid_session_id("session_abc"));
        assert!(!valid_session_id("20260721_200441_not-hex"));
    }

    #[test]
    fn parses_reasoning_effort_shapes() {
        assert_eq!(
            reasoning_effort(r#"{"reasoning_config":"high"}"#).as_deref(),
            Some("high")
        );
        assert_eq!(
            reasoning_effort(r#"{"reasoning_config":{"effort":"medium"}}"#).as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn converts_real_hermes_tool_call_shape() {
        let message = HermesMessage {
            id: 1,
            role: "assistant".into(),
            content: Some("Checking.".into()),
            tool_call_id: None,
            tool_calls: Some(
                r#"[{"id":"call_1","type":"function","function":{"name":"terminal","arguments":"{\"command\":\"pwd\"}"}}]"#
                    .into(),
            ),
            tool_name: None,
            effect_disposition: None,
            reasoning: Some("Need the cwd.".into()),
            reasoning_content: None,
        };
        let events = message_events(&message);
        assert!(matches!(events[0], SessionEvent::Reasoning { .. }));
        assert!(matches!(events[1], SessionEvent::Message { .. }));
        match &events[2] {
            SessionEvent::ToolUse {
                tool,
                args,
                call_id,
            } => {
                assert_eq!(tool, "terminal");
                assert_eq!(args["command"], "pwd");
                assert_eq!(call_id.as_deref(), Some("call_1"));
            }
            other => panic!("expected tool use, got {other:?}"),
        }
    }

    #[test]
    fn usage_delta_preserves_prompt_split_and_reasoning_output() {
        let prior = UsageTotals {
            input: 100,
            output: 10,
            cache_read: 50,
            cache_write: 5,
            reasoning: 2,
            usd: 0.2,
        };
        let current = UsageTotals {
            input: 120,
            output: 15,
            cache_read: 80,
            cache_write: 7,
            reasoning: 5,
            usd: 0.3,
        };
        match usage_delta(&current, &prior).unwrap() {
            SessionEvent::Cost {
                usd,
                tokens_in,
                tokens_out,
                tokens_cached,
            } => {
                assert!((usd - 0.1).abs() < 0.00001);
                assert_eq!(tokens_in, 52);
                assert_eq!(tokens_out, 8);
                assert_eq!(tokens_cached, 30);
            }
            other => panic!("expected cost, got {other:?}"),
        }
    }

    #[test]
    fn database_user_rows_do_not_duplicate_construct_input_events() {
        let message = HermesMessage {
            id: 1,
            role: "user".into(),
            content: Some("hello".into()),
            tool_call_id: None,
            tool_calls: None,
            tool_name: None,
            effect_disposition: None,
            reasoning: None,
            reasoning_content: None,
        };
        assert!(message_events(&message).is_empty());
    }

    #[test]
    fn reads_session_and_messages_from_real_schema_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("state.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                id TEXT PRIMARY KEY, source TEXT, model TEXT, model_config TEXT,
                started_at REAL, input_tokens INTEGER, output_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER,
                reasoning_tokens INTEGER, actual_cost_usd REAL,
                estimated_cost_usd REAL
             );
             CREATE TABLE messages (
                id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT,
                tool_call_id TEXT, tool_calls TEXT, tool_name TEXT,
                effect_disposition TEXT, reasoning TEXT, reasoning_content TEXT,
                active INTEGER
             );
             INSERT INTO sessions VALUES (
                '20260721_200441_18c475', 'construct:s1', 'openai/gpt-5',
                '{\"reasoning_config\":\"high\"}', 1, 10, 2, 3, 1, 4, NULL, 0.25
             );
             INSERT INTO messages VALUES (
                7, '20260721_200441_18c475', 'assistant', 'done', NULL, NULL,
                NULL, NULL, 'thinking', NULL, 1
             );",
        )
        .unwrap();
        let session = read_session(&conn, "construct:s1", None).unwrap();
        assert_eq!(session.model.as_deref(), Some("openai/gpt-5"));
        assert_eq!(session.effort.as_deref(), Some("high"));
        assert_eq!(session.usage.cache_read, 3);
        let messages = read_messages(&conn, &session.id, 0);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id, 7);
    }
}
