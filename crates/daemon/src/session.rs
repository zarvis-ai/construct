//! Session management: lifecycle, adapter binding, event ingestion, broadcast.

use crate::adapter::{locate_binary, Adapter, AdapterMessage};
use crate::config::Config;
use crate::storage::Storage;
use crate::worktree;
use agentd_protocol::{
    ahp_method, CreateSessionParams, DeletedNotificationPayload, EventNotificationPayload,
    GroupDeletedNotificationPayload, GroupStateNotificationPayload, GroupSummary, HarnessInfo,
    MessageRole, MoveDirection, PtyReplayResult, PtySize, SessionDetail, SessionEvent,
    SessionStartParams, SessionState, SessionSummary, StateNotificationPayload, TimestampedEvent,
    TranscriptResult,
};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

const BROADCAST_CAP: usize = 4096;
const ADAPTER_DRAIN_CAP: usize = 256;
/// Per-session PTY history kept in memory for late-attach replay.
const PTY_RING_CAP: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionEdge {
    Top,
    Bottom,
}

/// Returns the `group_id` of the region immediately above the given region
/// in display order. `Some(None)` = ungrouped; `Some(Some(id))` = group N-1.
/// `None` = there is nothing above (ungrouped is already at the top).
fn region_above(region: Option<&str>, groups: &[GroupSummary]) -> Option<Option<String>> {
    match region {
        None => None,
        Some(id) => {
            let idx = groups.iter().position(|g| g.id == id)?;
            if idx == 0 {
                Some(None)
            } else {
                Some(Some(groups[idx - 1].id.clone()))
            }
        }
    }
}

/// Returns the `group_id` of the region immediately below the given region
/// in display order. `Some(Some(id))` = next group. `None` = nothing below.
fn region_below(region: Option<&str>, groups: &[GroupSummary]) -> Option<Option<String>> {
    match region {
        None => groups.first().map(|g| Some(g.id.clone())),
        Some(id) => {
            let idx = groups.iter().position(|g| g.id == id)?;
            groups.get(idx + 1).map(|g| Some(g.id.clone()))
        }
    }
}

#[derive(Clone, Debug)]
pub enum BroadcastMsg {
    Event(EventNotificationPayload),
    State(StateNotificationPayload),
    Deleted(DeletedNotificationPayload),
    GroupState(GroupStateNotificationPayload),
    GroupDeleted(GroupDeletedNotificationPayload),
}

pub struct SessionEntry {
    pub id: String,
    summary: RwLock<SessionSummary>,
    transcript_count: AtomicU64,
    adapter: tokio::sync::Mutex<Option<Arc<Adapter>>>,
    pty: tokio::sync::Mutex<PtyState>,
    /// Set by [`SessionManager::delete`] before tearing down the adapter so
    /// the drain task and event handler stop writing storage after the
    /// session has been removed.
    deleted: AtomicBool,
}

impl SessionEntry {
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::SeqCst)
    }
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

pub struct GroupEntry {
    summary: RwLock<GroupSummary>,
}

impl GroupEntry {
    pub async fn summary(&self) -> GroupSummary {
        self.summary.read().await.clone()
    }
}

