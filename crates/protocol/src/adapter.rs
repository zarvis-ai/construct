//! Helper for writing an adapter binary.
//!
//! An adapter binary's `main()` reduces to:
//!
//! ```no_run
//! use agentd_protocol::adapter::{run, AdapterContext, AdapterInboxMsg};
//! use agentd_protocol::{Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let metadata = InitializeResult {
//!         name: "demo".into(),
//!         version: env!("CARGO_PKG_VERSION").into(),
//!         capabilities: Capabilities { supports_input: true, ..Default::default() },
//!     };
//!     run(metadata, |params, mut ctx| async move {
//!         ctx.emit.emit(SessionEvent::Status { state: SessionState::Running, detail: None });
//!         ctx.emit.emit(SessionEvent::Message {
//!             role: MessageRole::Assistant,
//!             text: format!("got prompt: {:?}", params.prompt),
//!         });
//!         while let Some(msg) = ctx.inbox.recv().await {
//!             match msg {
//!                 AdapterInboxMsg::Input(t) => ctx.emit.emit(SessionEvent::Message {
//!                     role: MessageRole::User, text: t,
//!                 }),
//!                 AdapterInboxMsg::Interrupt | AdapterInboxMsg::Stop => break,
//!                 _ => {} // ignore PTY traffic, tool decisions, approval-mode changes
//!             }
//!         }
//!         ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
//!     }).await
//! }
//! ```

use crate::jsonrpc::{self, MessageKind, Response};
use crate::{
    agent_context, ahp_method, ahp_notif, transport, ErrorObject, EventEnvelope, InitializeResult,
    Notification, Request, SessionEvent, SessionInputParams, SessionPtyInputParams,
    SessionPtyResizeParams, SessionStartParams,
};
use anyhow::{Context, Result};
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncWrite, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

#[cfg(feature = "pty")]
pub mod pty;

pub mod policy;

use crate::paths;

/// A command prefix supplied through an adapter's `CONSTRUCT_*_CMD` override.
///
/// The first token is the executable to spawn; remaining tokens are prepended
/// before the adapter's generated CLI arguments. This lets users configure
/// commands such as `exec codex` or `mise exec -- codex` without writing a
/// wrapper script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOverride {
    pub bin: String,
    pub args: Vec<String>,
}

impl CommandOverride {
    pub fn argv_preview(&self) -> String {
        std::iter::once(self.bin.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Resolve a child CLI command from either a full command override env var
/// (for example `CONSTRUCT_CODEX_CMD=exec codex`) or a binary-only fallback env
/// var (for example `CONSTRUCT_CODEX_BIN=/opt/bin/codex`).
///
/// The parser intentionally implements simple shell-like quoting for spaces,
/// single quotes, double quotes, and backslash escapes without evaluating a
/// shell. Invalid command overrides are ignored and the binary fallback is
/// used instead.
pub fn resolve_command_override(
    command_env: &str,
    binary_env: &str,
    default_bin: &str,
) -> CommandOverride {
    if let Ok(raw) = std::env::var(command_env) {
        if let Some(cmd) = parse_command_words(&raw) {
            return cmd;
        }
        eprintln!(
            "construct adapter: ignoring invalid {command_env}; falling back to {binary_env}/{default_bin}"
        );
    }
    CommandOverride {
        bin: std::env::var(binary_env).unwrap_or_else(|_| default_bin.to_string()),
        args: Vec::new(),
    }
}

fn parse_command_words(raw: &str) -> Option<CommandOverride> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut chars = raw.chars().peekable();
    let mut quote: Option<char> = None;
    let mut in_word = false;

    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                in_word = true;
            }
            Some('\'') => {
                cur.push(c);
                in_word = true;
            }
            Some(_) if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                } else {
                    cur.push(c);
                }
                in_word = true;
            }
            Some(_) => {
                cur.push(c);
                in_word = true;
            }
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                in_word = true;
            }
            None if c == '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                } else {
                    cur.push(c);
                }
                in_word = true;
            }
            None if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            None => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(cur);
    }
    let mut iter = words.into_iter();
    let bin = iter.next()?;
    if bin.is_empty() {
        return None;
    }
    Some(CommandOverride {
        bin,
        args: iter.collect(),
    })
}

/// Build a friendly "failed to start binary" message for adapters to emit
/// when spawning the agent CLI fails (e.g. binary not on PATH). Adapters
/// trust the user's shell to provide PATH; if that doesn't work, this
/// hint tells the user where to look.
pub fn missing_bin_hint(bin: &str, source: &std::io::Error) -> String {
    format!(
        "Failed to start `{bin}`: {source}.\n\
         Make sure `{bin}` is on PATH in the shell you started the construct \
         daemon from (try `which {bin}` there). If you use a version manager \
         (nvm, asdf, pyenv, …), activate it in your shell's startup file so \
         PATH is set before launching the daemon."
    )
}

