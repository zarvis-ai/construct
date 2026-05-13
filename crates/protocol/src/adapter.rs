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
//!             }
//!         }
//!         ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
//!     }).await
//! }
//! ```

use crate::jsonrpc::{self, MessageKind, Response};
use crate::{
    ahp_method, ahp_notif, transport, ErrorObject, EventEnvelope, InitializeResult, Notification,
    Request, SessionEvent, SessionInputParams, SessionPtyInputParams, SessionPtyResizeParams,
    SessionStartParams,
};
use anyhow::{Context, Result};
use std::future::Future;
use tokio::io::BufReader;
use tokio::sync::mpsc;

#[cfg(feature = "pty")]
pub mod pty;

use crate::paths;
use std::path::PathBuf;

/// If `AGENTD_INJECT_MCP` is not set to `"0"`, attempt to write a per-session
/// MCP config (under `state_dir/mcp/<session_id>.json`) that registers
/// `agentd-mcp` as an MCP server. Returns the config path on success; pass
/// it to the child CLI via `--mcp-config <path>`.
///
/// Used by the claude/codex adapters in interactive mode to let an agent
/// running inside an agentd session reach the daemon over MCP.
/// A shell snippet that loads nvm (if installed) and activates its
/// `default` Node version. Idempotent and silent: a no-op when there's no
/// nvm. The motivating case is `npm i -g`-installed agents (`codex`,
/// `claude`, etc.) that only resolve on PATH once `nvm use default` runs.
///
/// Built to be safe to prepend to any `bash -lc` / `zsh -lc` command.
/// Future extensions could chain in pyenv / asdf / mise here.
pub fn nvm_init_snippet() -> &'static str {
    // Prefer `nvm use node` (latest installed) over `default` — the
    // `default` alias is often pointed at a very old release (or unset),
    // while `node` is guaranteed to be a real installed version. Falls
    // back to `default` if `node` isn't installed.
    r#"export NVM_DIR="${NVM_DIR:-$HOME/.nvm}"; if [ -s "$NVM_DIR/nvm.sh" ]; then . "$NVM_DIR/nvm.sh" >/dev/null 2>&1; nvm use --silent node >/dev/null 2>&1 || nvm use --silent default >/dev/null 2>&1 || true; fi"#
}

/// Wrap a command so it's spawned through the user's login shell instead
/// of directly. Lets package managers that customize PATH in shell init
/// files (nvm, pyenv, asdf, conda, mise, …) take effect, so agents
/// installed via `npm i -g` under nvm-managed Node, etc., are reachable.
///
/// The shell is `$SHELL` (falling back to `/bin/bash`). The wrapper uses
/// `-lc '<nvm-init>; exec "$@"'` so:
/// - login init files are read (PATH, NVM_DIR, etc.)
/// - nvm's default Node is activated (no-op if no nvm)
/// - the shell process exec-replaces itself with the real command,
///   leaving the process tree as `adapter → <real-cmd>` (not
///   `adapter → shell → <real-cmd>`).
///
/// Opt out with `AGENTD_LOGIN_SHELL=0` (returns `(bin, args)` unchanged).
pub fn login_shell_wrap(bin: String, args: Vec<String>) -> (String, Vec<String>) {
    if std::env::var("AGENTD_LOGIN_SHELL").as_deref() == Ok("0") {
        return (bin, args);
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let cmd = format!("{}; exec \"$@\"", nvm_init_snippet());
    let mut wrapped = vec![
        "-lc".to_string(),
        cmd,
        // Conventional placeholder for $0 (the shell uses this as argv[0]
        // before the exec; the exec'd program gets its own from $@).
        "agentd-spawn".to_string(),
        bin,
    ];
    wrapped.extend(args);
    (shell, wrapped)
}

pub fn maybe_inject_mcp_config(session_id: &str) -> Option<PathBuf> {
    if std::env::var("AGENTD_INJECT_MCP").as_deref() == Ok("0") {
        return None;
    }
    let mcp_bin = paths::locate_sibling_binary("agentd-mcp")?;
    let paths = paths::Paths::discover();
    let dir = paths.state_dir.join("mcp");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "agentd MCP inject: mkdir {} failed: {e}",
            dir.display()
        );
        return None;
    }
    let cfg_path = dir.join(format!("{session_id}.json"));
    let config = serde_json::json!({
        "mcpServers": {
            "agentd": {
                "command": mcp_bin.to_string_lossy(),
                "args": [],
                "env": { "AGENTD_SESSION_ID": session_id },
            }
        }
    });
    let text = serde_json::to_string_pretty(&config).ok()?;
    if let Err(e) = std::fs::write(&cfg_path, text) {
        eprintln!(
            "agentd MCP inject: write {} failed: {e}",
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
    let stdin = tokio::io::stdin();
    let mut stdin = BufReader::new(stdin);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();

    // Writer task: serialize outgoing messages to stdout one per line.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(v) = out_rx.recv().await {
            if transport::write_message(&mut stdout, &v).await.is_err() {
                break;
            }
        }
    });

    let mut handler = Some(handler);
    let mut inbox_tx: Option<mpsc::Sender<AdapterInboxMsg>> = None;
    let mut session_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut should_exit = false;

    while !should_exit {
        let raw = match transport::read_message(&mut stdin).await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "adapter: invalid input, ignoring");
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
                let params: SessionStartParams = match req
                    .params
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                {
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
                let p: SessionInputParams = match req
                    .params
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                {
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
                let p: SessionPtyInputParams = match req
                    .params
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                {
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
                let p: SessionPtyResizeParams = match req
                    .params
                    .clone()
                    .map(serde_json::from_value)
                    .transpose()
                {
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
    let _ = writer.await;
    Ok(())
}

#[allow(dead_code)]
fn _ensure_send<T: Send>() {}

// Suppress unused-import warning of `Context` in some configurations.
#[allow(dead_code)]
fn _unused_context() {
    let _: Result<()> = Err(anyhow::anyhow!("x")).context("y");
}
