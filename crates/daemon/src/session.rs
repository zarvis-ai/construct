//! Session management: lifecycle, adapter binding, event ingestion, broadcast.

use crate::adapter::{locate_binary, Adapter, AdapterMessage};
use crate::config::Config;
use crate::storage::Storage;
use crate::worktree;
use agentd_protocol::{
    ahp_method, CreateSessionParams, EventNotificationPayload, HarnessInfo, MessageRole,
    PtyReplayResult, PtySize, SessionDetail, SessionEvent, SessionStartParams, SessionState,
    SessionSummary, StateNotificationPayload, TimestampedEvent, TranscriptResult,
};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

const BROADCAST_CAP: usize = 4096;
const ADAPTER_DRAIN_CAP: usize = 256;
/// Per-session PTY history kept in memory for late-attach replay.
const PTY_RING_CAP: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub enum BroadcastMsg {
    Event(EventNotificationPayload),
    State(StateNotificationPayload),
}

pub struct SessionEntry {
    pub id: String,
    summary: RwLock<SessionSummary>,
    transcript_count: AtomicU64,
    adapter: tokio::sync::Mutex<Option<Arc<Adapter>>>,
    pty: tokio::sync::Mutex<PtyState>,
}

#[derive(Default)]
struct PtyState {
    ring: VecDeque<u8>,
    size: Option<PtySize>,
}

impl PtyState {
    fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= PTY_RING_CAP {
            self.ring.clear();
            self.ring
                .extend(&bytes[bytes.len() - PTY_RING_CAP..]);
            return;
        }
        while self.ring.len() + bytes.len() > PTY_RING_CAP {
            self.ring.pop_front();
        }
        self.ring.extend(bytes);
    }

    fn snapshot(&self) -> Vec<u8> {
        let (a, b) = self.ring.as_slices();
        let mut out = Vec::with_capacity(a.len() + b.len());
        out.extend_from_slice(a);
        out.extend_from_slice(b);
        out
    }
}

impl SessionEntry {
    pub async fn summary(&self) -> SessionSummary {
        self.summary.read().await.clone()
    }
}

pub struct SessionManager {
    storage: Arc<Storage>,
    config: Arc<Config>,
    sessions: RwLock<HashMap<String, Arc<SessionEntry>>>,
    broadcast: broadcast::Sender<BroadcastMsg>,
}