/// Returns codex `-c key=value` flag pairs that register `construct-mcp` as a
/// session-scoped MCP server. Codex has no `--mcp-config` flag; MCP servers
/// live in `[mcp_servers.<name>]` in `config.toml`, and the per-invocation
/// override surface is `-c <dotted.key>=<toml-value>`.
///
/// The returned `Vec<String>` is appended to codex's argv (`-c`, `<value>`,
/// `-c`, `<value>`, ...). Empty when `CONSTRUCT_INJECT_MCP=0` or the
/// `construct-mcp` binary cannot be located.
pub fn maybe_inject_codex_mcp_args(session_id: &str) -> Vec<String> {
    if std::env::var("CONSTRUCT_INJECT_MCP").as_deref() == Ok("0") {
        return Vec::new();
    }
    let Some(bin) = paths::locate_sibling_binary("construct") else {
        return Vec::new();
    };
    let bin_lit = toml_quote(&bin.to_string_lossy());
    let env_lit = mcp_env_toml(session_id);
    let inline = format!("{{ command = {bin_lit}, args = [\"__mcp\"], env = {env_lit} }}");
    vec!["-c".into(), format!("mcp_servers.construct={inline}")]
}

fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn mcp_env_toml(session_id: &str) -> String {
    mcp_env_toml_from(session_id, |name| std::env::var(name).ok())
}

fn mcp_env_toml_from(session_id: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut pairs = vec![format!(
        "{} = {}",
        agent_context::ENV_SESSION_ID,
        toml_quote(session_id)
    )];
    for name in agent_context::MCP_CONTEXT_ENV_VARS {
        if let Some(value) = lookup(name) {
            pairs.push(format!("{name} = {}", toml_quote(&value)));
        }
    }
    format!("{{ {} }}", pairs.join(", "))
}

/// If `CONSTRUCT_INJECT_MCP` is not set to `"0"`, attempt to write a per-session
/// MCP config (under `state_dir/mcp/<session_id>.json`) that registers
/// `construct-mcp` as an MCP server. Returns the config path on success; pass
/// it to the child CLI via `--mcp-config <path>` (claude-style).
///
/// Used by the claude adapter. Codex uses
/// [`maybe_inject_codex_mcp_args`] instead.
pub fn maybe_inject_mcp_config(session_id: &str) -> Option<PathBuf> {
    if std::env::var("CONSTRUCT_INJECT_MCP").as_deref() == Ok("0") {
        return None;
    }
    let mcp_bin = paths::locate_sibling_binary("construct")?;
    let paths = paths::Paths::discover();
    let dir = paths.state_dir.join("mcp");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("construct MCP inject: mkdir {} failed: {e}", dir.display());
        return None;
    }
    let cfg_path = dir.join(format!("{session_id}.json"));
    let mut env = serde_json::Map::new();
    env.insert(
        agent_context::ENV_SESSION_ID.to_string(),
        serde_json::json!(session_id),
    );
    for name in agent_context::MCP_CONTEXT_ENV_VARS {
        if let Ok(value) = std::env::var(name) {
            env.insert(name.to_string(), serde_json::json!(value));
        }
    }
    let config = serde_json::json!({
        "mcpServers": {
            "construct": {
                "command": mcp_bin.to_string_lossy(),
                "args": ["__mcp"],
                "env": env,
            }
        }
    });
    let text = serde_json::to_string_pretty(&config).ok()?;
    if let Err(e) = std::fs::write(&cfg_path, text) {
        eprintln!(
            "construct MCP inject: write {} failed: {e}",
            cfg_path.display()
        );
        return None;
    }
    Some(cfg_path)
}

/// Messages the adapter runner can deliver into a running session task.
#[derive(Debug, Clone)]
pub enum AdapterInboxMsg {
    /// Daemon forwarded text from the user (line-oriented).
    Input(String),
    /// Raw bytes destined for the session's PTY master.
    PtyInput(Vec<u8>),
    /// Resize the session's PTY.
    PtyResize { cols: u16, rows: u16 },
    /// Daemon asks the adapter to interrupt the current operation.
    Interrupt,
    /// Daemon asks the adapter to wind down cleanly.
    Stop,
    /// User's decision for a pending tool-approval request emitted by the
    /// adapter. `decision` is the open string from
    /// [`crate::SessionToolDecisionParams`] — typically `"approve"`,
    /// `"deny"`, `"auto_review"`, or `"unsafe_auto"`.
    ToolDecision { call_id: String, decision: String },
    /// User changed the session's approval mode. The adapter updates its
    /// in-memory copy so the next classification respects the change.
    SetApprovalMode(crate::ApprovalMode),
    /// Client clicked `[kill]` / `[bg]` on a running tool block (or
    /// invoked the equivalent slash command / IPC method). The
    /// adapter looks up `call_id` in its in-flight task registry
    /// and applies the action. `action` is an open string —
    /// `"kill"` aborts the task, `"background"` moves it to the
    /// background pool.
    ToolAction { call_id: String, action: String },
}

