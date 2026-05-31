//! IPC client. JSON-RPC over a Unix socket to `agentd`.

use agentd_protocol::jsonrpc::{self, MessageKind};
use agentd_protocol::{
    ipc_method, transport, CreateSessionParams, DiffResult, ErrorObject, GroupCreateParams,
    GroupDeleteParams, GroupMoveParams, GroupRenameParams, GroupSetCollapsedParams, GroupSummary,
    HarnessInfo, MoveDirection, Notification, PingResult, ProjectCreateParams, ProjectCreateResult,
    ProjectDeleteParams, ProjectMoveParams, ProjectRenameParams, ProjectSetCollapsedParams,
    ProjectSummary, PtyReplayResult, Request, Response, SessionAttachClipboardParams,
    SessionAttachClipboardResult, SessionDetail, SessionEmitEventParams, SessionIdParams,
    SessionInputParams, SessionMoveParams, SessionPtyInputParams, SessionPtyResizeParams,
    SessionSetAutomodeParams, SessionSetPinnedParams, SessionSetProjectParams,
    SessionSetTitleParams, SessionSummary, SessionToolDecisionParams, SubscribeParams,
    TranscriptParams, TranscriptResult,
};
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

type RpcResult = Result<serde_json::Value, ErrorObject>;

pub struct Client {
    socket: PathBuf,
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>>,
    next_id: AtomicU64,
    notif_rx: Mutex<Option<mpsc::UnboundedReceiver<Notification>>>,
    /// Set by the reader / writer tasks when their socket I/O
    /// fails — i.e. when the daemon has gone away. `request()`
    /// checks this and short-circuits with a "daemon
    /// disconnected" error instead of inserting into `pending`
    /// and awaiting a response that will never come. Without
    /// this, every key the user pressed after a daemon crash
    /// would hang the TUI on the 120s response timeout.
    disconnected: Arc<AtomicBool>,
}

/// Mark the client as disconnected and immediately fail every
/// in-flight RPC. Called from both the reader task (whose exit
/// is the canonical signal "no responses will arrive") and the
/// writer task (whose exit means we can't send anything either).
/// Idempotent — second call's `pending.drain()` finds an empty
/// map.
fn mark_disconnected(
    disconnected: &AtomicBool,
    pending: &StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>,
) {
    disconnected.store(true, Ordering::SeqCst);
    let mut map = pending.lock().unwrap();
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(ErrorObject::internal("daemon disconnected")));
    }
}

impl Client {
    pub async fn connect(socket: &Path) -> Result<Arc<Self>> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connect {}", socket.display()))?;
        let (reader, writer) = stream.into_split();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (notif_tx, notif_rx) = mpsc::unbounded_channel::<Notification>();
        let pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>> =
            Arc::new(StdMutex::new(HashMap::new()));
        let disconnected = Arc::new(AtomicBool::new(false));

        // writer task
        {
            let disconnected = disconnected.clone();
            let pending = pending.clone();
            tokio::spawn(async move {
                let mut writer = writer;
                while let Some(v) = out_rx.recv().await {
                    if transport::write_message(&mut writer, &v).await.is_err() {
                        break;
                    }
                }
                mark_disconnected(&disconnected, &pending);
            });
        }

