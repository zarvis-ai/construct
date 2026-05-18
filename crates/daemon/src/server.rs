//! Unix-socket IPC server. Dispatches JSON-RPC requests to [`SessionManager`]
//! and forwards subscribed broadcast events back to the client.

use crate::session::{BroadcastMsg, SessionManager};
use agentd_protocol::jsonrpc::{self, MessageKind};
use agentd_protocol::{
    ipc_method, ipc_notif, transport, CreateSessionParams, ErrorObject, GroupCreateParams,
    GroupCreateResult, GroupDeleteParams, GroupMoveParams, GroupRenameParams,
    GroupSetCollapsedParams, Notification, PingResult, Request, Response, SessionIdParams,
    SessionInputParams, SessionMoveParams, SessionPtyInputParams, SessionPtyResizeParams,
    SessionSetAutomodeParams, SessionSetGroupParams, SessionSetPinnedParams,
    SessionSetTitleParams, SessionToolActionParams, SessionToolDecisionParams, SubscribeParams,
    TranscriptParams, IPC_VERSION,
};
use anyhow::Result;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, Mutex};

pub async fn serve(manager: Arc<SessionManager>, socket_path: PathBuf) -> Result<()> {
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&socket_path)?;
    tracing::info!(socket = %socket_path.display(), "listening");
    loop {
        let (stream, _) = listener.accept().await?;
        let manager = manager.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, manager).await {
                tracing::debug!(error = ?e, "connection closed with error");
            }
        });
    }
}

#[derive(Debug)]
enum SubCmd {
    Subscribe(Option<String>),
    Unsubscribe,
}

async fn handle_connection(stream: UnixStream, manager: Arc<SessionManager>) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let (sub_cmd_tx, sub_cmd_rx) = mpsc::channel::<SubCmd>(8);

    let sub_writer = writer.clone();
    let sub_manager = manager.clone();
    let sub_task = tokio::spawn(async move {
        run_subscription_loop(sub_manager, sub_writer, sub_cmd_rx).await;
    });

    let mut reader = BufReader::new(reader);
    loop {
        let raw = match transport::read_message(&mut reader).await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(e) => {
                tracing::warn!(error = %e, "client sent bad JSON");
                continue;
            }
        };
        if !matches!(jsonrpc::classify(&raw), Some(MessageKind::Request)) {
            continue;
        }
        let req: Request = match serde_json::from_value(raw) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "invalid request shape");
                continue;
            }
        };
        let resp = dispatch(&manager, &sub_cmd_tx, req).await;
        let v = match serde_json::to_value(&resp) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "serialize response failed");
                continue;
            }
        };
        let mut w = writer.lock().await;
        if transport::write_message(&mut *w, &v).await.is_err() {
            break;
        }
    }

    sub_task.abort();
    Ok(())
}

async fn run_subscription_loop(
    manager: Arc<SessionManager>,
    writer: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    mut cmd_rx: mpsc::Receiver<SubCmd>,
) {
    let mut sub_rx: Option<broadcast::Receiver<BroadcastMsg>> = None;
    let mut filter: Option<String> = None;

    loop {
        if let Some(rx) = sub_rx.as_mut() {
            tokio::select! {
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(SubCmd::Subscribe(f)) => {
                            filter = f;
                            sub_rx = Some(manager.subscribe());
                        }
                        Some(SubCmd::Unsubscribe) => {
                            sub_rx = None;
                            filter = None;
                        }
                        None => return,
                    }
                }
                msg = rx.recv() => {
                    match msg {
                        Ok(m) => {
                            forward_broadcast(&writer, &filter, m).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(skipped = n, "subscriber lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            sub_rx = None;
                        }
                    }
                }
            }
        } else {
            match cmd_rx.recv().await {
                Some(SubCmd::Subscribe(f)) => {
                    filter = f;
                    sub_rx = Some(manager.subscribe());
                }
                Some(SubCmd::Unsubscribe) => {}
                None => return,
            }
        }
    }
}

