//! Daemon-side adapter handle. Spawns the adapter binary and exchanges
//! JSON-RPC over its stdio.

use construct_protocol::jsonrpc::{self, MessageKind, Response};
use construct_protocol::{
    ahp_method, ahp_notif, transport, ErrorObject, EventEnvelope, InitializeParams,
    InitializeResult, Notification, Request, AHP_VERSION,
};
use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

type RpcResult = Result<serde_json::Value, ErrorObject>;

/// Messages the adapter wrapper forwards to the session manager.
#[derive(Debug)]
pub enum AdapterMessage {
    Event(EventEnvelope),
    Log {
        session_id: Option<String>,
        line: String,
    },
    Closed {
        exit_code: Option<i32>,
    },
}

/// Handle for one running adapter process. Cloning is intentionally
/// disallowed; share via `Arc`.
pub struct Adapter {
    pub name: String,
    pid: Option<u32>,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>,
    next_id: AtomicU64,
}

impl Adapter {
    /// Spawn the adapter binary and complete the `initialize` handshake.
    /// Notifications and the final `Closed` message are pushed to `message_tx`.
    #[allow(dead_code)]
    pub async fn spawn(
        name: String,
        binary: PathBuf,
        args: Vec<String>,
        env: HashMap<String, String>,
        message_tx: mpsc::Sender<AdapterMessage>,
    ) -> Result<(Arc<Self>, InitializeResult)> {
        let mut cmd = Command::new(&binary);
        cmd.args(&args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn adapter {}", binary.display()))?;

        let pid = child.id();
        let stdin = child.stdin.take().context("missing child stdin")?;
        let stdout = child.stdout.take().context("missing child stdout")?;
        let stderr = child.stderr.take().context("missing child stderr")?;

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Writer task: serialize outgoing messages to the adapter's stdin.
        let writer = {
            let name = name.clone();
            tokio::spawn(async move {
                let mut stdin = stdin;
                while let Some(v) = out_rx.recv().await {
                    if transport::write_message(&mut stdin, &v).await.is_err() {
                        break;
                    }
                }
                let _ = stdin.shutdown().await;
                tracing::debug!(%name, "adapter writer task exited");
            })
        };

        // Stderr drain: forward as log messages.
        {
            let tx = message_tx.clone();
            tokio::spawn(async move {
                let mut buf = BufReader::new(stderr);
                use tokio::io::AsyncBufReadExt;
                let mut line = String::new();
                loop {
                    line.clear();
                    match buf.read_line(&mut line).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                            if trimmed.is_empty() {
                                continue;
                            }
                            let _ = tx
                                .send(AdapterMessage::Log {
                                    session_id: None,
                                    line: trimmed,
                                })
                                .await;
                        }
                    }
                }
            });
        }