        // reader task
        {
            let pending = pending.clone();
            let disconnected = disconnected.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(reader);
                loop {
                    let raw = match transport::read_message(&mut reader).await {
                        Ok(Some(v)) => v,
                        _ => break,
                    };
                    match jsonrpc::classify(&raw) {
                        Some(MessageKind::Response) => {
                            let r: Response = match serde_json::from_value(raw) {
                                Ok(r) => r,
                                Err(_) => continue,
                            };
                            let id_num = r.id.as_u64().unwrap_or(u64::MAX);
                            let waiter = pending.lock().unwrap().remove(&id_num);
                            if let Some(tx) = waiter {
                                let payload = if let Some(err) = r.error {
                                    Err(err)
                                } else {
                                    Ok(r.result.unwrap_or(serde_json::Value::Null))
                                };
                                let _ = tx.send(payload);
                            }
                        }
                        Some(MessageKind::Notification) => {
                            if let Ok(n) = serde_json::from_value::<Notification>(raw) {
                                let _ = notif_tx.send(n);
                            }
                        }
                        _ => {}
                    }
                }
                // Daemon closed the socket (clean EOF or I/O
                // error). Drop the notif sender so the consumer
                // sees `None` on its next `recv` (the TUI's
                // notification loop relies on that to set
                // `connected=false`), and fail every pending RPC
                // so the UI thread doesn't hang on responses
                // that will never come.
                drop(notif_tx);
                mark_disconnected(&disconnected, &pending);
            });
        }

        Ok(Arc::new(Self {
            socket: socket.to_path_buf(),
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
            notif_rx: Mutex::new(Some(notif_rx)),
            disconnected,
        }))
    }

    /// Has the underlying I/O task detected a closed socket? When
    /// true, every `request()` call returns immediately with a
    /// disconnected error — no more 120s response-timeout hangs.
    /// Cheap to call (atomic load).
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::SeqCst)
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub async fn take_notifications(&self) -> Option<mpsc::UnboundedReceiver<Notification>> {
        self.notif_rx.lock().await.take()
    }

    pub async fn request<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        if self.disconnected.load(Ordering::SeqCst) {
            return Err(anyhow!("daemon disconnected"));
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<RpcResult>();
        self.pending.lock().unwrap().insert(id, tx);
        // Race: the I/O tasks might have transitioned to
        // disconnected between our check above and our insert.
        // Re-check now and clean up our pending entry if so,
        // otherwise `rx.await` would hang on a sender that the
        // mark_disconnected() drain already missed.
        if self.disconnected.load(Ordering::SeqCst) {
            self.pending.lock().unwrap().remove(&id);
            return Err(anyhow!("daemon disconnected"));
        }
        let req = Request::new(
            serde_json::json!(id),
            method.to_string(),
            Some(serde_json::to_value(params)?),
        );
        self.out_tx
            .send(serde_json::to_value(&req)?)
            .map_err(|_| anyhow!("daemon disconnected"))?;
        let res = tokio::time::timeout(Duration::from_secs(120), rx).await??;
        match res {
            Ok(v) => Ok(serde_json::from_value(v)?),
            Err(e) => Err(anyhow!("daemon error: {}", e.message)),
        }
    }

    pub async fn ping(&self) -> Result<PingResult> {
        self.request(ipc_method::PING, &serde_json::Value::Null)
            .await
    }
    pub async fn harnesses(&self) -> Result<Vec<HarnessInfo>> {
        self.request(ipc_method::HARNESS_LIST, &serde_json::Value::Null)
            .await
    }
    /// Start (or look up) the daemon's remote WS listener and
    /// return a QR + URL ready to display.
    ///
    /// `local_only=false` is the `/remote-control` path: blocks
    /// for up to ~15s while cloudflared publishes its
    /// `*.trycloudflare.com` URL and returns the public URL or a
    /// clear error.
    ///
    /// `local_only=true` is the `/remote-control-debug` path:
    /// returns the `ws://127.0.0.1:<port>` URL immediately and
    /// never spawns cloudflared.
    ///
    /// Idempotent — repeat calls return the same token + URL.
    pub async fn remote_start(
        &self,
        local_only: bool,
        password: Option<String>,
    ) -> Result<agentd_protocol::RemoteStartResult> {
        self.remote_start_with_wait(local_only, password, true)
            .await
    }
    pub async fn remote_start_with_wait(
        &self,
        local_only: bool,
        password: Option<String>,
        wait_for_tunnel: bool,
    ) -> Result<agentd_protocol::RemoteStartResult> {
        let params = agentd_protocol::RemoteStartParams {
            local_only,
            password,
            wait_for_tunnel,
        };
        self.request(ipc_method::REMOTE_START, &params).await
    }
    /// Tear down the remote WS listener + cloudflared tunnel.
    /// Idempotent — `was_running: false` is the natural state when
    /// stop is called without an active listener.
    pub async fn remote_stop(&self) -> Result<agentd_protocol::RemoteStopResult> {
        self.request(ipc_method::REMOTE_STOP, &serde_json::Value::Null)
            .await
    }
    /// Restart the daemon in place (exec self). The IPC connection
    /// is closed by the kernel during exec(), so the reply is the
    /// last thing this client sees — the call will likely error on
    /// the recv side with "broken pipe". Callers should treat any
    /// reply (Ok or `BrokenPipe`-style error) as "restart in flight"
    /// and re-attempt connect with backoff.
    /// `exe: Some(path)` execs that binary instead of the daemon's own
    /// (e.g. a freshly-built one); `None` re-execs in place.
    pub async fn daemon_restart(
        &self,
        exe: Option<String>,
    ) -> Result<agentd_protocol::DaemonRestartResult> {
        self.request(
            ipc_method::DAEMON_RESTART,
            &agentd_protocol::DaemonRestartParams { exe },
        )
        .await
    }
    /// Dev-only: point the daemon's web server at a directory of assets
    /// (or `None` to revert to embedded). No-op on release daemons.
    pub async fn dev_set_assets(
        &self,
        dir: Option<String>,
    ) -> Result<agentd_protocol::DevAssetsResult> {
        self.request(
            ipc_method::DEV_SET_ASSETS,
            &agentd_protocol::DevSetAssetsParams { dir },
        )
        .await
    }
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        self.request(ipc_method::SESSION_LIST, &serde_json::Value::Null)
            .await
    }
    pub async fn get(&self, id: &str) -> Result<SessionDetail> {
        self.request(
            ipc_method::SESSION_GET,
            &SessionIdParams {
                session_id: id.to_string(),
            },
        )
        .await
    }
    pub async fn create(&self, p: CreateSessionParams) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct R {
            session_id: String,
        }
        let r: R = self.request(ipc_method::SESSION_CREATE, &p).await?;
        Ok(r.session_id)
    }
    pub async fn send_input(&self, id: &str, text: String) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_INPUT,
                &SessionInputParams {
                    session_id: id.to_string(),
                    text,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn attach_clipboard(
        &self,
        id: &str,
        data: String,
        filename: Option<String>,
        mime: Option<String>,
    ) -> Result<SessionAttachClipboardResult> {
        self.request(
            ipc_method::SESSION_ATTACH_CLIPBOARD,
            &SessionAttachClipboardParams {
                session_id: id.to_string(),
                data,
                filename,
                mime,
            },
        )
        .await
    }
    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_PTY_INPUT,
                &SessionPtyInputParams::from_bytes(id, &bytes),
            )
            .await?;
        Ok(())
    }
    pub async fn pty_resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_PTY_RESIZE,
                &SessionPtyResizeParams {
                    session_id: id.to_string(),
                    cols,
                    rows,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn pty_replay(&self, id: &str) -> Result<PtyReplayResult> {
        self.request(
            ipc_method::SESSION_PTY_REPLAY,
            &SessionIdParams {
                session_id: id.to_string(),
            },
        )
        .await
    }
    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_INTERRUPT,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn stop(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_STOP,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn kill(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_KILL,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_DELETE,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn delete_widget(&self, session_id: &str, panel_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_WIDGET_DELETE,
                &agentd_protocol::SessionWidgetDeleteParams {
                    session_id: session_id.to_string(),
                    panel_id: panel_id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    /// Respawn a session's adapter (TUI restart-confirm flow). Used
    /// on a `Done` session to bring it back to life so the user can
    /// keep typing. The daemon launches the new adapter with
    /// `AGENTD_RESUME=1`.
    pub async fn restart(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_RESTART,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_PINNED,
                &SessionSetPinnedParams {
                    session_id: id.to_string(),
                    pinned,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn set_title(&self, id: &str, title: Option<String>) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_TITLE,
                &SessionSetTitleParams {
                    session_id: id.to_string(),
                    title,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn set_automode(&self, id: &str, on: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_AUTOMODE,
                &SessionSetAutomodeParams {
                    session_id: id.to_string(),
                    on,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn emit_event(&self, id: &str, event: agentd_protocol::SessionEvent) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_EMIT_EVENT,
                &SessionEmitEventParams {
                    session_id: id.to_string(),
                    event,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn tool_decision(
        &self,
        id: &str,
        call_id: impl Into<String>,
        decision: impl Into<String>,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_TOOL_DECISION,
                &SessionToolDecisionParams {
                    session_id: id.to_string(),
                    call_id: call_id.into(),
                    decision: decision.into(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn loop_create(
        &self,
        params: agentd_protocol::LoopCreateParams,
    ) -> Result<agentd_protocol::Loop> {
        self.request(ipc_method::LOOP_CREATE, &params).await
    }
    pub async fn loop_list(&self, session_id: Option<&str>) -> Result<Vec<agentd_protocol::Loop>> {
        let r: agentd_protocol::LoopListResult = self
            .request(
                ipc_method::LOOP_LIST,
                &agentd_protocol::LoopListParams {
                    session_id: session_id.map(|s| s.to_string()),
                },
            )
            .await?;
        Ok(r.loops)
    }
    pub async fn loop_update(
        &self,
        params: agentd_protocol::LoopUpdateParams,
    ) -> Result<agentd_protocol::Loop> {
        self.request(ipc_method::LOOP_UPDATE, &params).await
    }
    pub async fn loop_remove(&self, loop_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::LOOP_REMOVE,
                &agentd_protocol::LoopRemoveParams {
                    loop_id: loop_id.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    /// List the per-session task registry: running + backgrounded
    /// + recent terminal entries. Empty for sessions whose adapter
    /// doesn't emit `TaskStart` lifecycle events (claude / codex /
    /// shell today).
    pub async fn list_tasks(&self, id: &str) -> Result<Vec<agentd_protocol::TaskInfo>> {
        let r: agentd_protocol::ListTasksResult = self
            .request(
                ipc_method::SESSION_LIST_TASKS,
                &agentd_protocol::ListTasksParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(r.tasks)
    }

    /// Tell the adapter to act on a running tool call — `"kill"` to
    /// abort, `"background"` to detach and continue. Used by the
    /// TUI's `[bg]` / `[kill]` button click handlers; future
    /// orchestrator slash commands will use it too.
    pub async fn tool_action(
        &self,
        id: &str,
        call_id: impl Into<String>,
        action: impl Into<String>,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_TOOL_ACTION,
                &agentd_protocol::SessionToolActionParams {
                    session_id: id.to_string(),
                    call_id: call_id.into(),
                    action: action.into(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn move_session(&self, id: &str, direction: MoveDirection) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_MOVE,
                &SessionMoveParams {
                    session_id: id.to_string(),
                    direction,
                },
            )
            .await?;
        Ok(())
    }
    /// Change a session's group membership. `group_id: None` ungroups
    /// the session. `position` controls where in the target region
    /// the session lands (`Top` of the list or `Bottom`).
    pub async fn set_session_group(
        &self,
        id: &str,
        group_id: Option<String>,
        position: agentd_protocol::SessionGroupPosition,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_GROUP,
                &agentd_protocol::SessionSetGroupParams {
                    session_id: id.to_string(),
                    group_id,
                    position,
                },
            )
            .await?;
        Ok(())
    }
    /// Project-named alias for `set_session_group`.
    pub async fn set_session_project(
        &self,
        id: &str,
        project_id: Option<String>,
        position: agentd_protocol::SessionGroupPosition,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_PROJECT,
                &SessionSetProjectParams {
                    session_id: id.to_string(),
                    project_id,
                    position,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn list_groups(&self) -> Result<Vec<GroupSummary>> {
        self.request(ipc_method::GROUP_LIST, &serde_json::Value::Null)
            .await
    }
    pub async fn list_projects(&self) -> Result<Vec<ProjectSummary>> {
        self.request(ipc_method::PROJECT_LIST, &serde_json::Value::Null)
            .await
    }
    pub async fn create_group(&self, name: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct R {
            group_id: String,
        }
        let r: R = self
            .request(
                ipc_method::GROUP_CREATE,
                &GroupCreateParams {
                    name: name.to_string(),
                },
            )
            .await?;
        Ok(r.group_id)
    }
    pub async fn create_project(&self, name: &str) -> Result<String> {
        let r: ProjectCreateResult = self
            .request(
                ipc_method::PROJECT_CREATE,
                &ProjectCreateParams {
                    name: name.to_string(),
                },
            )
            .await?;
        Ok(r.project_id)
    }
    pub async fn rename_group(&self, id: &str, name: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_RENAME,
                &GroupRenameParams {
                    group_id: id.to_string(),
                    name: name.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn rename_project(&self, id: &str, name: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::PROJECT_RENAME,
                &ProjectRenameParams {
                    project_id: id.to_string(),
                    name: name.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    /// Delete a group. When `delete_members` is true the daemon
    /// cascade-deletes every member session (kills its adapter, removes
    /// its on-disk dir, tears down any worktree) before removing the
    /// group itself. When false (the previous behavior) members are
    /// orphaned: their `group_id` clears but the sessions survive.
    pub async fn delete_group(&self, id: &str, delete_members: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_DELETE,
                &GroupDeleteParams {
                    group_id: id.to_string(),
                    delete_members,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn delete_project(&self, id: &str, delete_members: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::PROJECT_DELETE,
                &ProjectDeleteParams {
                    project_id: id.to_string(),
                    delete_members,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn set_group_collapsed(&self, id: &str, collapsed: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_SET_COLLAPSED,
                &GroupSetCollapsedParams {
                    group_id: id.to_string(),
                    collapsed,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn set_project_collapsed(&self, id: &str, collapsed: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::PROJECT_SET_COLLAPSED,
                &ProjectSetCollapsedParams {
                    project_id: id.to_string(),
                    collapsed,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn move_group(&self, id: &str, direction: MoveDirection) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_MOVE,
                &GroupMoveParams {
                    group_id: id.to_string(),
                    direction,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn move_project(&self, id: &str, direction: MoveDirection) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::PROJECT_MOVE,
                &ProjectMoveParams {
                    project_id: id.to_string(),
                    direction,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn diff(&self, id: &str) -> Result<DiffResult> {
        self.request(
            ipc_method::SESSION_DIFF,
            &SessionIdParams {
                session_id: id.to_string(),
            },
        )
        .await
    }
    pub async fn transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
    ) -> Result<TranscriptResult> {
        self.request(
            ipc_method::SESSION_TRANSCRIPT,
            &TranscriptParams {
                session_id: id.to_string(),
                from,
                limit,
                tail: None,
            },
        )
        .await
    }
    pub async fn subscribe(&self, session_id: Option<String>) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SUBSCRIBE_EVENTS,
                &SubscribeParams { session_id },
            )
            .await?;
        Ok(())
    }
    pub async fn unsubscribe(&self) -> Result<()> {
        let _: serde_json::Value = self
            .request(ipc_method::UNSUBSCRIBE_EVENTS, &serde_json::Value::Null)
            .await?;
        Ok(())
    }
}