/// Context handed to the user's session handler.
pub struct AdapterContext {
    pub session_id: String,
    pub emit: EventEmitter,
    pub inbox: mpsc::Receiver<AdapterInboxMsg>,
}

/// Clone-able sender for [`SessionEvent`]s. Drops events if the writer task
/// has exited (typically only on shutdown).
#[derive(Clone)]
pub struct EventEmitter {
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    session_id: String,
}

impl EventEmitter {
    /// Build an emitter wired to a fresh in-memory channel, returning the
    /// receiving end. Production emitters are constructed by the adapter
    /// runner; this is for tests and embedding that want to observe emitted
    /// notifications directly.
    pub fn channel(
        session_id: impl Into<String>,
    ) -> (Self, mpsc::UnboundedReceiver<serde_json::Value>) {
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (
            Self {
                out_tx,
                session_id: session_id.into(),
            },
            out_rx,
        )
    }

    pub fn emit(&self, event: SessionEvent) {
        let env = EventEnvelope {
            session_id: self.session_id.clone(),
            event,
        };
        let params = match serde_json::to_value(&env) {
            Ok(v) => v,
            Err(_) => return,
        };
        let notif = Notification::new(ahp_notif::EVENT, Some(params));
        if let Ok(v) = serde_json::to_value(&notif) {
            let _ = self.out_tx.send(v);
        }
    }

    /// Convenience: emit raw PTY bytes (base64-encoded by [`SessionEvent::pty`]).
    pub fn emit_pty(&self, bytes: &[u8]) {
        self.emit(SessionEvent::pty(bytes));
    }

    /// Emit a free-form log line for the daemon's log.
    pub fn log(&self, line: impl Into<String>) {
        let notif = Notification::new(
            ahp_notif::LOG,
            Some(serde_json::json!({
                "session_id": self.session_id,
                "line": line.into(),
            })),
        );
        if let Ok(v) = serde_json::to_value(&notif) {
            let _ = self.out_tx.send(v);
        }
    }
}

/// Drive the adapter event loop. Reads JSON-RPC from stdin, writes responses
/// and notifications to stdout, dispatches a single session to `handler`.
///
/// The runner exits when the daemon sends `shutdown`, when `session.stop`
/// completes, or when stdin reaches EOF.
pub async fn run<F, Fut>(metadata: InitializeResult, handler: F) -> Result<()>
where
    F: FnOnce(SessionStartParams, AdapterContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(socket) = std::env::var_os("CONSTRUCT_ADAPTER_SOCKET") {
        return run_reconnectable(metadata, handler, PathBuf::from(socket)).await;
    }
    let reader = BufReader::new(tokio::io::stdin());
    let writer = tokio::io::stdout();
    run_with_io(metadata, handler, reader, writer).await
}

/// Socket-backed adapter runner used when `CONSTRUCT_ADAPTER_SOCKET` is set.
///
/// Unlike stdio mode, daemon disconnect is not adapter shutdown: the session
/// task keeps running, outgoing events are retained in memory, and a restarted
/// daemon can reconnect to the same adapter process.
pub async fn run_reconnectable<F, Fut>(
    metadata: InitializeResult,
    handler: F,
    socket_path: PathBuf,
) -> Result<()>
where
    F: FnOnce(SessionStartParams, AdapterContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind adapter socket {}", socket_path.display()))?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let mut handler = Some(handler);
    let mut inbox_tx: Option<mpsc::Sender<AdapterInboxMsg>> = None;
    let mut session_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut backlog = VecDeque::<serde_json::Value>::new();
    let mut should_exit = false;

    while !should_exit {
        let stream = tokio::select! {
            accept = listener.accept() => {
                match accept {
                    Ok((stream, _)) => stream,
                    Err(e) => {
                        tracing::warn!(error = %e, "adapter: reconnect accept failed");
                        continue;
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Some(v) => backlog.push_back(v),
                    None => break,
                }
                continue;
            }
            _ = wait_session_done(&mut session_handle), if session_handle.is_some() => {
                session_handle = None;
                break;
            }
        };

        let disconnected = run_reconnectable_connection(
            stream,
            &metadata,
            &mut handler,
            &mut inbox_tx,
            &mut session_handle,
            &event_tx,
            &mut event_rx,
            &mut backlog,
            &mut should_exit,
        )
        .await?;
        if disconnected {
            tracing::debug!("adapter: daemon disconnected; waiting for reconnect");
        }
    }

    if let Some(h) = session_handle.take() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
    }
    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_reconnectable_connection<F, Fut>(
    stream: UnixStream,
    metadata: &InitializeResult,
    handler: &mut Option<F>,
    inbox_tx: &mut Option<mpsc::Sender<AdapterInboxMsg>>,
    session_handle: &mut Option<tokio::task::JoinHandle<()>>,
    event_tx: &mpsc::UnboundedSender<serde_json::Value>,
    event_rx: &mut mpsc::UnboundedReceiver<serde_json::Value>,
    backlog: &mut VecDeque<serde_json::Value>,
    should_exit: &mut bool,
) -> Result<bool>
where
    F: FnOnce(SessionStartParams, AdapterContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    while let Some(v) = backlog.pop_front() {
        if transport::write_message(&mut write_half, &v).await.is_err() {
            backlog.push_front(v);
            return Ok(true);
        }
    }

    while !*should_exit {
        let raw_msg = {
            let read_fut = transport::read_message(&mut reader);
            tokio::pin!(read_fut);
            tokio::select! {
                biased;
                msg = &mut read_fut => Some(msg),
                event = event_rx.recv() => {
                    match event {
                        Some(v) => {
                            if transport::write_message(&mut write_half, &v).await.is_err() {
                                backlog.push_back(v);
                                return Ok(true);
                            }
                        }
                        None => return Ok(false),
                    }
                    continue;
                }
                _ = wait_session_done(session_handle), if session_handle.is_some() => {
                    *session_handle = None;
                    *should_exit = true;
                    continue;
                }
            }
        };
        let raw = match raw_msg {
            Some(Ok(Some(v))) => v,
            Some(Ok(None)) => return Ok(true),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "adapter: invalid input, ignoring");
                continue;
            }
            None => continue,
        };
        if !matches!(jsonrpc::classify(&raw), Some(MessageKind::Request)) {
            continue;
        }
        let req: Request = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let resp = handle_request(
            req,
            metadata,
            handler,
            inbox_tx,
            session_handle,
            event_tx,
            should_exit,
        )
        .await;
        if transport::write_message(&mut write_half, &resp)
            .await
            .is_err()
        {
            return Ok(true);
        }
    }

    Ok(false)
}

