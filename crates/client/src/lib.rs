//! IPC client. JSON-RPC over a Unix socket to `agentd`.

use agentd_protocol::jsonrpc::{self, MessageKind};
use agentd_protocol::{
    ipc_method, transport, CreateSessionParams, DiffResult, ErrorObject, GroupCreateParams,
    GroupIdParams, GroupMoveParams, GroupRenameParams, GroupSetCollapsedParams, GroupSummary,
    HarnessInfo, MoveDirection, Notification, PingResult, PtyReplayResult, Request, Response,
    SessionDetail, SessionIdParams, SessionInputParams, SessionMoveParams, SessionPtyInputParams,
    SessionPtyResizeParams, SessionSetPinnedParams, SessionSetTitleParams, SessionSummary,
    SubscribeParams, TranscriptParams, TranscriptResult,
};
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot, Mutex};

type RpcResult = Result<serde_json::Value, ErrorObject>;

pub struct Client {
    out_tx: mpsc::UnboundedSender<serde_json::Value>,
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<RpcResult>>>>,
    next_id: AtomicU64,
    notif_rx: Mutex<Option<mpsc::UnboundedReceiver<Notification>>>,
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

        // writer task
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(v) = out_rx.recv().await {
                if transport::write_message(&mut writer, &v).await.is_err() {
                    break;
                }
            }
        });

        // reader task
        {
            let pending = pending.clone();
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
            });
        }

        Ok(Arc::new(Self {
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
            notif_rx: Mutex::new(Some(notif_rx)),
        }))
    }

    pub async fn take_notifications(&self) -> Option<mpsc::UnboundedReceiver<Notification>> {
        self.notif_rx.lock().await.take()
    }

    pub async fn request<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel::<RpcResult>();
        self.pending.lock().unwrap().insert(id, tx);
        let req = Request::new(
            serde_json::json!(id),
            method.to_string(),
            Some(serde_json::to_value(params)?),
        );
        self.out_tx
            .send(serde_json::to_value(&req)?)
            .map_err(|_| anyhow!("client writer closed"))?;
        let res = tokio::time::timeout(Duration::from_secs(120), rx).await??;
        match res {
            Ok(v) => Ok(serde_json::from_value(v)?),
            Err(e) => Err(anyhow!("daemon error: {}", e.message)),
        }
    }

    pub async fn ping(&self) -> Result<PingResult> {
        self.request(ipc_method::PING, &serde_json::Value::Null).await
    }
    pub async fn harnesses(&self) -> Result<Vec<HarnessInfo>> {
        self.request(ipc_method::HARNESS_LIST, &serde_json::Value::Null).await
    }
    pub async fn list(&self) -> Result<Vec<SessionSummary>> {
        self.request(ipc_method::SESSION_LIST, &serde_json::Value::Null).await
    }
    pub async fn get(&self, id: &str) -> Result<SessionDetail> {
        self.request(
            ipc_method::SESSION_GET,
            &SessionIdParams { session_id: id.to_string() },
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
            &SessionIdParams { session_id: id.to_string() },
        )
        .await
    }
    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_INTERRUPT,
                &SessionIdParams { session_id: id.to_string() },
            )
            .await?;
        Ok(())
    }
    pub async fn stop(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_STOP,
                &SessionIdParams { session_id: id.to_string() },
            )
            .await?;
        Ok(())
    }
    pub async fn kill(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_KILL,
                &SessionIdParams { session_id: id.to_string() },
            )
            .await?;
        Ok(())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_DELETE,
                &SessionIdParams { session_id: id.to_string() },
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
    pub async fn list_groups(&self) -> Result<Vec<GroupSummary>> {
        self.request(ipc_method::GROUP_LIST, &serde_json::Value::Null).await
    }
    pub async fn create_group(&self, name: &str) -> Result<String> {
        #[derive(serde::Deserialize)]
        struct R { group_id: String }
        let r: R = self
            .request(ipc_method::GROUP_CREATE, &GroupCreateParams { name: name.to_string() })
            .await?;
        Ok(r.group_id)
    }
    pub async fn rename_group(&self, id: &str, name: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_RENAME,
                &GroupRenameParams { group_id: id.to_string(), name: name.to_string() },
            )
            .await?;
        Ok(())
    }
    pub async fn delete_group(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_DELETE,
                &GroupIdParams { group_id: id.to_string() },
            )
            .await?;
        Ok(())
    }
    pub async fn set_group_collapsed(&self, id: &str, collapsed: bool) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_SET_COLLAPSED,
                &GroupSetCollapsedParams { group_id: id.to_string(), collapsed },
            )
            .await?;
        Ok(())
    }
    pub async fn move_group(&self, id: &str, direction: MoveDirection) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::GROUP_MOVE,
                &GroupMoveParams { group_id: id.to_string(), direction },
            )
            .await?;
        Ok(())
    }
    pub async fn diff(&self, id: &str) -> Result<DiffResult> {
        self.request(
            ipc_method::SESSION_DIFF,
            &SessionIdParams { session_id: id.to_string() },
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
            },
        )
        .await
    }
    pub async fn subscribe(&self, session_id: Option<String>) -> Result<()> {
        let _: serde_json::Value = self
            .request(ipc_method::SUBSCRIBE_EVENTS, &SubscribeParams { session_id })
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