pub struct SessionManager {
    storage: Arc<Storage>,
    config: Arc<Config>,
    sessions: RwLock<HashMap<String, Arc<SessionEntry>>>,
    groups: RwLock<HashMap<String, Arc<GroupEntry>>>,
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
            // Rehydrate the in-memory PTY ring from the on-disk tail so
            // scrollback survives daemon restarts.
            let mut pty_state = PtyState::default();
            match storage.read_pty_tail(&s.id, PTY_RING_CAP) {
                Ok(bytes) if !bytes.is_empty() => pty_state.push(&bytes),
                Ok(_) => {}
                Err(e) => tracing::warn!(id = %s.id, error = ?e, "pty_log tail read failed"),
            }
            let entry = SessionEntry {
                id: s.id.clone(),
                summary: RwLock::new(s.clone()),
                transcript_count: AtomicU64::new(count),
                adapter: tokio::sync::Mutex::new(None),
                pty: tokio::sync::Mutex::new(pty_state),
                deleted: AtomicBool::new(false),
            };
            sessions.insert(s.id.clone(), Arc::new(entry));
        }
        // Load persisted groups.
        let mut groups: HashMap<String, Arc<GroupEntry>> = HashMap::new();
        match storage.load_groups() {
            Ok(list) => {
                for g in list {
                    groups.insert(
                        g.id.clone(),
                        Arc::new(GroupEntry {
                            summary: RwLock::new(g),
                        }),
                    );
                }
            }
            Err(e) => tracing::warn!(error = ?e, "load_groups failed"),
        }

        let (broadcast, _) = broadcast::channel(BROADCAST_CAP);
        Ok(Self {
            storage,
            config,
            sessions: RwLock::new(sessions),
            groups: RwLock::new(groups),
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
        // Primary: user-controlled position ASC. Tiebreaker: newer first.
        out.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
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
            pinned: false,
            // Negative timestamp so newer sessions sort to the top by default.
            position: -now.timestamp_millis(),
            group_id: None,
            last_pty_at_ms: None,
            automode: false,
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
            deleted: AtomicBool::new(false),
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
                    if entry.is_deleted() {
                        // Session was deleted out from under us — don't
                        // resurrect storage or broadcast a stale state.
                        *entry.adapter.lock().await = None;
                        break;
                    }
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
        // Skip everything once the session has been deleted — the drain task
        // and the adapter can still feed us events for a beat.
        if entry.is_deleted() {
            return;
        }
        // PTY events take a fast path: they go to the in-memory ring +
        // append to the on-disk pty.log + a live broadcast, but they don't
        // bloat the structured transcript.
        if let SessionEvent::Pty { .. } = &event {
            if let Some(bytes) = event.pty_bytes() {
                entry.pty.lock().await.push(&bytes);
                if let Err(e) = self.storage.append_pty_bytes(&entry.id, &bytes) {
                    tracing::warn!(
                        session = %entry.id,
                        error = ?e,
                        "pty_log append failed",
                    );
                }
            }
            let now = Utc::now();
            // Track activity for the "session looks busy" signal. In-memory
            // only; the value gets persisted next time a lifecycle event
            // triggers save_summary.
            entry.summary.write().await.last_pty_at_ms = Some(now.timestamp_millis());
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
                | SessionEvent::Pty { .. }
                | SessionEvent::ToolApprovalRequest { .. } => {}
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

    /// Delete a session entirely: kill the adapter if still alive, remove the
    /// worktree (best effort), drop the on-disk record, evict from the live
    /// map, and broadcast a `session/deleted` notification.
    pub async fn delete(&self, id: &str) -> Result<()> {
        // Pull out the entry so the in-memory map releases the Arc; the
        // entry itself stays alive via our local Arc until the function ends.
        let entry = {
            let mut map = self.sessions.write().await;
            map.remove(id)
                .ok_or_else(|| anyhow!("session not found: {}", id))?
        };

        // Tell the drain task and event handler not to write storage anymore
        // before we tear the adapter down (killing the adapter triggers a
        // Closed event that the drain task would otherwise persist).
        entry.deleted.store(true, Ordering::SeqCst);

        // Kill the adapter if it's still running.
        if let Some(adapter) = entry.adapter.lock().await.take() {
            adapter.kill();
        }

        // Give the drain task a moment to observe the Closed event and exit
        // cleanly. With is_deleted set, it won't touch storage either way.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Remove the worktree if there is one. Best effort.
        let summary = entry.summary.read().await.clone();
        if let Some(wt) = summary.worktree.as_deref() {
            let wt_path = PathBuf::from(wt);
            if let Err(e) = worktree::remove_worktree(&wt_path).await {
                tracing::warn!(%id, error = %e, "remove_worktree failed");
            }
        }

        // Drop the on-disk record. Best effort.
        if let Err(e) = self.storage.remove_session(id) {
            tracing::warn!(%id, error = ?e, "remove_session failed");
        }

        // Best-effort: remove the per-session MCP config the adapter may
        // have written for an injected `agentd-mcp` server.
        let mcp_path = agentd_protocol::paths::Paths::discover()
            .state_dir
            .join("mcp")
            .join(format!("{id}.json"));
        if mcp_path.exists() {
            let _ = std::fs::remove_file(&mcp_path);
        }

        let _ = self
            .broadcast
            .send(BroadcastMsg::Deleted(DeletedNotificationPayload {
                session_id: id.to_string(),
            }));
        Ok(())
    }

    /// Move a session by one slot in the list view.
    ///
    /// Within a single region (ungrouped or one group), this swaps positions
    /// with the neighbor. At a region boundary, the session either *enters*
    /// the adjacent group or *exits* its current group:
    ///
    /// - Move-down past the bottom of a region → enter the next region as
    ///   its first child (top of next group).
    /// - Move-up past the top of a region → enter the previous region as
    ///   its last child (bottom of previous group, or end of ungrouped).
    ///
    /// No-op at the absolute top (ungrouped session #0) or bottom (last
    /// member of last group).
    pub async fn move_session(&self, id: &str, dir: MoveDirection) -> Result<()> {
        let all_sessions: Vec<SessionSummary> = self.list().await;
        let all_groups: Vec<GroupSummary> = self.list_groups().await;
        let me = all_sessions
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("session not found: {}", id))?;

        // Find neighbors in `me`'s region (same group_id), sorted by position.
        let region: Vec<&SessionSummary> = all_sessions
            .iter()
            .filter(|s| s.group_id == me.group_id)
            .collect();
        let pos_in_region = region.iter().position(|s| s.id == id).unwrap();

        match dir {
            MoveDirection::Up => {
                if pos_in_region > 0 {
                    // Same-region swap.
                    let other = region[pos_in_region - 1];
                    return self.swap_session_positions(&me.id, &other.id).await;
                }
                // At top of region — try to exit into the previous region.
                let prev = region_above(me.group_id.as_deref(), &all_groups);
                let Some(prev_region) = prev else { return Ok(()); };
                self.move_session_into_region(&me.id, &prev_region, RegionEdge::Bottom, &all_sessions)
                    .await
            }
            MoveDirection::Down => {
                if pos_in_region + 1 < region.len() {
                    let other = region[pos_in_region + 1];
                    return self.swap_session_positions(&me.id, &other.id).await;
                }
                // At bottom of region — try to enter the next region.
                let next = region_below(me.group_id.as_deref(), &all_groups);
                let Some(next_region) = next else { return Ok(()); };
                self.move_session_into_region(&me.id, &next_region, RegionEdge::Top, &all_sessions)
                    .await
            }
        }
    }

    async fn swap_session_positions(&self, a_id: &str, b_id: &str) -> Result<()> {
        let entry_a = self
            .get_entry(a_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", a_id))?;
        let entry_b = self
            .get_entry(b_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", b_id))?;
        let a_pos = entry_a.summary.read().await.position;
        let b_pos = entry_b.summary.read().await.position;
        let snap_a = {
            let mut s = entry_a.summary.write().await;
            s.position = b_pos;
            s.clone()
        };
        let snap_b = {
            let mut s = entry_b.summary.write().await;
            s.position = a_pos;
            s.clone()
        };
        self.storage.save_summary(&snap_a)?;
        self.storage.save_summary(&snap_b)?;
        let _ = self.broadcast.send(BroadcastMsg::State(
            StateNotificationPayload { session: snap_a },
        ));
        let _ = self.broadcast.send(BroadcastMsg::State(
            StateNotificationPayload { session: snap_b },
        ));
        Ok(())
    }

    /// Re-tag a session into a new region (group_id) and set its position
    /// so it lands at the top or bottom of that region.
    async fn move_session_into_region(
        &self,
        session_id: &str,
        new_group_id: &Option<String>,
        edge: RegionEdge,
        all_sessions: &[SessionSummary],
    ) -> Result<()> {
        // Pick a position that puts us at the requested edge of the region.
        let region_positions: Vec<i64> = all_sessions
            .iter()
            .filter(|s| s.id != session_id && s.group_id == *new_group_id)
            .map(|s| s.position)
            .collect();
        let new_pos = match edge {
            RegionEdge::Top => {
                let min = region_positions.iter().min().copied().unwrap_or(0);
                min - 1
            }
            RegionEdge::Bottom => {
                let max = region_positions.iter().max().copied().unwrap_or(0);
                max + 1
            }
        };

        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.group_id = new_group_id.clone();
            s.position = new_pos;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self.broadcast.send(BroadcastMsg::State(
            StateNotificationPayload { session: snapshot },
        ));
        Ok(())
    }

    // ----- Groups -----

    pub async fn list_groups(&self) -> Vec<GroupSummary> {
        let guard = self.groups.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for entry in guard.values() {
            out.push(entry.summary().await);
        }
        out.sort_by_key(|g| g.position);
        out
    }

    pub async fn create_group(&self, name: String) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("group name is empty"));
        }
        let id = format!("g{}", uuid::Uuid::new_v4().simple());
        let now = Utc::now();
        let summary = GroupSummary {
            id: id.clone(),
            name: name.to_string(),
            created_at: now,
            position: -now.timestamp_millis(),
            collapsed: false,
        };
        self.storage.save_group(&summary)?;
        self.groups.write().await.insert(
            id.clone(),
            Arc::new(GroupEntry {
                summary: RwLock::new(summary.clone()),
            }),
        );
        let _ = self.broadcast.send(BroadcastMsg::GroupState(
            GroupStateNotificationPayload { group: summary },
        ));
        Ok(id)
    }

    pub async fn rename_group(&self, id: &str, name: String) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("group name is empty"));
        }
        let entry = self
            .groups
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.name = name.to_string();
            s.clone()
        };
        self.storage.save_group(&snapshot)?;
        let _ = self.broadcast.send(BroadcastMsg::GroupState(
            GroupStateNotificationPayload { group: snapshot },
        ));
        Ok(())
    }

    pub async fn set_group_collapsed(&self, id: &str, collapsed: bool) -> Result<()> {
        let entry = self
            .groups
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.collapsed = collapsed;
            s.clone()
        };
        self.storage.save_group(&snapshot)?;
        let _ = self.broadcast.send(BroadcastMsg::GroupState(
            GroupStateNotificationPayload { group: snapshot },
        ));
        Ok(())
    }

    /// Delete a group. Member sessions are orphaned (group_id set to None);
    /// they survive the group deletion.
    pub async fn delete_group(&self, id: &str) -> Result<()> {
        let entry = self.groups.write().await.remove(id);
        if entry.is_none() {
            return Err(anyhow!("group not found: {}", id));
        }
        // Orphan members: scan sessions, anything with this group_id becomes None.
        let session_ids: Vec<String> = self.sessions.read().await.keys().cloned().collect();
        for sid in session_ids {
            let Some(s_entry) = self.sessions.read().await.get(&sid).cloned() else {
                continue;
            };
            let needs_update = {
                let s = s_entry.summary.read().await;
                s.group_id.as_deref() == Some(id)
            };
            if !needs_update {
                continue;
            }
            let snapshot = {
                let mut s = s_entry.summary.write().await;
                s.group_id = None;
                s.clone()
            };
            let _ = self.storage.save_summary(&snapshot);
            let _ = self.broadcast.send(BroadcastMsg::State(
                StateNotificationPayload { session: snapshot },
            ));
        }
        let _ = self.storage.remove_group(id);
        let _ = self.broadcast.send(BroadcastMsg::GroupDeleted(
            GroupDeletedNotificationPayload {
                group_id: id.to_string(),
            },
        ));
        Ok(())
    }

    /// Swap a group's position with its neighbor in the requested direction.
    /// No-op at the edges.
    pub async fn move_group(&self, id: &str, dir: MoveDirection) -> Result<()> {
        let groups = self.list_groups().await; // sorted by position
        let idx = groups
            .iter()
            .position(|g| g.id == id)
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let neighbor_idx = match dir {
            MoveDirection::Up => {
                if idx == 0 {
                    return Ok(());
                }
                idx - 1
            }
            MoveDirection::Down => {
                if idx + 1 >= groups.len() {
                    return Ok(());
                }
                idx + 1
            }
        };
        let a_id = groups[idx].id.clone();
        let b_id = groups[neighbor_idx].id.clone();
        let a_pos = groups[idx].position;
        let b_pos = groups[neighbor_idx].position;
        let entry_a = self
            .groups
            .read()
            .await
            .get(&a_id)
            .cloned()
            .ok_or_else(|| anyhow!("group missing"))?;
        let entry_b = self
            .groups
            .read()
            .await
            .get(&b_id)
            .cloned()
            .ok_or_else(|| anyhow!("group missing"))?;
        let snap_a = {
            let mut s = entry_a.summary.write().await;
            s.position = b_pos;
            s.clone()
        };
        let snap_b = {
            let mut s = entry_b.summary.write().await;
            s.position = a_pos;
            s.clone()
        };
        self.storage.save_group(&snap_a)?;
        self.storage.save_group(&snap_b)?;
        let _ = self.broadcast.send(BroadcastMsg::GroupState(
            GroupStateNotificationPayload { group: snap_a },
        ));
        let _ = self.broadcast.send(BroadcastMsg::GroupState(
            GroupStateNotificationPayload { group: snap_b },
        ));
        Ok(())
    }

    pub async fn set_title(&self, id: &str, title: Option<String>) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Normalize: trim, treat empty as None.
        let normalized = title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.title = normalized;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    pub async fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.pinned = pinned;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    pub async fn set_automode(&self, id: &str, on: bool) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.automode = on;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        // Forward to the adapter so it picks up the change for the next tool
        // classification. If the adapter is gone (session ended), skip.
        if let Some(adapter) = entry.adapter.lock().await.clone() {
            let params = serde_json::to_value(&agentd_protocol::SessionSetAutomodeParams {
                session_id: id.to_string(),
                on,
            })?;
            // Best-effort: don't fail the call if the adapter doesn't recognize
            // the method (e.g. claude/codex, which don't gate tools).
            let _ = adapter
                .request(ahp_method::SESSION_SET_AUTOMODE, params)
                .await;
        }
        Ok(())
    }

    pub async fn tool_decision(
        &self,
        id: &str,
        call_id: String,
        decision: String,
    ) -> Result<()> {
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
        // If the user chose "automode" from the prompt, flip the session flag
        // too so the modeline reflects the new state across clients.
        if decision == "automode" {
            let snapshot = {
                let mut s = entry.summary.write().await;
                s.automode = true;
                s.clone()
            };
            self.storage.save_summary(&snapshot)?;
            let _ = self
                .broadcast
                .send(BroadcastMsg::State(StateNotificationPayload {
                    session: snapshot,
                }));
        }
        let params = serde_json::to_value(&agentd_protocol::SessionToolDecisionParams {
            session_id: id.to_string(),
            call_id,
            decision,
        })?;
        adapter
            .request(ahp_method::SESSION_TOOL_DECISION, params)
            .await?;
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