async fn wait_session_done(handle: &mut Option<tokio::task::JoinHandle<()>>) {
    if let Some(h) = handle.as_mut() {
        let _ = h.await;
    } else {
        std::future::pending::<()>().await;
    }
}

async fn handle_request<F, Fut>(
    req: Request,
    metadata: &InitializeResult,
    handler: &mut Option<F>,
    inbox_tx: &mut Option<mpsc::Sender<AdapterInboxMsg>>,
    session_handle: &mut Option<tokio::task::JoinHandle<()>>,
    out_tx: &mpsc::UnboundedSender<serde_json::Value>,
    should_exit: &mut bool,
) -> serde_json::Value
where
    F: FnOnce(SessionStartParams, AdapterContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let id = req.id.clone();
    let ok = |result: serde_json::Value| {
        serde_json::to_value(&Response::ok(id.clone(), result)).unwrap()
    };
    let err = |err: ErrorObject| serde_json::to_value(&Response::err(id.clone(), err)).unwrap();

    match req.method.as_str() {
        ahp_method::INITIALIZE => {
            ok(serde_json::to_value(metadata).unwrap_or(serde_json::Value::Null))
        }
        ahp_method::SESSION_START => {
            let params: SessionStartParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            let handler_fn = match handler.take() {
                Some(h) => h,
                None => return err(ErrorObject::invalid_request("session already started")),
            };
            let (tx, rx) = mpsc::channel::<AdapterInboxMsg>(64);
            *inbox_tx = Some(tx);
            let ctx = AdapterContext {
                session_id: params.session_id.clone(),
                emit: EventEmitter {
                    out_tx: out_tx.clone(),
                    session_id: params.session_id.clone(),
                },
                inbox: rx,
            };
            *session_handle = Some(tokio::spawn(handler_fn(params, ctx)));
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_INPUT => {
            let p: SessionInputParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            if let Some(tx) = inbox_tx {
                let _ = tx.send(AdapterInboxMsg::Input(p.text)).await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_PTY_INPUT => {
            let p: SessionPtyInputParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            let bytes = match p.decode() {
                Ok(b) => b,
                Err(e) => {
                    return err(ErrorObject::invalid_params(format!(
                        "pty_input base64 decode: {e}"
                    )))
                }
            };
            if let Some(tx) = inbox_tx {
                let _ = tx.send(AdapterInboxMsg::PtyInput(bytes)).await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_PTY_RESIZE => {
            let p: SessionPtyResizeParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            if let Some(tx) = inbox_tx {
                let _ = tx
                    .send(AdapterInboxMsg::PtyResize {
                        cols: p.cols,
                        rows: p.rows,
                    })
                    .await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_INTERRUPT => {
            if let Some(tx) = inbox_tx {
                let _ = tx.send(AdapterInboxMsg::Interrupt).await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_STOP => {
            if let Some(tx) = inbox_tx {
                let _ = tx.send(AdapterInboxMsg::Stop).await;
            }
            if let Some(h) = session_handle.take() {
                let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
            }
            *should_exit = true;
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_TOOL_DECISION => {
            let p: crate::SessionToolDecisionParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            if let Some(tx) = inbox_tx {
                let _ = tx
                    .send(AdapterInboxMsg::ToolDecision {
                        call_id: p.call_id,
                        decision: p.decision,
                    })
                    .await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_TOOL_ACTION => {
            let p: crate::SessionToolActionParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            if let Some(tx) = inbox_tx {
                let _ = tx
                    .send(AdapterInboxMsg::ToolAction {
                        call_id: p.call_id,
                        action: p.action,
                    })
                    .await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SESSION_SET_APPROVAL_MODE => {
            let p: crate::SessionSetApprovalModeParams =
                match req.params.clone().map(serde_json::from_value).transpose() {
                    Ok(Some(p)) => p,
                    Ok(None) => return err(ErrorObject::invalid_params("missing params")),
                    Err(e) => return err(ErrorObject::invalid_params(e.to_string())),
                };
            if let Some(tx) = inbox_tx {
                let _ = tx.send(AdapterInboxMsg::SetApprovalMode(p.mode)).await;
            }
            ok(serde_json::Value::Null)
        }
        ahp_method::SHUTDOWN => {
            *should_exit = true;
            ok(serde_json::Value::Null)
        }
        other => err(ErrorObject::method_not_found(other)),
    }
}

/// I/O-generic core of [`run`]. Split out so unit tests can drive the
/// adapter event loop over an in-memory duplex pipe (`tokio::io::duplex`)
/// instead of the real process stdio. The behavior is identical to
/// [`run`]; the only difference is where the JSON-RPC frames flow.
pub async fn run_with_io<R, W, F, Fut>(
    metadata: InitializeResult,
    handler: F,
    mut reader: R,
    mut writer: W,
) -> Result<()>
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
    F: FnOnce(SessionStartParams, AdapterContext) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // Writer task: serialize outgoing messages to the configured sink one
    // per line.
    let writer_task = tokio::spawn(async move {
        while let Some(v) = out_rx.recv().await {
            if transport::write_message(&mut writer, &v).await.is_err() {
                break;
            }
        }
    });

    let mut handler = Some(handler);
    let mut inbox_tx: Option<mpsc::Sender<AdapterInboxMsg>> = None;
    let mut session_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut should_exit = false;

    while !should_exit {
        // Race the AHP stdin loop against the running session
        // handle. If the session handler completes on its own
        // (e.g. smith hits EOF after a Ctrl-D), we MUST exit the
        // loop and let the process die — otherwise the inbox
        // receiver is gone but `inbox_tx` is still held by this
        // loop, every subsequent `pty_input` request silently
        // errors on `tx.send(...)`, the adapter still acks `Ok`
        // to the daemon, and from the user's seat typing is dead.
        // The daemon transitions state to Done only when the
        // adapter process actually exits.
        let raw_msg = {
            let read_fut = transport::read_message(&mut reader);
            let handle_done_fut = async {
                match session_handle.as_mut() {
                    Some(h) => {
                        let _ = h.await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(read_fut);
            tokio::pin!(handle_done_fut);
            tokio::select! {
                biased;
                msg = &mut read_fut => Some(msg),
                _ = &mut handle_done_fut => None,
            }
        };
        let raw = match raw_msg {
            Some(Ok(Some(v))) => v,
            Some(Ok(None)) => break,
            Some(Err(e)) => {
                tracing::warn!(error = %e, "adapter: invalid input, ignoring");
                continue;
            }
            None => {
                // Session handler returned — clear the handle so we
                // don't await it again, and exit the loop.
                session_handle = None;
                should_exit = true;
                continue;
            }
        };
        if !matches!(jsonrpc::classify(&raw), Some(MessageKind::Request)) {
            continue;
        }
        let req: Request = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let id = req.id.clone();
        let send_ok = |result: serde_json::Value| {
            let _ = out_tx.send(serde_json::to_value(&Response::ok(id.clone(), result)).unwrap());
        };
        let send_err = |err: ErrorObject| {
            let _ = out_tx.send(serde_json::to_value(&Response::err(id.clone(), err)).unwrap());
        };

        match req.method.as_str() {
            ahp_method::INITIALIZE => {
                send_ok(serde_json::to_value(&metadata).unwrap_or(serde_json::Value::Null));
            }
            ahp_method::SESSION_START => {
                let params: SessionStartParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                let handler_fn = match handler.take() {
                    Some(h) => h,
                    None => {
                        send_err(ErrorObject::invalid_request("session already started"));
                        continue;
                    }
                };
                let (tx, rx) = mpsc::channel::<AdapterInboxMsg>(64);
                inbox_tx = Some(tx);
                let ctx = AdapterContext {
                    session_id: params.session_id.clone(),
                    emit: EventEmitter {
                        out_tx: out_tx.clone(),
                        session_id: params.session_id.clone(),
                    },
                    inbox: rx,
                };
                let fut = handler_fn(params, ctx);
                session_handle = Some(tokio::spawn(fut));
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_INPUT => {
                let p: SessionInputParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                if let Some(tx) = &inbox_tx {
                    let _ = tx.send(AdapterInboxMsg::Input(p.text)).await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_PTY_INPUT => {
                let p: SessionPtyInputParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                let bytes = match p.decode() {
                    Ok(b) => b,
                    Err(e) => {
                        send_err(ErrorObject::invalid_params(format!(
                            "pty_input base64 decode: {e}"
                        )));
                        continue;
                    }
                };
                if let Some(tx) = &inbox_tx {
                    let _ = tx.send(AdapterInboxMsg::PtyInput(bytes)).await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_PTY_RESIZE => {
                let p: SessionPtyResizeParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                if let Some(tx) = &inbox_tx {
                    let _ = tx
                        .send(AdapterInboxMsg::PtyResize {
                            cols: p.cols,
                            rows: p.rows,
                        })
                        .await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_INTERRUPT => {
                if let Some(tx) = &inbox_tx {
                    let _ = tx.send(AdapterInboxMsg::Interrupt).await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_STOP => {
                if let Some(tx) = &inbox_tx {
                    let _ = tx.send(AdapterInboxMsg::Stop).await;
                }
                if let Some(h) = session_handle.take() {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), h).await;
                }
                send_ok(serde_json::Value::Null);
                should_exit = true;
            }
            ahp_method::SESSION_TOOL_DECISION => {
                let p: crate::SessionToolDecisionParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                if let Some(tx) = &inbox_tx {
                    let _ = tx
                        .send(AdapterInboxMsg::ToolDecision {
                            call_id: p.call_id,
                            decision: p.decision,
                        })
                        .await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_TOOL_ACTION => {
                let p: crate::SessionToolActionParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                if let Some(tx) = &inbox_tx {
                    let _ = tx
                        .send(AdapterInboxMsg::ToolAction {
                            call_id: p.call_id,
                            action: p.action,
                        })
                        .await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SESSION_SET_APPROVAL_MODE => {
                let p: crate::SessionSetApprovalModeParams =
                    match req.params.clone().map(serde_json::from_value).transpose() {
                        Ok(Some(p)) => p,
                        Ok(None) => {
                            send_err(ErrorObject::invalid_params("missing params"));
                            continue;
                        }
                        Err(e) => {
                            send_err(ErrorObject::invalid_params(e.to_string()));
                            continue;
                        }
                    };
                if let Some(tx) = &inbox_tx {
                    let _ = tx.send(AdapterInboxMsg::SetApprovalMode(p.mode)).await;
                }
                send_ok(serde_json::Value::Null);
            }
            ahp_method::SHUTDOWN => {
                send_ok(serde_json::Value::Null);
                should_exit = true;
            }
            other => {
                send_err(ErrorObject::method_not_found(other));
            }
        }
    }

    if let Some(h) = session_handle.take() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), h).await;
    }
    drop(out_tx);
    let _ = writer_task.await;
    Ok(())
}

#[allow(dead_code)]
fn _ensure_send<T: Send>() {}

// Suppress unused-import warning of `Context` in some configurations.
#[allow(dead_code)]
fn _unused_context() {
    let _: Result<()> = Err(anyhow::anyhow!("x")).context("y");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Capabilities, MessageRole, SessionIdParams, SessionState};
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;
    use tokio::sync::oneshot;

    fn test_metadata() -> InitializeResult {
        InitializeResult {
            name: "test".into(),
            version: "0.0.0".into(),
            capabilities: Capabilities {
                supports_input: true,
                ..Default::default()
            },
        }
    }

    #[test]
    fn parse_command_words_handles_shell_like_quotes() {
        let cmd = parse_command_words(r#"mise exec -- "codex beta" --flag='two words'"#)
            .expect("valid command");

        assert_eq!(cmd.bin, "mise");
        assert_eq!(
            cmd.args,
            vec!["exec", "--", "codex beta", "--flag=two words"]
        );
        assert_eq!(
            cmd.argv_preview(),
            "mise exec -- codex beta --flag=two words"
        );
    }

    #[test]
    fn parse_command_words_rejects_unclosed_quotes() {
        assert!(parse_command_words("exec 'codex").is_none());
        assert!(parse_command_words("   ").is_none());
    }

    #[test]
    fn mcp_env_toml_includes_memory_context_env() {
        let got = mcp_env_toml_from("s123", |name| match name {
            agent_context::ENV_GLOBAL_MEMORY_FILE => Some("/tmp/global.md".to_string()),
            agent_context::ENV_PROJECT_MEMORY_FILE => Some("/tmp/project.md".to_string()),
            agent_context::ENV_PROJECT_ID => Some("g123".to_string()),
            _ => None,
        });

        assert!(got.contains("CONSTRUCT_SESSION_ID = \"s123\""));
        assert!(got.contains("CONSTRUCT_GLOBAL_MEMORY_FILE = \"/tmp/global.md\""));
        assert!(got.contains("CONSTRUCT_PROJECT_MEMORY_FILE = \"/tmp/project.md\""));
        assert!(got.contains("CONSTRUCT_PROJECT_ID = \"g123\""));
    }

    /// Symptom-level repro for the stuck-smith-prompt bug. The user
    /// hit Ctrl-D in a smith session; `interactive::run` returned;
    /// but the AHP loop in `run_with_io` kept polling stdin and
    /// silently dropped every subsequent `pty_input` (the inbox
    /// receiver had been dropped with the handler future, so
    /// `tx.send(...)` errored and was ignored). The TUI typed into
    /// the void.
    ///
    /// Fix: race the AHP read loop against `session_handle` so the
    /// adapter exits the moment the handler is done. The daemon's
    /// wait task then sees `AdapterMessage::Closed` and transitions
    /// state to `Done` instead of leaving it at `AwaitingInput`.
    ///
    /// This test drives `run_with_io` over an in-memory duplex pipe.
    /// The handler returns as soon as `SESSION_START` lands; without
    /// the fix the run future blocks on stdin forever and the
    /// timeout below fires.
    #[tokio::test]
    async fn adapter_exits_when_session_handler_returns() {
        let (mut daemon_side, adapter_side) = tokio::io::duplex(8192);
        let adapter_reader = BufReader::new(adapter_side);

        let handler = |_params: SessionStartParams, ctx: AdapterContext| async move {
            // Emit a couple of events and return — mirrors what a
            // smith interactive loop does after Ctrl-D once it
            // learns to emit Done. The library-level fix doesn't
            // depend on the Done emission; it MUST exit when the
            // handler returns regardless.
            ctx.emit.emit(SessionEvent::Status {
                state: SessionState::Running,
                detail: None,
            });
            ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
            // drop ctx → drops inbox receiver → handler future ends.
        };

        let adapter_task = tokio::spawn(async move {
            run_with_io(test_metadata(), handler, adapter_reader, tokio::io::sink()).await
        });

        // Drive the protocol: INITIALIZE, then SESSION_START. The
        // SessionStartParams shape is permissive (most fields are
        // `#[serde(default)]`) so a minimal body is fine.
        let initialize = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": ahp_method::INITIALIZE,
            "params": {
                "protocol_version": "1",
                "client_info": {"name": "test", "version": "0"},
            },
        });
        let session_start = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": ahp_method::SESSION_START,
            "params": {
                "session_id": "s_test",
                "cwd": "/",
            },
        });
        for v in [&initialize, &session_start] {
            let mut buf = serde_json::to_string(v).unwrap();
            buf.push('\n');
            daemon_side.write_all(buf.as_bytes()).await.unwrap();
        }
        // Keep daemon_side alive (not dropped) so the adapter's
        // stdin doesn't see EOF — only the session-handle race
        // should drive the exit.
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), adapter_task)
            .await
            .expect(
                "adapter did not exit after session handler returned \
             — run_with_io kept blocking on stdin (zombie loop)",
            );
        let inner = result.expect("adapter task panicked");
        inner.expect("adapter returned Err");
        // daemon_side dropped here; not relied on for exit.
        drop(daemon_side);
    }

    #[tokio::test]
    async fn reconnectable_request_dispatch_forwards_session_input() {
        let (seen_tx, seen_rx) = oneshot::channel::<String>();
        let handler = move |_params: SessionStartParams, mut ctx: AdapterContext| async move {
            if let Some(AdapterInboxMsg::Input(text)) = ctx.inbox.recv().await {
                let _ = seen_tx.send(text);
            }
        };
        let (out_tx, _out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let mut handler = Some(handler);
        let mut inbox_tx = None;
        let mut session_handle = None;
        let mut should_exit = false;

        let start = Request::new(
            serde_json::json!(1),
            ahp_method::SESSION_START.to_string(),
            Some(
                serde_json::to_value(SessionStartParams {
                    session_id: "s_dispatch".to_string(),
                    cwd: "/".to_string(),
                    prompt: None,
                    model: None,
                    mode: None,
                    pty_size: None,
                    env: Default::default(),
                    args: Vec::new(),
                })
                .unwrap(),
            ),
        );
        let raw = handle_request(
            start,
            &test_metadata(),
            &mut handler,
            &mut inbox_tx,
            &mut session_handle,
            &out_tx,
            &mut should_exit,
        )
        .await;
        let resp: Response = serde_json::from_value(raw).unwrap();
        assert!(resp.error.is_none());

        let input = Request::new(
            serde_json::json!(2),
            ahp_method::SESSION_INPUT.to_string(),
            Some(
                serde_json::to_value(SessionInputParams {
                    session_id: "s_dispatch".to_string(),
                    text: "hello".to_string(),
                })
                .unwrap(),
            ),
        );
        let raw = handle_request(
            input,
            &test_metadata(),
            &mut handler,
            &mut inbox_tx,
            &mut session_handle,
            &out_tx,
            &mut should_exit,
        )
        .await;
        let resp: Response = serde_json::from_value(raw).unwrap();
        assert!(resp.error.is_none());
        assert_eq!(seen_rx.await.unwrap(), "hello");
        if let Some(handle) = session_handle.take() {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires Unix socket bind permission in the test sandbox"]
    async fn reconnectable_adapter_buffers_events_until_daemon_reconnects() {
        let socket_path = std::env::temp_dir().join(format!(
            "a{}{}.sock",
            std::process::id(),
            uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(4)
                .collect::<String>()
        ));
        let socket_for_task = socket_path.clone();
        let handler = |_params: SessionStartParams, mut ctx: AdapterContext| async move {
            ctx.emit.emit(SessionEvent::Status {
                state: SessionState::Running,
                detail: None,
            });
            tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            ctx.emit.emit(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "buffered while daemon was gone".to_string(),
            });
            while let Some(msg) = ctx.inbox.recv().await {
                if matches!(msg, AdapterInboxMsg::Stop) {
                    break;
                }
            }
            ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
        };

        let adapter_task = tokio::spawn(async move {
            run_reconnectable(test_metadata(), handler, socket_for_task).await
        });
        tokio::task::yield_now().await;
        if adapter_task.is_finished() {
            let result = adapter_task.await.expect("adapter task panicked");
            panic!("reconnectable adapter exited before the first connection: {result:?}");
        }

        let (reader, mut writer) = connect_test_socket(&socket_path).await.into_split();
        let mut reader = BufReader::new(reader);
        write_request(
            &mut writer,
            1,
            ahp_method::INITIALIZE,
            serde_json::json!({}),
        )
        .await;
        read_response(&mut reader, 1).await;
        write_request(
            &mut writer,
            2,
            ahp_method::SESSION_START,
            serde_json::to_value(SessionStartParams {
                session_id: "s_reconnect".to_string(),
                cwd: "/".to_string(),
                prompt: None,
                model: None,
                mode: None,
                pty_size: None,
                env: Default::default(),
                args: Vec::new(),
            })
            .unwrap(),
        )
        .await;
        read_response(&mut reader, 2).await;
        drop(writer);
        drop(reader);

        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        let (reader, mut writer) = connect_test_socket(&socket_path).await.into_split();
        let mut reader = BufReader::new(reader);
        let mut saw_buffered = false;
        for _ in 0..4 {
            let raw = transport::read_message(&mut reader)
                .await
                .unwrap()
                .expect("adapter closed before buffered event");
            if event_text(&raw).as_deref() == Some("buffered while daemon was gone") {
                saw_buffered = true;
                break;
            }
        }
        assert!(saw_buffered, "buffered event was not replayed on reconnect");

        write_request(
            &mut writer,
            3,
            ahp_method::SESSION_STOP,
            serde_json::to_value(SessionIdParams {
                session_id: "s_reconnect".to_string(),
            })
            .unwrap(),
        )
        .await;
        read_response(&mut reader, 3).await;

        let result = tokio::time::timeout(std::time::Duration::from_secs(3), adapter_task)
            .await
            .expect("reconnectable adapter did not exit after session.stop");
        result
            .expect("adapter task panicked")
            .expect("adapter returned Err");
        let _ = std::fs::remove_file(socket_path);
    }

    async fn connect_test_socket(path: &std::path::Path) -> UnixStream {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match UnixStream::connect(path).await {
                Ok(stream) => return stream,
                Err(e) => {
                    if tokio::time::Instant::now() >= deadline {
                        panic!("failed to connect test socket {}: {e}", path.display());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
        }
    }

    async fn write_request<W: AsyncWrite + Unpin>(
        writer: &mut W,
        id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let req = Request::new(serde_json::json!(id), method.to_string(), Some(params));
        let raw = serde_json::to_value(req).unwrap();
        transport::write_message(writer, &raw).await.unwrap();
    }

    async fn read_response<R: AsyncBufRead + Unpin>(reader: &mut R, expected_id: u64) {
        loop {
            let raw = transport::read_message(reader)
                .await
                .unwrap()
                .expect("adapter closed before response");
            if !matches!(jsonrpc::classify(&raw), Some(MessageKind::Response)) {
                continue;
            }
            let resp: Response = serde_json::from_value(raw).unwrap();
            if resp.id.as_u64() == Some(expected_id) {
                assert!(resp.error.is_none(), "response had error: {:?}", resp.error);
                return;
            }
        }
    }

    fn event_text(raw: &serde_json::Value) -> Option<String> {
        let notif: Notification = serde_json::from_value(raw.clone()).ok()?;
        if notif.method != ahp_notif::EVENT {
            return None;
        }
        let env: EventEnvelope = serde_json::from_value(notif.params?).ok()?;
        match env.event {
            SessionEvent::Message { text, .. } => Some(text),
            _ => None,
        }
    }
}