impl SessionManager {
    pub async fn new(storage: Arc<Storage>, config: Arc<Config>) -> Result<Self> {
        let summaries = storage.list_summaries()?;
        let mut sessions = HashMap::new();
        for mut s in summaries {
            // Sessions whose adapter was alive when the daemon last died are
            // by definition orphaned now — mark them errored on restart.
            if !s.state.is_terminal() {
                s.state = SessionState::Errored;
                let _ = storage.save_summary(&s);
            }
            // Recover seq counter from transcript line count.
            let path = storage.transcript_path(&s.id);
            let count = if path.exists() {
                let f = std::fs::File::open(&path)?;
                let reader = std::io::BufReader::new(f);
                use std::io::BufRead;
                let mut n = 0u64;
                for line in reader.lines() {
                    let line = line?;
                    if !line.trim().is_empty() {
                        n += 1;
                    }
                }
                n
            } else {
                0
            };
            let entry = SessionEntry {
                id: s.id.clone(),
                summary: RwLock::new(s.clone()),
                transcript_count: AtomicU64::new(count),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(PtyState::default()),
            };
            sessions.insert(s.id.clone(), Arc::new(entry));
        }
        let (broadcast, _) = broadcast::channel(BROADCAST_CAP);
        Ok(Self {
            storage,
            config,
            sessions: RwLock::new(sessions),
            broadcast,
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastMsg> {
        self.broadcast.subscribe()
    }

    pub fn harnesses(&self) -> Vec<HarnessInfo> {
        self.config
            .adapters
            .iter()
            .map(|(name, cfg)| {
                let binary_spec = cfg.binary.clone().unwrap_or_else(|| name.clone());
                let resolved = locate_binary(&binary_spec);
                HarnessInfo {
                    name: name.clone(),
                    available: resolved.is_some(),
                    binary: resolved.as_ref().map(|p| p.to_string_lossy().to_string()),
                    description: cfg.description.clone(),
                    capabilities: Default::default(),
                }
            })
            .collect()
    }

    pub async fn list(&self) -> Vec<SessionSummary> {
        let guard = self.sessions.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for entry in guard.values() {
            out.push(entry.summary().await);
        }
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    pub async fn get_entry(&self, id: &str) -> Option<Arc<SessionEntry>> {
        self.sessions.read().await.get(id).cloned()
    }

    pub async fn detail(&self, id: &str) -> Result<SessionDetail> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let summary = entry.summary().await;
        let transcript = self.storage.read_transcript(id, 0, None)?;
        Ok(SessionDetail {
            summary,
            events: transcript.events,
        })
    }

    pub async fn transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
    ) -> Result<TranscriptResult> {
        if self.get_entry(id).await.is_none() {
            return Err(anyhow!("session not found: {}", id));
        }
        self.storage.read_transcript(id, from, limit)
    }

    pub async fn diff(&self, id: &str) -> Result<String> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let summary = entry.summary().await;
        if let Some(wt) = summary.worktree.as_deref() {
            let p = PathBuf::from(wt);
            if p.exists() {
                return worktree::diff_worktree(&p).await;
            }
        }
        // No worktree → run git diff in the original cwd.
        let cwd = PathBuf::from(&summary.cwd);
        if worktree::is_git_repo(&cwd).await {
            return worktree::diff_worktree(&cwd).await;
        }
        Ok(String::new())
    }

    pub async fn create(self: &Arc<Self>, params: CreateSessionParams) -> Result<String> {
        let adapter_cfg = self
            .config
            .adapters
            .get(&params.harness)
            .ok_or_else(|| anyhow!("unknown harness: {}", params.harness))?
            .clone();
        let binary_spec = adapter_cfg
            .binary
            .clone()
            .unwrap_or_else(|| params.harness.clone());
        let binary = locate_binary(&binary_spec)
            .ok_or_else(|| anyhow!("adapter binary not found: {}", binary_spec))?;

        let id = format!("s{}", uuid::Uuid::new_v4().simple());
        let now = Utc::now();

        // Worktree setup (best effort).
        let want_worktree =
            params.worktree || self.config.defaults.worktree.unwrap_or(false);
        let cwd_path = PathBuf::from(&params.cwd);
        let worktree_path = if want_worktree && worktree::is_git_repo(&cwd_path).await {
            let dest = self.storage.worktree_path(&id);
            let branch = format!("agentd/{}", id);
            match worktree::create_worktree(&cwd_path, &dest, &branch).await {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(%id, error = %e, "worktree creation failed; using original cwd");
                    None
                }
            }
        } else {
            None
        };
        let effective_cwd = worktree_path
            .clone()
            .unwrap_or_else(|| cwd_path.clone());

        let mut summary = SessionSummary {
            id: id.clone(),
            harness: params.harness.clone(),
            cwd: effective_cwd.to_string_lossy().to_string(),
            title: params.title.clone(),
            state: SessionState::Pending,
            created_at: now,
            last_event_at: None,
            cost_usd: None,
            model: params.model.clone(),
            worktree: worktree_path.as_ref().map(|p| p.to_string_lossy().to_string()),
            pending_input: false,
            last_prompt: params.prompt.clone(),
            event_count: 0,
            has_pty: false,
            mode: params.mode.clone(),
        };
        self.storage.save_summary(&summary)?;

        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);
        let combined_args = {
            let mut a = adapter_cfg.args.clone();
            a.extend(params.args.clone());
            a
        };
        let (adapter, info) = Adapter::spawn(
            params.harness.clone(),
            binary,
            combined_args,
            params.env.clone(),
            msg_tx.clone(),
        )
        .await
        .with_context(|| format!("spawn adapter for {}", params.harness))?;