async fn forward_broadcast(
    writer: &Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    filter: &Option<String>,
    msg: BroadcastMsg,
) {
    if let Some(f) = filter {
        let matches = match &msg {
            BroadcastMsg::Event(e) => e.session_id == *f,
            BroadcastMsg::State(s) => s.session.id == *f,
            BroadcastMsg::Deleted(d) => d.session_id == *f,
            // Group notifications aren't session-specific; always forward.
            BroadcastMsg::GroupState(_) | BroadcastMsg::GroupDeleted(_) => true,
        };
        if !matches {
            return;
        }
    }
    let notif = match msg {
        BroadcastMsg::Event(e) => {
            let p = match serde_json::to_value(&e) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::EVENT, Some(p))
        }
        BroadcastMsg::State(s) => {
            let p = match serde_json::to_value(&s) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::STATE, Some(p))
        }
        BroadcastMsg::Deleted(d) => {
            let p = match serde_json::to_value(&d) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::DELETED, Some(p))
        }
        BroadcastMsg::GroupState(g) => {
            let p = match serde_json::to_value(&g) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::GROUP_STATE, Some(p))
        }
        BroadcastMsg::GroupDeleted(g) => {
            let p = match serde_json::to_value(&g) {
                Ok(v) => v,
                Err(_) => return,
            };
            Notification::new(ipc_notif::GROUP_DELETED, Some(p))
        }
    };
    let v = match serde_json::to_value(&notif) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut w = writer.lock().await;
    let _ = transport::write_message(&mut *w, &v).await;
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: Option<serde_json::Value>,
) -> Result<T, ErrorObject> {
    let v = params.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(v).map_err(|e| ErrorObject::invalid_params(e.to_string()))
}