        // Reader task: stdout → classify → dispatch.
        {
            let pending = pending.clone();
            let tx = message_tx.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    let raw = match transport::read_message(&mut reader).await {
                        Ok(Some(v)) => v,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(%name, error = %e, "adapter read error");
                            continue;
                        }
                    };
                    match jsonrpc::classify(&raw) {
                        Some(MessageKind::Response) => {
                            let resp: Response = match serde_json::from_value(raw) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            let id_num = resp.id.as_u64().unwrap_or(u64::MAX);
                            let waiter = pending.lock().unwrap().remove(&id_num);
                            if let Some(tx) = waiter {
                                let payload = if let Some(err) = resp.error {
                                    Err(err)
                                } else {
                                    Ok(resp.result.unwrap_or(serde_json::Value::Null))
                                };
                                let _ = tx.send(payload);
                            }
                        }
                        Some(MessageKind::Notification) => {
                            let n: Notification = match serde_json::from_value(raw) {
                                Ok(n) => n,
                                Err(_) => continue,
                            };
                            match n.method.as_str() {
                                m if m == ahp_notif::EVENT => {
                                    if let Some(p) = n.params {
                                        match serde_json::from_value::<EventEnvelope>(p) {
                                            Ok(env) => {
                                                let _ = tx.send(AdapterMessage::Event(env)).await;
                                            }
                                            Err(e) => {
                                                tracing::warn!(%name, error = %e, "bad event")
                                            }
                                        }
                                    }
                                }
                                m if m == ahp_notif::LOG => {
                                    let session_id = n
                                        .params
                                        .as_ref()
                                        .and_then(|p| p.get("session_id"))
                                        .and_then(|v| v.as_str())
                                        .map(String::from);
                                    let line = n
                                        .params
                                        .as_ref()
                                        .and_then(|p| p.get("line"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let _ = tx.send(AdapterMessage::Log { session_id, line }).await;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                tracing::debug!(%name, "adapter reader task exited");
            });
        }

        // Wait task: when the child exits, signal Closed and drain pending.
        {
            let pending = pending.clone();
            let tx = message_tx.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                let exit_code = status.ok().and_then(|s| s.code());
                {
                    let mut pending = pending.lock().unwrap();
                    for (_, waiter) in pending.drain() {
                        let _ = waiter.send(Err(ErrorObject::new(
                            construct_protocol::jsonrpc::error_codes::ADAPTER_FAILED,
                            "adapter exited before responding",
                        )));
                    }
                }
                let _ = tx.send(AdapterMessage::Closed { exit_code }).await;
                tracing::info!(%name, ?exit_code, "adapter process closed");
                drop(writer);
            });
        }

        let adapter = Arc::new(Self {
            name: name.clone(),
            pid,
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
        });

        // Initialize handshake.
        let init_params = serde_json::to_value(&InitializeParams {
            protocol_version: AHP_VERSION.to_string(),
            client_info: construct_protocol::ClientInfo {
                name: "agentd".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })?;
        let result = adapter
            .request(ahp_method::INITIALIZE, init_params)
            .await
            .context("adapter initialize failed")?;
        let info: InitializeResult =
            serde_json::from_value(result).context("invalid initialize response")?;
        Ok((adapter, info))
    }

    /// Spawn an adapter in reconnectable socket mode. The child is not killed
    /// when this daemon process exits; a later daemon can attach to the same
    /// per-session socket and continue using the running adapter.
    pub async fn spawn_reconnectable(
        name: String,
        binary: PathBuf,
        args: Vec<String>,
        mut env: HashMap<String, String>,
        socket_path: PathBuf,
        message_tx: mpsc::Sender<AdapterMessage>,
    ) -> Result<(Arc<Self>, InitializeResult)> {
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::remove_file(&socket_path);
        env.insert(
            "CONSTRUCT_ADAPTER_SOCKET".to_string(),
            socket_path.to_string_lossy().to_string(),
        );

        let mut cmd = Command::new(&binary);
        cmd.args(&args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(false);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn adapter {}", binary.display()))?;
        let pid = child.id();
        let stderr = child.stderr.take().context("missing child stderr")?;

        drain_stderr(stderr, message_tx.clone());
        let stream = match connect_with_retry(&socket_path).await {
            Ok(stream) => stream,
            Err(e) => {
                let _ = child.kill().await;
                return Err(e);
            }
        };
        let adapter = Self::from_stream(name.clone(), pid, stream, message_tx.clone(), false).await;

        {
            let pending = adapter.pending.clone();
            let tx = message_tx.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let status = child.wait().await;
                let exit_code = status.ok().and_then(|s| s.code());
                fail_pending(&pending, "adapter exited before responding");
                let _ = tx.send(AdapterMessage::Closed { exit_code }).await;
                tracing::info!(%name, ?exit_code, "adapter process closed");
            });
        }

        let info = adapter.initialize().await?;
        Ok((adapter, info))
    }

    /// Attach to an adapter process that survived a previous daemon.
    pub async fn attach(
        name: String,
        socket_path: PathBuf,
        message_tx: mpsc::Sender<AdapterMessage>,
    ) -> Result<(Arc<Self>, InitializeResult)> {
        let stream = UnixStream::connect(&socket_path)
            .await
            .with_context(|| format!("connect adapter socket {}", socket_path.display()))?;
        let adapter = Self::from_stream(name, None, stream, message_tx, true).await;
        let info = adapter.initialize().await?;
        Ok((adapter, info))
    }