        // Apply capability-derived info.
        if summary.model.is_none() {
            summary.model = info.capabilities.models.first().cloned();
        }
        summary.has_pty = info.capabilities.supports_pty;
        self.storage.save_summary(&summary)?;

        // Send session.start.
        let start_params = SessionStartParams {
            session_id: id.clone(),
            cwd: summary.cwd.clone(),
            prompt: params.prompt.clone(),
            model: summary.model.clone(),
            mode: params.mode.clone(),
            pty_size: params.pty_size,
            env: params.env.clone(),
            args: params.args.clone(),
        };
        // Reflect Pending → Running on start (the adapter may also emit a status).
        summary.state = SessionState::Running;
        self.storage.save_summary(&summary)?;

        let entry = Arc::new(SessionEntry {
            id: id.clone(),
            summary: RwLock::new(summary.clone()),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(Some(adapter.clone())),
            pty: tokio::sync::Mutex::new(PtyState {
                ring: VecDeque::new(),
                size: params.pty_size,
            }),
        });

        // Record the user's initial prompt as the first transcript event so
        // the transcript reads coherently (user → assistant) for every adapter.
        if let Some(p) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            self.handle_event(
                &entry,
                SessionEvent::Message {
                    role: MessageRole::User,
                    text: p.clone(),
                },
            )
            .await;
        }

        adapter
            .request(
                ahp_method::SESSION_START,
                serde_json::to_value(&start_params)?,
            )
            .await
            .context("adapter session.start failed")?;

        self.sessions.write().await.insert(id.clone(), entry.clone());

        // Spawn drain task for adapter messages.
        let manager = self.clone();
        let entry_for_drain = entry.clone();
        tokio::spawn(async move {
            manager.drain_adapter(entry_for_drain, msg_rx).await;
        });

        // Broadcast initial state.
        let _ = self.broadcast.send(BroadcastMsg::State(StateNotificationPayload {
            session: summary,
        }));

        Ok(id)
    }

    async fn drain_adapter(
        self: Arc<Self>,
        entry: Arc<SessionEntry>,
        mut msg_rx: mpsc::Receiver<AdapterMessage>,
    ) {
        while let Some(msg) = msg_rx.recv().await {
            match msg {
                AdapterMessage::Event(env) => {
                    self.handle_event(&entry, env.event).await;
                }
                AdapterMessage::Log { session_id: _, line } => {
                    tracing::info!(session = %entry.id, "adapter: {line}");
                }
                AdapterMessage::Closed { exit_code } => {
                    let mut summary = entry.summary.write().await;
                    if !summary.state.is_terminal() {
                        summary.state = if exit_code.unwrap_or(0) == 0 {
                            SessionState::Done
                        } else {
                            SessionState::Errored
                        };
                    }
                    summary.last_event_at = Some(Utc::now());
                    let snapshot = summary.clone();
                    drop(summary);
                    let _ = self.storage.save_summary(&snapshot);
                    *entry.adapter.lock().await = None;
                    let _ = self.broadcast.send(BroadcastMsg::State(
                        StateNotificationPayload { session: snapshot },
                    ));
                    break;
                }
            }
        }
    }

    async fn handle_event(&self, entry: &SessionEntry, event: SessionEvent) {
        // PTY events take a fast path: they go to the in-memory ring + a live
        // broadcast, but they don't bloat the structured transcript.
        if let SessionEvent::Pty { .. } = &event {
            if let Some(bytes) = event.pty_bytes() {
                entry.pty.lock().await.push(&bytes);
            }
            let now = Utc::now();
            // Latest seq for ordering only; not persisted.
            let seq = entry.transcript_count.load(Ordering::Relaxed);
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq,
                }));
            return;
        }

        let seq = entry.transcript_count.fetch_add(1, Ordering::Relaxed) + 1;
        let now = Utc::now();
        let ts = TimestampedEvent {
            seq,
            at: now,
            event: event.clone(),
        };
        if let Err(e) = self.storage.append_event(&entry.id, &ts) {
            tracing::warn!(session = %entry.id, error = ?e, "append_event failed");
        }
        // Update summary based on event semantics.
        {
            let mut s = entry.summary.write().await;
            s.last_event_at = Some(now);
            s.event_count = seq;
            match &event {
                SessionEvent::Status { state, .. } => {
                    s.state = *state;
                    s.pending_input = matches!(state, SessionState::AwaitingInput);
                }
                SessionEvent::AwaitingInput { prompt } => {
                    s.state = SessionState::AwaitingInput;
                    s.pending_input = true;
                    if let Some(p) = prompt {
                        s.last_prompt = Some(p.clone());
                    }
                }
                SessionEvent::Cost { usd, .. } => {
                    s.cost_usd = Some(s.cost_usd.unwrap_or(0.0) + *usd);
                }
                SessionEvent::Done { exit_code } => {
                    s.state = if *exit_code == 0 {
                        SessionState::Done
                    } else {
                        SessionState::Errored
                    };
                    s.pending_input = false;
                }
                SessionEvent::Error { .. } => {
                    s.state = SessionState::Errored;
                    s.pending_input = false;
                }
                SessionEvent::Message { .. }
                | SessionEvent::ToolUse { .. }
                | SessionEvent::ToolResult { .. }
                | SessionEvent::Diff { .. }
                | SessionEvent::Pty { .. } => {}
            }
            let snapshot = s.clone();
            drop(s);
            let _ = self.storage.save_summary(&snapshot);
        }

        let _ = self.broadcast.send(BroadcastMsg::Event(EventNotificationPayload {
            session_id: entry.id.clone(),
            at: now,
            event,
            seq,
        }));

        // Also push a state snapshot so list views update without explicit refresh.
        let summary = entry.summary().await;
        let _ = self.broadcast.send(BroadcastMsg::State(StateNotificationPayload {
            session: summary,
        }));
    }

    pub async fn send_input(&self, id: &str, text: String) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        // Record the input as a user message so it shows in the transcript.
        self.handle_event(
            &entry,
            SessionEvent::Message {
                role: MessageRole::User,
                text: text.clone(),
            },
        )
        .await;
        let params = serde_json::to_value(&agentd_protocol::SessionInputParams {
            session_id: id.to_string(),
            text,
        })?;
        adapter.request(ahp_method::SESSION_INPUT, params).await?;
        Ok(())
    }

    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(
            &agentd_protocol::SessionPtyInputParams::from_bytes(id, &bytes),
        )?;
        adapter.request(ahp_method::SESSION_PTY_INPUT, params).await?;
        Ok(())
    }

    pub async fn pty_resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        entry.pty.lock().await.size = Some(PtySize { cols, rows });
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionPtyResizeParams {
            session_id: id.to_string(),
            cols,
            rows,
        })?;
        adapter
            .request(ahp_method::SESSION_PTY_RESIZE, params)
            .await?;
        Ok(())
    }

    pub async fn pty_replay(&self, id: &str) -> Result<PtyReplayResult> {
        use base64::Engine;
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let pty = entry.pty.lock().await;
        Ok(PtyReplayResult {
            data: base64::engine::general_purpose::STANDARD.encode(pty.snapshot()),
            size: pty.size,
        })
    }

    pub async fn interrupt(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionIdParams {
            session_id: id.to_string(),
        })?;
        adapter.request(ahp_method::SESSION_INTERRUPT, params).await?;
        Ok(())
    }

    pub async fn stop(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionIdParams {
            session_id: id.to_string(),
        })?;
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            adapter.request(ahp_method::SESSION_STOP, params),
        )
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
        Ok(())
    }

    pub async fn kill(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let adapter = entry.adapter.lock().await.clone();
        if let Some(a) = adapter {
            a.kill();
        }
        let mut s = entry.summary.write().await;
        if !s.state.is_terminal() {
            s.state = SessionState::Errored;
        }
        let snapshot = s.clone();
        drop(s);
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }
}