async fn dispatch(
    manager: &Arc<SessionManager>,
    sub_cmd_tx: &mpsc::Sender<SubCmd>,
    req: Request,
) -> Response {
    let id = req.id.clone();
    macro_rules! ok {
        ($v:expr) => {
            match serde_json::to_value($v) {
                Ok(v) => Response::ok(id.clone(), v),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        };
    }
    macro_rules! params {
        ($t:ty) => {{
            match parse_params::<$t>(req.params.clone()) {
                Ok(p) => p,
                Err(e) => return Response::err(id.clone(), e),
            }
        }};
    }
    match req.method.as_str() {
        m if m == ipc_method::PING => ok!(&PingResult {
            pong: true,
            version: IPC_VERSION.to_string(),
        }),
        m if m == ipc_method::HARNESS_LIST => ok!(&manager.harnesses()),
        m if m == ipc_method::SESSION_LIST => ok!(&manager.list().await),
        m if m == ipc_method::SESSION_CREATE => {
            let p = params!(CreateSessionParams);
            match manager.create(p).await {
                Ok(sid) => Response::ok(id.clone(), json!({ "session_id": sid })),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_GET => {
            let p = params!(SessionIdParams);
            match manager.detail(&p.session_id).await {
                Ok(d) => ok!(&d),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_INPUT => {
            let p = params!(SessionInputParams);
            match manager.send_input(&p.session_id, p.text).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_INPUT => {
            let p = params!(SessionPtyInputParams);
            let bytes = match p.decode() {
                Ok(b) => b,
                Err(e) => return Response::err(id.clone(), ErrorObject::invalid_params(e.to_string())),
            };
            match manager.pty_input(&p.session_id, bytes).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_RESIZE => {
            let p = params!(SessionPtyResizeParams);
            match manager.pty_resize(&p.session_id, p.cols, p.rows).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_PTY_REPLAY => {
            let p = params!(SessionIdParams);
            match manager.pty_replay(&p.session_id).await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_INTERRUPT => {
            let p = params!(SessionIdParams);
            match manager.interrupt(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_STOP => {
            let p = params!(SessionIdParams);
            match manager.stop(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_KILL => {
            let p = params!(SessionIdParams);
            match manager.kill(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_DELETE => {
            let p = params!(SessionIdParams);
            match manager.delete(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_RESTART => {
            let p = params!(SessionIdParams);
            match manager.clone().restart(&p.session_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_PINNED => {
            let p = params!(SessionSetPinnedParams);
            match manager.set_pinned(&p.session_id, p.pinned).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_TITLE => {
            let p = params!(SessionSetTitleParams);
            match manager.set_title(&p.session_id, p.title).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_AUTOMODE => {
            let p = params!(SessionSetAutomodeParams);
            match manager.set_automode(&p.session_id, p.on).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TOOL_DECISION => {
            let p = params!(SessionToolDecisionParams);
            match manager
                .tool_decision(&p.session_id, p.call_id, p.decision)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TOOL_ACTION => {
            let p = params!(SessionToolActionParams);
            match manager
                .tool_action(&p.session_id, p.call_id, p.action)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_LIST_TASKS => {
            let p = params!(agentd_protocol::ListTasksParams);
            match manager.list_tasks(&p.session_id).await {
                Ok(tasks) => Response::ok(
                    id.clone(),
                    serde_json::to_value(agentd_protocol::ListTasksResult { tasks })
                        .unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_CREATE => {
            let p = params!(agentd_protocol::LoopCreateParams);
            match manager.loop_create(p).await {
                Ok(l) => Response::ok(
                    id.clone(),
                    serde_json::to_value(&l).unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_LIST => {
            let p = params!(agentd_protocol::LoopListParams);
            let loops = manager.loop_list(p.session_id.as_deref()).await;
            Response::ok(
                id.clone(),
                serde_json::to_value(agentd_protocol::LoopListResult { loops })
                    .unwrap_or(serde_json::Value::Null),
            )
        }
        m if m == ipc_method::LOOP_UPDATE => {
            let p = params!(agentd_protocol::LoopUpdateParams);
            match manager.loop_update(p).await {
                Ok(l) => Response::ok(
                    id.clone(),
                    serde_json::to_value(&l).unwrap_or(serde_json::Value::Null),
                ),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::LOOP_REMOVE => {
            let p = params!(agentd_protocol::LoopRemoveParams);
            match manager.loop_remove(&p.loop_id).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_MOVE => {
            let p = params!(SessionMoveParams);
            match manager.move_session(&p.session_id, p.direction).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_SET_GROUP => {
            let p = params!(SessionSetGroupParams);
            match manager
                .set_session_group(&p.session_id, p.group_id, p.position)
                .await
            {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_LIST => ok!(&manager.list_groups().await),
        m if m == ipc_method::GROUP_CREATE => {
            let p = params!(GroupCreateParams);
            match manager.create_group(p.name).await {
                Ok(gid) => ok!(&GroupCreateResult { group_id: gid }),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_RENAME => {
            let p = params!(GroupRenameParams);
            match manager.rename_group(&p.group_id, p.name).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_DELETE => {
            // Accept the new `GroupDeleteParams` shape (with optional
            // `delete_members`); older clients sending the bare
            // `{"group_id": "…"}` payload deserialize too because
            // `delete_members` is `#[serde(default)]`.
            let p = params!(GroupDeleteParams);
            match manager.delete_group(&p.group_id, p.delete_members).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_SET_COLLAPSED => {
            let p = params!(GroupSetCollapsedParams);
            match manager.set_group_collapsed(&p.group_id, p.collapsed).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::GROUP_MOVE => {
            let p = params!(GroupMoveParams);
            match manager.move_group(&p.group_id, p.direction).await {
                Ok(()) => Response::ok(id.clone(), serde_json::Value::Null),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_DIFF => {
            let p = params!(SessionIdParams);
            match manager.diff(&p.session_id).await {
                Ok(patch) => Response::ok(id.clone(), json!({ "patch": patch })),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SESSION_TRANSCRIPT => {
            let p = params!(TranscriptParams);
            match manager.transcript(&p.session_id, p.from, p.limit).await {
                Ok(r) => ok!(&r),
                Err(e) => Response::err(id.clone(), ErrorObject::internal(e.to_string())),
            }
        }
        m if m == ipc_method::SUBSCRIBE_EVENTS => {
            let p = params!(SubscribeParams);
            let _ = sub_cmd_tx.send(SubCmd::Subscribe(p.session_id)).await;
            Response::ok(id.clone(), serde_json::Value::Null)
        }
        m if m == ipc_method::UNSUBSCRIBE_EVENTS => {
            let _ = sub_cmd_tx.send(SubCmd::Unsubscribe).await;
            Response::ok(id.clone(), serde_json::Value::Null)
        }
        other => Response::err(id.clone(), ErrorObject::method_not_found(other)),
    }
}