    async fn from_stream(
        name: String,
        pid: Option<u32>,
        stream: UnixStream,
        message_tx: mpsc::Sender<AdapterMessage>,
        notify_closed_on_reader_exit: bool,
    ) -> Arc<Self> {
        let (reader, writer) = stream.into_split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        {
            let name = name.clone();
            tokio::spawn(async move {
                let mut writer = writer;
                while let Some(v) = out_rx.recv().await {
                    if transport::write_message(&mut writer, &v).await.is_err() {
                        break;
                    }
                }
                let _ = writer.shutdown().await;
                tracing::debug!(%name, "adapter socket writer task exited");
            });
        }

        {
            let pending = pending.clone();
            let tx = message_tx.clone();
            let name = name.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(reader);
                loop {
                    let raw = match transport::read_message(&mut reader).await {
                        Ok(Some(v)) => v,
                        Ok(None) => break,
                        Err(e) => {
                            tracing::warn!(%name, error = %e, "adapter read error");
                            continue;
                        }
                    };
                    dispatch_adapter_message(&name, &pending, &tx, raw).await;
                }
                tracing::debug!(%name, "adapter socket reader task exited");
                fail_pending(&pending, "adapter connection closed before responding");
                if notify_closed_on_reader_exit {
                    let _ = tx.send(AdapterMessage::Closed { exit_code: None }).await;
                }
            });
        }

        Arc::new(Self {
            name,
            pid,
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
        })
    }

    async fn initialize(&self) -> Result<InitializeResult> {
        let init_params = serde_json::to_value(&InitializeParams {
            protocol_version: AHP_VERSION.to_string(),
            client_info: construct_protocol::ClientInfo {
                name: "agentd".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })?;
        let result = self
            .request(ahp_method::INITIALIZE, init_params)
            .await
            .context("adapter initialize failed")?;
        serde_json::from_value(result).context("invalid initialize response")
    }

    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<RpcResult>();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request::new(serde_json::json!(id), method.to_string(), Some(params));
        let v = serde_json::to_value(&req)?;
        self.out_tx
            .send(v)
            .map_err(|_| anyhow!("adapter writer channel closed"))?;
        let result = tokio::time::timeout(Duration::from_secs(60), rx)
            .await
            .with_context(|| format!("adapter {} timed out responding to {}", self.name, method))?;
        match result {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("adapter error: {}", e.message)),
            Err(_) => Err(anyhow!("adapter dropped response")),
        }
    }

    /// Send `session.stop` then drop the writer. Best-effort, won't error if
    /// the adapter is already gone.
    pub async fn shutdown(&self) {
        let _ = self
            .request(
                ahp_method::SHUTDOWN,
                serde_json::Value::Object(Default::default()),
            )
            .await;
    }

    /// Send SIGKILL to the adapter process if we still know its pid.
    pub fn kill(&self) {
        if let Some(pid) = self.pid {
            #[cfg(unix)]
            {
                use nix::sys::signal::{kill, Signal};
                use nix::unistd::Pid;
                let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
            }
        }
    }
}

fn drain_stderr(stderr: tokio::process::ChildStderr, message_tx: mpsc::Sender<AdapterMessage>) {
    tokio::spawn(async move {
        let mut buf = BufReader::new(stderr);
        use tokio::io::AsyncBufReadExt;
        let mut line = String::new();
        loop {
            line.clear();
            match buf.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let trimmed = line.trim_end_matches(['\n', '\r']).to_string();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let _ = message_tx
                        .send(AdapterMessage::Log {
                            session_id: None,
                            line: trimmed,
                        })
                        .await;
                }
            }
        }
    });
}

async fn connect_with_retry(socket_path: &PathBuf) -> Result<UnixStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(anyhow::Error::from(e)
                        .context(format!("connect adapter socket {}", socket_path.display())));
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }
}

fn fail_pending(
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>,
    message: &'static str,
) {
    let mut pending = pending.lock().unwrap();
    for (_, waiter) in pending.drain() {
        let _ = waiter.send(Err(ErrorObject::new(
            construct_protocol::jsonrpc::error_codes::ADAPTER_FAILED,
            message,
        )));
    }
}

async fn dispatch_adapter_message(
    name: &str,
    pending: &Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResult>>>>,
    tx: &mpsc::Sender<AdapterMessage>,
    raw: serde_json::Value,
) {
    match jsonrpc::classify(&raw) {
        Some(MessageKind::Response) => {
            let resp: Response = match serde_json::from_value(raw) {
                Ok(r) => r,
                Err(_) => return,
            };
            let id_num = resp.id.as_u64().unwrap_or(u64::MAX);
            let waiter = pending.lock().unwrap().remove(&id_num);
            if let Some(tx) = waiter {
                let payload = if let Some(err) = resp.error {
                    Err(err)
                } else {
                    Ok(resp.result.unwrap_or(serde_json::Value::Null))
                };
                let _ = tx.send(payload);
            }
        }
        Some(MessageKind::Notification) => {
            let n: Notification = match serde_json::from_value(raw) {
                Ok(n) => n,
                Err(_) => return,
            };
            match n.method.as_str() {
                m if m == ahp_notif::EVENT => {
                    if let Some(p) = n.params {
                        match serde_json::from_value::<EventEnvelope>(p) {
                            Ok(env) => {
                                let _ = tx.send(AdapterMessage::Event(env)).await;
                            }
                            Err(e) => tracing::warn!(%name, error = %e, "bad event"),
                        }
                    }
                }
                m if m == ahp_notif::LOG => {
                    let session_id = n
                        .params
                        .as_ref()
                        .and_then(|p| p.get("session_id"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let line = n
                        .params
                        .as_ref()
                        .and_then(|p| p.get("line"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = tx.send(AdapterMessage::Log { session_id, line }).await;
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// Resolve an adapter binary spec to a concrete path.
pub fn locate_binary(spec: &str) -> Option<PathBuf> {
    let p = PathBuf::from(spec);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(&p);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    which::which(spec).ok()
}
