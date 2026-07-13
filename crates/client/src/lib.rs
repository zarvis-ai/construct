//! IPC client. JSON-RPC over a Unix socket to `agentd`.

use construct_protocol::jsonrpc::{self, MessageKind};
use construct_protocol::{
    ipc_method, transport, ChatViewerActiveResult, ClientView, CreateSessionParams, DiffResult,
    ErrorObject, GroupCreateParams, GroupDeleteParams, GroupMoveParams, GroupRenameParams,
    GroupSetCollapsedParams, GroupSummary, HarnessInfo, MoveDirection, Notification, PingResult,
    ProgramCursorParams, ProgramCursorResult, ProgramEditParams, ProgramExecuteParams,
    ProgramExecuteResult, ProgramGetParams, ProgramGetResult, ProgramListTemplatesResult,
    ProgramListVerbsResult, ProgramUpdateActor, ProgramUpdateParams, ProgramUpdateResult,
    ProgramVerbExecuteParams, ProgramVerbExecuteResult, ProjectCreateParams,
    ProjectCreateResult, ProjectDeleteParams, ProjectMoveParams, ProjectRenameParams,
    ProjectSetCollapsedParams, ProjectSummary, PtyReplayResult, PtySize, Request, Response,
    SearchParams, SearchResult, SessionAttachClipboardParams, SessionAttachClipboardResult,
    SessionDetail, SessionEmitEventParams, SessionIdParams, SessionInputParams, SessionMoveParams,
    SessionPtyInputParams, SessionPtyResizeParams, SessionSetApprovalModeParams,
    SessionSetFocusedParams, SessionSetPinnedParams, SessionSetProjectParams,
    SessionSetTitleParams, SessionSetViewParams, SessionSummary, SessionToolDecisionParams,
    SetTerminalBackgroundParams, SmithAuthStatusResult, SmithSetAuthMethodParams,
    SmithSetAuthMethodResult, SubscribeParams, TranscriptParams, TranscriptResult,
    UsageQueryParams, UsageQueryResult,
};
use anyhow::{anyhow, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
        let mut list: Vec<HarnessInfo> = self.request(ipc_method::HARNESS_LIST, &serde_json::Value::Null)
            .await?;
        for h in &mut list {
            if h.name == "antigravity" {
                h.name = "agy".to_string();
            }
        }
        Ok(list)
    }
    pub async fn smith_auth_status(&self) -> Result<SmithAuthStatusResult> {
        self.request(ipc_method::SMITH_AUTH_STATUS, &serde_json::Value::Null)
            .await
    }
    pub async fn smith_set_auth_method(&self, method: &str) -> Result<SmithSetAuthMethodResult> {
        self.request(
            ipc_method::SMITH_SET_AUTH_METHOD,
            &SmithSetAuthMethodParams {
                method: method.to_string(),
            },
        )
        .await
    }
    /// Query (and optionally trigger a background refresh of) the cached
    /// usage-probe snapshot for `harness` (spec 0086). Never blocks on the
    /// probe itself — see [`UsageQueryResult`].
    pub async fn usage_query(&self, harness: &str, allow_refresh: bool) -> Result<UsageQueryResult> {
        self.request(
            ipc_method::USAGE_QUERY,
            &UsageQueryParams {
                harness: harness.to_string(),
                allow_refresh,
            },
        )
        .await
    }
    pub async fn program_get(&self, id: &str) -> Result<ProgramGetResult> {
        self.request(
            ipc_method::PROGRAM_GET,
            &ProgramGetParams {
                session_id: id.to_string(),
            },
        )
        .await
    }
    pub async fn program_update(&self, params: ProgramUpdateParams) -> Result<ProgramUpdateResult> {
        self.request(ipc_method::PROGRAM_UPDATE, &params).await
    }
    pub async fn program_edit(&self, params: ProgramEditParams) -> Result<ProgramUpdateResult> {
        self.request(ipc_method::PROGRAM_EDIT, &params).await
    }
    pub async fn program_cursor(&self, params: ProgramCursorParams) -> Result<ProgramCursorResult> {
        self.request(ipc_method::PROGRAM_CURSOR, &params).await
    }
    pub async fn program_execute(
        &self,
        params: ProgramExecuteParams,
    ) -> Result<ProgramExecuteResult> {
        self.request(ipc_method::PROGRAM_EXECUTE, &params).await
    }
    pub async fn program_templates(&self) -> Result<ProgramListTemplatesResult> {
        self.request(ipc_method::PROGRAM_LIST_TEMPLATES, &serde_json::Value::Null)
            .await
    }
    pub async fn program_verbs(&self) -> Result<ProgramListVerbsResult> {
        self.request(ipc_method::PROGRAM_LIST_VERBS, &serde_json::Value::Null)
            .await
    }
    pub async fn program_verb_execute(
        &self,
        params: ProgramVerbExecuteParams,
    ) -> Result<ProgramVerbExecuteResult> {
        self.request(ipc_method::PROGRAM_VERB_EXECUTE, &params)
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
    ) -> Result<construct_protocol::RemoteStartResult> {
        self.remote_start_with_wait(local_only, password, true)
            .await
    }
    pub async fn remote_start_with_wait(
        &self,
        local_only: bool,
        password: Option<String>,
        wait_for_tunnel: bool,
    ) -> Result<construct_protocol::RemoteStartResult> {
        let params = construct_protocol::RemoteStartParams {
            local_only,
            password,
            wait_for_tunnel,
        };
        self.request(ipc_method::REMOTE_START, &params).await
    }
    /// Tear down the remote WS listener + cloudflared tunnel.
    /// Idempotent — `was_running: false` is the natural state when
    /// stop is called without an active listener.
    pub async fn remote_stop(&self) -> Result<construct_protocol::RemoteStopResult> {
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
    /// `restart_sessions: true` also bounces every session's adapter so
    /// the new daemon respawns each one (and its MCP child) fresh,
    /// instead of letting the survivors reattach.
    pub async fn daemon_restart(
        &self,
        exe: Option<String>,
        restart_sessions: bool,
    ) -> Result<construct_protocol::DaemonRestartResult> {
        self.request(
            ipc_method::DAEMON_RESTART,
            &construct_protocol::DaemonRestartParams {
                exe,
                restart_sessions,
            },
        )
        .await
    }
    /// Stop the daemon gracefully: it stops every session's adapter
    /// (leaving sessions resumable on the next start) and exits. Like
    /// [`Self::daemon_restart`], the IPC connection closes as the
    /// process exits, so callers should treat a `BrokenPipe`-style
    /// error as "shutdown in flight".
    pub async fn daemon_shutdown(&self) -> Result<construct_protocol::DaemonShutdownResult> {
        self.request(
            ipc_method::DAEMON_SHUTDOWN,
            &construct_protocol::DaemonShutdownParams {},
        )
        .await
    }
    /// Dev-only: point the daemon's web server at a directory of assets
    /// (or `None` to revert to embedded). No-op on release daemons.
    pub async fn dev_set_assets(
        &self,
        dir: Option<String>,
    ) -> Result<construct_protocol::DevAssetsResult> {
        self.request(
            ipc_method::DEV_SET_ASSETS,
            &construct_protocol::DevSetAssetsParams { dir },
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
    /// Fork an existing session into a new **sibling** session backed by
    /// `harness` (which may differ from the source's). The new session
    /// inherits the source's cwd and group and runs as an independent
    /// top-level session — not a child/subagent — so the source is left
    /// untouched (a session's own harness is immutable, per spec 0001).
    ///
    /// The fork preserves the source's terminal/headless shape: forking a
    /// visible terminal session starts the target harness in interactive PTY
    /// mode. Unless [`ForkOptions::seed`] is false or the target is the
    /// `shell` harness (which takes a command, not conversation context), the
    /// fork's initial prompt is seeded with a rendered summary of the source
    /// transcript so an agent harness can pick up where the original left off.
    /// A SAME-harness fork into a harness that forks natively
    /// ([`harness_forks_natively`]) skips the seed entirely: the daemon hands
    /// the adapter the source's native conversation to fork byte-for-byte
    /// (spec 0078), so the rendered transcript would be redundant context —
    /// delivered, worse, as a noisy "read the initial prompt file" first
    /// turn. A typed fork prompt still flows through either way.
    /// The source's Program document is copied to the fork as durable
    /// orchestration state; active execution/run state is not copied.
    /// Returns the new session id.
    pub async fn fork_session(
        &self,
        source_id: &str,
        harness: &str,
        opts: ForkOptions,
    ) -> Result<String> {
        let src = self.get(source_id).await?.summary;

        let mut prompt_parts: Vec<String> = Vec::new();
        let transcript_seq = self.transcript(source_id, 0, None).await.ok();
        let source_is_terminal = src.has_pty && src.mode.as_deref() != Some("headless");
        // Native continuation only happens on the adapters' interactive
        // paths, so a headless fork keeps the portable seed.
        let native_fork =
            harness == src.harness && harness_forks_natively(harness) && source_is_terminal;
        if opts.seed && !native_fork && harness != "shell" {
            // Full transcript from the start (seq 0) so the original objective
            // — usually stated in the opening message — is carried, not just
            // the recent tail.
            if let Some(tr) = transcript_seq.as_ref() {
                if let Some(seed) = render_fork_seed(&tr.events, opts.max_seed_bytes) {
                    prompt_parts.push(seed);
                }
            }
        }
        if let Some(p) = opts
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            prompt_parts.push(p.to_string());
        }
        let prompt = (!prompt_parts.is_empty()).then(|| prompt_parts.join("\n\n"));

        // A model spec is harness-specific (`openai:gpt-5` means nothing to
        // the claude harness), so only carry the source's model when the
        // harness is unchanged — unless the caller passed an explicit one.
        let model = opts.model.clone().or_else(|| {
            (harness == src.harness)
                .then(|| src.model.clone())
                .flatten()
        });

        // Text prefix rather than ⑂: the TUI list already shows ⑂ for
        // forked sessions via `forked_from`, so embedding the glyph in the
        // title would double-mark the row (e.g. "⑂  ⑂ name").
        let title = Some(match &src.title {
            Some(t) => format!("(fork) {t}"),
            None => format!("(fork) fork of {}", short_id(&src.id)),
        });
        let pty_size = source_is_terminal.then(|| {
            opts.pty_size.unwrap_or(PtySize {
                cols: 100,
                rows: 30,
            })
        });
        let mode = if source_is_terminal {
            Some("interactive".to_string())
        } else {
            src.mode.clone()
        };

        let new_id = self
            .create(CreateSessionParams {
                harness: harness.to_string(),
                cwd: src.cwd.clone(),
                prompt,
                model,
                title,
                mode,
                pty_size,
                worktree: false,
                env: HashMap::new(),
                args: Vec::new(),
                kind: construct_protocol::SessionKind::User,
                parent_session_id: None,        // sibling, not a subagent
                group_id: src.group_id.clone(), // same group → rendered alongside the source
                position_after_session_id: Some(src.id.clone()),
                // Every fork is lineage-tracked — the branch rail, fork log,
                // and merge eligibility all key off `forked_from` being
                // present, uniformly, whether the fork was the instant
                // same-harness primary path or the cross-harness picker.
                forked_from: Some(construct_protocol::ForkedFrom {
                    session_id: src.id.clone(),
                    transcript_seq: transcript_seq.as_ref().map(|t| t.total).unwrap_or(0),
                    at_ms: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64,
                    // The parent's compute time so far — the busy-time
                    // counterpart to `transcript_seq`, so lineage windows
                    // can report summed compute time per window.
                    parent_busy_ms: src.busy_ms_at(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis() as i64,
                    ),
                    // The parent's chat-message tally so far — the
                    // message-only counterpart to `transcript_seq`, so
                    // lineage windows can count actual messages.
                    parent_message_count: src.message_count,
                    // A user-initiated fork through this path, never an
                    // automatic reset snapshot (spec 0085) — those are
                    // synthesized entirely daemon-side, never through
                    // `fork_session`.
                    is_reset_snapshot: false,
                }),
            })
            .await?;

        let source_program = self.program_get(source_id).await?.program;
        if !source_program.markdown.is_empty() || source_program.template_id.is_some() {
            self.program_update(ProgramUpdateParams {
                session_id: new_id.clone(),
                markdown: source_program.markdown,
                base_version: None,
                actor: ProgramUpdateActor::Human,
                template_id: source_program.template_id,
                note: Some(format!("copied from fork source {}", short_id(source_id))),
                shimmer: None,
                shimmer_tooltips: None,
            })
            .await?;
        }

        Ok(new_id)
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
    /// Report which surface (chat vs terminal) this client is currently
    /// showing the given session through. Best-effort UI hint for the daemon.
    pub async fn set_view(&self, id: &str, view: ClientView) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_VIEW,
                &SessionSetViewParams {
                    session_id: id.to_string(),
                    view,
                },
            )
            .await?;
        Ok(())
    }
    /// Whether any connected client is watching the session in the chat view.
    pub async fn chat_viewer_active(&self, id: &str) -> Result<bool> {
        let r: ChatViewerActiveResult = self
            .request(
                ipc_method::SESSION_CHAT_VIEWER_ACTIVE,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(r.active)
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
    /// Fetch up to `max_bytes` of the most recent PTY-log tail — bounded, for
    /// rendering a compact screen preview without pulling the full replay cap.
    pub async fn pty_replay_tail(&self, id: &str, max_bytes: usize) -> Result<PtyReplayResult> {
        self.request(
            ipc_method::SESSION_PTY_REPLAY,
            &construct_protocol::PtyReplayParams {
                session_id: id.to_string(),
                max_bytes: Some(max_bytes),
                before_offset: None,
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
    /// Archive a session: terminate its adapter but keep history/worktree and
    /// hide it from the list by default. Reversed by [`Self::restart`].
    pub async fn archive(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_ARCHIVE,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    pub async fn merge(&self, id: &str, mode: construct_protocol::ForkMergeMode) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_MERGE,
                &construct_protocol::SessionMergeParams {
                    session_id: id.to_string(),
                    mode,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn delete_widget(&self, session_id: &str, panel_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_WIDGET_DELETE,
                &construct_protocol::SessionWidgetDeleteParams {
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
    /// `CONSTRUCT_RESUME=1`.
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
    /// Clear a session's `needs_attention` marker and mark it focused.
    pub async fn mark_seen(&self, id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_MARK_SEEN,
                &SessionIdParams {
                    session_id: id.to_string(),
                },
            )
            .await?;
        Ok(())
    }
    /// Update the set of visible/focused sessions in the daemon.
    /// Report this connection's painted terminal background (spec 0073):
    /// `Some([r, g, b])` when the client's theme paints the frame background,
    /// `None` for background-aware themes that leave the terminal visible.
    pub async fn set_terminal_background(&self, background: Option<[u8; 3]>) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::CLIENT_SET_TERMINAL_BACKGROUND,
                &SetTerminalBackgroundParams { background },
            )
            .await?;
        Ok(())
    }

    pub async fn set_focused_sessions(&self, ids: Vec<String>) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_FOCUSED,
                &SessionSetFocusedParams { session_ids: ids },
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
    pub async fn set_approval_mode(
        &self,
        id: &str,
        mode: construct_protocol::ApprovalMode,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_APPROVAL_MODE,
                &SessionSetApprovalModeParams {
                    session_id: id.to_string(),
                    mode,
                },
            )
            .await?;
        Ok(())
    }
    pub async fn emit_event(&self, id: &str, event: construct_protocol::SessionEvent) -> Result<()> {
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
        params: construct_protocol::LoopCreateParams,
    ) -> Result<construct_protocol::Loop> {
        self.request(ipc_method::LOOP_CREATE, &params).await
    }
    pub async fn loop_list(&self, session_id: Option<&str>) -> Result<Vec<construct_protocol::Loop>> {
        let r: construct_protocol::LoopListResult = self
            .request(
                ipc_method::LOOP_LIST,
                &construct_protocol::LoopListParams {
                    session_id: session_id.map(|s| s.to_string()),
                },
            )
            .await?;
        Ok(r.loops)
    }
    pub async fn loop_update(
        &self,
        params: construct_protocol::LoopUpdateParams,
    ) -> Result<construct_protocol::Loop> {
        self.request(ipc_method::LOOP_UPDATE, &params).await
    }
    pub async fn loop_remove(&self, loop_id: &str) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::LOOP_REMOVE,
                &construct_protocol::LoopRemoveParams {
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
    pub async fn list_tasks(&self, id: &str) -> Result<Vec<construct_protocol::TaskInfo>> {
        let r: construct_protocol::ListTasksResult = self
            .request(
                ipc_method::SESSION_LIST_TASKS,
                &construct_protocol::ListTasksParams {
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
                &construct_protocol::SessionToolActionParams {
                    session_id: id.to_string(),
                    call_id: call_id.into(),
                    action: action.into(),
                },
            )
            .await?;
        Ok(())
    }
    /// Returns whether the session actually moved — `false` means it was
    /// already at the edge of its reorder region (top/bottom of the list,
    /// or a forked session at the edge of its sibling forks).
    pub async fn move_session(&self, id: &str, direction: MoveDirection) -> Result<bool> {
        let r: construct_protocol::SessionMoveResult = self
            .request(
                ipc_method::SESSION_MOVE,
                &SessionMoveParams {
                    session_id: id.to_string(),
                    direction,
                },
            )
            .await?;
        Ok(r.moved)
    }
    /// Change a session's group membership. `group_id: None` ungroups
    /// the session. `position` controls where in the target region
    /// the session lands (`Top` of the list or `Bottom`).
    pub async fn set_session_group(
        &self,
        id: &str,
        group_id: Option<String>,
        position: construct_protocol::SessionGroupPosition,
    ) -> Result<()> {
        let _: serde_json::Value = self
            .request(
                ipc_method::SESSION_SET_GROUP,
                &construct_protocol::SessionSetGroupParams {
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
        position: construct_protocol::SessionGroupPosition,
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
    /// Fetch the most-recent `n` transcript events (bounded tail) — used for
    /// compact previews without paginating a long history.
    pub async fn transcript_tail(&self, id: &str, n: usize) -> Result<TranscriptResult> {
        self.request(
            ipc_method::SESSION_TRANSCRIPT,
            &TranscriptParams {
                session_id: id.to_string(),
                from: 0,
                limit: None,
                tail: Some(n),
            },
        )
        .await
    }
    /// Substring search across session name/metadata, stored program
    /// contents, and transcript history (spec 0076).
    pub async fn search(&self, params: SearchParams) -> Result<SearchResult> {
        self.request(ipc_method::SESSION_SEARCH, &params).await
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

/// Options for [`Client::fork_session`].
#[derive(Debug, Clone)]
pub struct ForkOptions {
    /// Model spec for the new session. `None` lets the target harness pick
    /// its default; the source's model is only inherited when the harness is
    /// unchanged.
    pub model: Option<String>,
    /// Extra user instruction appended after the seeded context (e.g.
    /// "continue from here"). `None` leaves the fork at just the seed (or
    /// interactive when nothing is seeded).
    pub prompt: Option<String>,
    /// Seed the fork's initial prompt with a rendering of the source
    /// transcript. Ignored for the `shell` harness.
    pub seed: bool,
    /// Safety ceiling on the rendered seed, in bytes. `0` (the default) means
    /// unlimited — the **full** transcript is seeded. When a positive cap is
    /// exceeded, the opening (which usually states the user's objective) and
    /// the most-recent activity are kept while the middle is elided, so the
    /// goal is never dropped.
    pub max_seed_bytes: usize,
    /// Initial PTY size for forks of terminal sessions. When omitted, the
    /// client uses a standard terminal default.
    pub pty_size: Option<PtySize>,
}

impl Default for ForkOptions {
    fn default() -> Self {
        Self {
            model: None,
            prompt: None,
            seed: true,
            max_seed_bytes: 0,
            pty_size: None,
        }
    }
}

/// Render transcript events into a plain-text context block for a forked
/// session. Renders the full history in chronological order so the opening
/// (objective) comes first. Returns `None` when there's nothing worth
/// seeding. When `max_bytes > 0` and the body exceeds it, keeps the opening
/// and the most-recent activity and elides the middle (see [`elide_middle`]).
/// Whether a SAME-harness fork of `harness` continues the source
/// conversation natively instead of via the portable transcript seed
/// (spec 0078): the daemon hands the adapter the source's native session
/// id and the harness forks it byte-for-byte (claude: `--resume <id>
/// --fork-session`; codex: `codex fork <id>`; opencode: `--session <id>
/// --fork`; grok: `-r <id>
/// --fork-session` — wired through the daemon's session lifecycle). For
/// these, `fork_session` skips the rendered seed — the harness already
/// holds the full context with better fidelity. Antigravity has no native
/// fork primitive (only in-place `--conversation` resume, backed by an
/// indexed store a state-copy would desync), so it keeps the seed.
fn harness_forks_natively(harness: &str) -> bool {
    matches!(harness, "claude" | "codex" | "opencode" | "grok")
}

fn render_fork_seed(
    events: &[construct_protocol::TimestampedEvent],
    max_bytes: usize,
) -> Option<String> {
    use construct_protocol::{MessageRole, SessionEvent};
    let mut lines: Vec<String> = Vec::new();
    for ev in events {
        match &ev.event {
            SessionEvent::Message { role, text } => {
                let t = text.trim();
                if t.is_empty() {
                    continue;
                }
                let who = match role {
                    MessageRole::User => "User",
                    MessageRole::Assistant => "Assistant",
                    MessageRole::System => "System",
                    MessageRole::Tool => "Tool",
                };
                lines.push(format!("{who}: {t}"));
            }
            SessionEvent::ToolUse { tool, .. } => lines.push(format!("[tool: {tool}]")),
            SessionEvent::ToolResult {
                tool, ok, output, ..
            } => {
                let status = if *ok { "ok" } else { "error" };
                lines.push(format!(
                    "[tool result: {tool} ({status})] {}",
                    truncate_str(output.trim(), 200)
                ));
            }
            // PTY/status/cost/etc. carry no portable conversation content.
            _ => {}
        }
    }
    if lines.is_empty() {
        return None;
    }
    let body = lines.join("\n");
    let body = if max_bytes > 0 && body.len() > max_bytes {
        elide_middle(&body, max_bytes)
    } else {
        body
    };
    Some(format!(
        "[Forked session context. The following is the prior conversation you \
         are continuing from — treat it as background; do not re-run past tool \
         calls.]\n\n{body}\n\n[End of forked context.]"
    ))
}

/// Compact portable transcript rendering for a fork-merge result.
pub fn render_fork_seed_for_merge(
    events: &[construct_protocol::TimestampedEvent],
    max_bytes: usize,
) -> Option<String> {
    render_fork_seed(events, max_bytes)
}

/// Keep roughly the first and last halves of `s` within `budget` bytes,
/// replacing the middle with an elision marker. The opening usually carries
/// the user's objective, so both ends are preserved. Char-boundary safe.
fn elide_middle(s: &str, budget: usize) -> String {
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;
    let mut head_end = head_budget.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(tail_budget);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start <= head_end {
        return s.to_string();
    }
    let middle = tail_start - head_end;
    format!(
        "{}\n…({middle} chars of earlier context elided)…\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod fork_tests {
    use super::*;
    use construct_protocol::{MessageRole, SessionEvent, TimestampedEvent};

    // Build via serde so the test doesn't need a direct `chrono` dep just to
    // stamp `at` (render_fork_seed only reads `.event`).
    fn ev(event: SessionEvent) -> TimestampedEvent {
        serde_json::from_value(serde_json::json!({
            "seq": 0,
            "at": "1970-01-01T00:00:00Z",
            "event": serde_json::to_value(&event).unwrap(),
        }))
        .unwrap()
    }

    #[test]
    fn seed_renders_messages_and_tools() {
        let events = vec![
            ev(SessionEvent::Message {
                role: MessageRole::User,
                text: "fix the bug".into(),
            }),
            ev(SessionEvent::ToolUse {
                tool: "edit_file".into(),
                args: serde_json::json!({}),
                call_id: None,
            }),
            ev(SessionEvent::ToolResult {
                tool: "edit_file".into(),
                ok: true,
                output: "patched".into(),
                call_id: None,
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "done".into(),
            }),
        ];
        let seed = render_fork_seed(&events, 0).expect("seed");
        assert!(seed.contains("User: fix the bug"));
        assert!(seed.contains("[tool: edit_file]"));
        assert!(seed.contains("[tool result: edit_file (ok)] patched"));
        assert!(seed.contains("Assistant: done"));
        assert!(seed.contains("Forked session context"));
    }

    #[test]
    fn seed_none_when_nothing_renderable() {
        let events = vec![
            ev(SessionEvent::Pty { data: "x".into() }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "   ".into(),
            }),
        ];
        assert!(render_fork_seed(&events, 0).is_none());
    }

    #[test]
    fn seed_unlimited_includes_full_history() {
        let events = vec![
            ev(SessionEvent::Message {
                role: MessageRole::User,
                text: "OBJECTIVE".into(),
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "MIDDLE".into(),
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "RECENT".into(),
            }),
        ];
        // max_bytes = 0 → nothing elided.
        let seed = render_fork_seed(&events, 0).unwrap();
        assert!(seed.contains("OBJECTIVE") && seed.contains("MIDDLE") && seed.contains("RECENT"));
        assert!(!seed.contains("elided"));
    }

    #[test]
    fn seed_cap_keeps_objective_and_recent_elides_middle() {
        let filler = "z".repeat(2000);
        let events = vec![
            ev(SessionEvent::Message {
                role: MessageRole::User,
                text: "OBJECTIVE_MARKER: build the thing".into(),
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: filler.clone(),
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: filler,
            }),
            ev(SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "RECENT_MARKER: latest state".into(),
            }),
        ];
        let seed = render_fork_seed(&events, 1000).unwrap();
        assert!(
            seed.contains("OBJECTIVE_MARKER"),
            "opening/objective preserved"
        );
        assert!(seed.contains("RECENT_MARKER"), "recent tail preserved");
        assert!(seed.contains("elided"), "middle elided with a marker");
    }

    #[test]
    fn default_options_seed_full() {
        let o = ForkOptions::default();
        assert!(o.seed);
        assert_eq!(o.max_seed_bytes, 0, "0 = unlimited / full transcript");
    }
}

#[cfg(test)]
mod fork_lineage_tests {
    use super::*;
    use tokio::io::{split, AsyncBufReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    /// Regression test for the fork/merge unification: every fork is now
    /// lineage-tracked (`forked_from` always set) — there is no more
    /// `side_quest` gate distinguishing an "instant" fork from a "tracked"
    /// one. This drives `fork_session` against a minimal mock daemon and
    /// asserts the `session.create` params it sends carry `forked_from`,
    /// even for a same-harness fork with default options (no explicit
    /// harness override, no typed prompt) — the exact shape of the primary
    /// instant-fork keybinding path.
    #[tokio::test]
    async fn fork_session_always_sets_forked_from() {
        // Unix socket paths are capped well under `PathBuf`'s limits
        // (SUN_LEN, ~104 bytes on macOS/BSD) — `std::env::temp_dir()` on
        // macOS already eats most of that budget (`/var/folders/.../T/`),
        // so bind directly under `/tmp` with a short, still-unique name
        // rather than a long descriptive one.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
            % 1_000_000;
        let sock =
            std::path::PathBuf::from(format!("/tmp/afl{}{}.sock", std::process::id(), nanos));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind mock daemon socket");

        let captured = Arc::new(StdMutex::new(None::<serde_json::Value>));
        let captured_srv = captured.clone();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = split(stream);
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            for _ in 0..4 {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let req: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let resp = match method {
                    ipc_method::SESSION_GET => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {
                            "summary": {
                                "id": "src-1",
                                "harness": "claude",
                                "cwd": "/tmp",
                                "state": "running",
                                "created_at": "1970-01-01T00:00:00Z",
                                "busy_ms": 4200,
                                "message_count": 3
                            },
                            "events": []
                        }
                    }),
                    ipc_method::SESSION_TRANSCRIPT => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": { "events": [], "total": 0 }
                    }),
                    ipc_method::PROGRAM_GET => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {
                            "program": {
                                "session_id": "src-1",
                                "markdown": "",
                                "version": 0,
                                "updated_at_ms": 0
                            },
                            "revisions": []
                        }
                    }),
                    ipc_method::SESSION_CREATE => {
                        *captured_srv.lock().unwrap() = req.get("params").cloned();
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": { "session_id": "new-1" }
                        })
                    }
                    _ => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
                };
                let s = resp.to_string() + "\n";
                let _ = w.write_all(s.as_bytes()).await;
            }
        });

        let client = Client::connect(&sock)
            .await
            .expect("connect to mock daemon");
        // Default options: no explicit harness override (fork into the same
        // "claude" harness the source already runs), no typed prompt.
        let new_id = client
            .fork_session("src-1", "claude", ForkOptions::default())
            .await
            .expect("fork_session");
        assert_eq!(new_id, "new-1");

        server.await.expect("mock daemon task");

        let params = captured
            .lock()
            .unwrap()
            .clone()
            .expect("session.create params were captured");
        let forked_from = params.get("forked_from").cloned();
        assert!(
            forked_from.as_ref().map(|v| !v.is_null()).unwrap_or(false),
            "fork_session must always set forked_from, even without an explicit \
             harness override: {params:?}"
        );
        assert_eq!(
            forked_from
                .as_ref()
                .and_then(|f| f.get("session_id"))
                .and_then(|s| s.as_str()),
            Some("src-1")
        );
        // The fork boundary snapshots the parent's accumulated compute time
        // so the lineage view can label windows with busy deltas. The mock
        // parent reports 4200ms banked and no open Running span, so the
        // stamp is exactly that.
        assert_eq!(
            forked_from
                .as_ref()
                .and_then(|f| f.get("parent_busy_ms"))
                .and_then(|v| v.as_u64()),
            Some(4200)
        );
        // Likewise the parent's chat-message tally, so window counts can be
        // message-only deltas.
        assert_eq!(
            forked_from
                .as_ref()
                .and_then(|f| f.get("parent_message_count"))
                .and_then(|v| v.as_u64()),
            Some(3)
        );
        // Untitled sources get a textual "(fork)" prefix — not the ⑂ glyph,
        // which the list view already draws from `forked_from`.
        assert_eq!(
            params.get("title").and_then(|t| t.as_str()),
            Some("(fork) fork of src-1")
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// Mock daemon whose source session runs `harness` and whose transcript
    /// holds one recognizable message; serves requests until the socket
    /// closes and captures every `session.create`'s params.
    async fn fork_capture_daemon(
        harness: &'static str,
        source_has_pty: bool,
    ) -> (
        std::path::PathBuf,
        Arc<StdMutex<Vec<serde_json::Value>>>,
        tokio::task::JoinHandle<()>,
    ) {
        // A per-process atomic counter, not just nanos: several of these
        // tests run concurrently within the same test binary (same pid)
        // and can land in the same microsecond, colliding on path.
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let sock =
            std::path::PathBuf::from(format!("/tmp/afc{}{}{}.sock", std::process::id(), nanos, n));
        let _ = std::fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock).expect("bind mock daemon socket");
        let captured = Arc::new(StdMutex::new(Vec::<serde_json::Value>::new()));
        let captured_srv = captured.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (r, mut w) = split(stream);
            let mut reader = BufReader::new(r);
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let req: serde_json::Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let resp = match method {
                    ipc_method::SESSION_GET => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {
                            "summary": {
                                "id": "src-1",
                                "harness": harness,
                                "cwd": "/tmp",
                                "state": "running",
                                "created_at": "1970-01-01T00:00:00Z",
                                "has_pty": source_has_pty
                            },
                            "events": []
                        }
                    }),
                    ipc_method::SESSION_TRANSCRIPT => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": { "events": [
                            {"seq": 1, "at": "1970-01-01T00:00:01Z", "event": {
                                "type": "message", "role": "user",
                                "text": "SEED_MARKER: original objective"
                            }}
                        ], "total": 1 }
                    }),
                    ipc_method::PROGRAM_GET => serde_json::json!({
                        "jsonrpc": "2.0", "id": id, "result": {
                            "program": {
                                "session_id": "src-1",
                                "markdown": "",
                                "version": 0,
                                "updated_at_ms": 0
                            },
                            "revisions": []
                        }
                    }),
                    ipc_method::SESSION_CREATE => {
                        captured_srv
                            .lock()
                            .unwrap()
                            .push(req.get("params").cloned().unwrap_or_default());
                        serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": { "session_id": "new-1" }
                        })
                    }
                    _ => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": null}),
                };
                let s = resp.to_string() + "\n";
                let _ = w.write_all(s.as_bytes()).await;
            }
        });
        (sock, captured, server)
    }

    /// A same-harness fork of a natively-forking harness (claude) must NOT
    /// seed the prompt with the rendered transcript — the daemon hands the
    /// adapter the source's native conversation (spec 0078), so the seed
    /// would be redundant context delivered as a noisy "read the initial
    /// prompt file" first turn. A typed fork prompt still goes through, and
    /// cross-harness forks keep the portable seed.
    #[tokio::test]
    async fn same_harness_claude_fork_skips_the_transcript_seed() {
        let (sock, captured, _server) = fork_capture_daemon("claude", true).await;
        let client = Client::connect(&sock).await.expect("connect");

        client
            .fork_session("src-1", "claude", ForkOptions::default())
            .await
            .expect("same-harness fork");
        let mut opts = ForkOptions::default();
        opts.prompt = Some("try the other approach".into());
        client
            .fork_session("src-1", "claude", opts)
            .await
            .expect("same-harness fork with typed prompt");
        client
            .fork_session("src-1", "smith", ForkOptions::default())
            .await
            .expect("cross-harness fork");

        let calls = captured.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[0].get("prompt").map(|p| p.is_null()).unwrap_or(true),
            "no seed, no prompt: {:?}",
            calls[0].get("prompt")
        );
        assert_eq!(
            calls[1].get("prompt").and_then(|p| p.as_str()),
            Some("try the other approach"),
            "the typed prompt flows through alone, without the seed"
        );
        let cross = calls[2]
            .get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        assert!(
            cross.contains("SEED_MARKER"),
            "a cross-harness fork keeps the portable transcript seed: {cross:?}"
        );

        let _ = std::fs::remove_file(&sock);
    }

    /// A same-harness fork of a harness WITHOUT native forking (smith) must
    /// keep the seed — it is the only context carrier there.
    #[tokio::test]
    async fn same_harness_smith_fork_keeps_the_transcript_seed() {
        let (sock, captured, _server) = fork_capture_daemon("smith", true).await;
        let client = Client::connect(&sock).await.expect("connect");
        client
            .fork_session("src-1", "smith", ForkOptions::default())
            .await
            .expect("same-harness smith fork");
        let calls = captured.lock().unwrap().clone();
        let prompt = calls[0]
            .get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        assert!(
            prompt.contains("SEED_MARKER"),
            "smith has no native fork; the seed is the only context: {prompt:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }

    /// codex (`codex fork <id>`), opencode (`--session <id> --fork`), and
    /// grok (`-r <id> --fork-session`) fork
    /// natively like claude — same-harness terminal forks skip the seed.
    /// Antigravity has no native fork primitive, so it keeps the seed.
    #[tokio::test]
    async fn native_fork_harnesses_skip_the_seed_antigravity_keeps_it() {
        for (harness, expect_seed) in [
            ("codex", false),
            ("opencode", false),
            ("grok", false),
            ("antigravity", true),
            ("agy", true),
        ] {
            let (sock, captured, _server) = fork_capture_daemon(harness, true).await;
            let client = Client::connect(&sock).await.expect("connect");
            client
                .fork_session("src-1", harness, ForkOptions::default())
                .await
                .expect("same-harness fork");
            let calls = captured.lock().unwrap().clone();
            let has_seed = calls[0]
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(|p| p.contains("SEED_MARKER"))
                .unwrap_or(false);
            assert_eq!(
                has_seed, expect_seed,
                "{harness}: seed presence should be {expect_seed}"
            );
            let _ = std::fs::remove_file(&sock);
        }
    }

    /// Native continuation only happens on the adapters' interactive
    /// paths — a fork of a HEADLESS source keeps the portable seed even
    /// for a natively-forking harness.
    #[tokio::test]
    async fn headless_same_harness_claude_fork_keeps_the_seed() {
        let (sock, captured, _server) = fork_capture_daemon("claude", false).await;
        let client = Client::connect(&sock).await.expect("connect");
        client
            .fork_session("src-1", "claude", ForkOptions::default())
            .await
            .expect("headless same-harness fork");
        let calls = captured.lock().unwrap().clone();
        let prompt = calls[0]
            .get("prompt")
            .and_then(|p| p.as_str())
            .unwrap_or_default();
        assert!(
            prompt.contains("SEED_MARKER"),
            "headless forks never native-fork; the seed must stay: {prompt:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }
}
