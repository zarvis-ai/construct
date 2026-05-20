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
/// Delay between session.start succeeding on respawn and the
/// force-redraw bump+restore that nudges the child into a full
/// SIGWINCH redraw. Long enough for the child to finish its initial
/// startup draw, short enough that the user sees the resumed pane
/// painted by the time they navigate to it.
const RESPAWN_REDRAW_DELAY: Duration = Duration::from_millis(250);

fn should_resume_on_startup(state: SessionState) -> bool {
    !matches!(state, SessionState::Done)
}

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
    /// Set the first time we kick off an auto-title generation for this
    /// session. Stops a flurry of user messages from spawning multiple
    /// title-gen processes; a failed title-gen leaves the title unset
    /// and the session keeps its hash-derived display name.
    title_gen_attempted: AtomicBool,
    /// PTY-input accumulator used to derive the auto-title prompt for
    /// adapters that don't echo user input back as `SessionEvent::Message`
    /// events (shell / claude / codex interactive). Decodes printable
    /// ASCII through a tiny ESC-sequence state machine; first CR/LF
    /// closes the buffer and feeds it to title-gen.
    pty_input_capture: tokio::sync::Mutex<PtyInputCapture>,
    /// Per-session tool-call lifecycle map. Updated from
    /// `SessionEvent::TaskStart` / `TaskBackgrounded` / `TaskEnd`.
    /// Surfaced by `session.list_tasks` for the TUI `/tasks` popup
    /// and the MCP `agentd_get_tasks` tool.
    pub tasks: tokio::sync::Mutex<TaskRegistry>,
}

/// Bounded log of recent + in-flight task entries. Held inside
/// each `SessionEntry`; rebuilt from event replay on rehydrate.
#[derive(Default)]
pub struct TaskRegistry {
    /// Newest-first list. Capped at [`TASK_REGISTRY_CAP`] entries;
    /// terminal-state oldest are evicted when over.
    entries: Vec<agentd_protocol::TaskInfo>,
}

/// How many tasks (running + recent terminal) we keep per session.
/// Bounded so the registry doesn't grow forever; recent enough that
/// `/tasks` shows useful history.
const TASK_REGISTRY_CAP: usize = 50;

impl TaskRegistry {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn upsert_start(
        &mut self,
        call_id: String,
        tool: String,
        args_summary: String,
        started_at_ms: i64,
    ) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            // Restart-of-same-call_id is unusual but harmless; treat
            // as a fresh entry by resetting state.
            e.tool = tool;
            e.args_summary = args_summary;
            e.state = agentd_protocol::TaskState::Running;
            e.started_at_ms = started_at_ms;
            e.backgrounded_at_ms = None;
            e.ended_at_ms = None;
            e.output_preview = None;
            e.ok = false;
            return;
        }
        self.entries.push(agentd_protocol::TaskInfo {
            call_id,
            tool,
            args_summary,
            state: agentd_protocol::TaskState::Running,
            started_at_ms,
            backgrounded_at_ms: None,
            ended_at_ms: None,
            output_preview: None,
            ok: false,
        });
        self.gc_terminal();
    }

    pub fn mark_backgrounded(&mut self, call_id: &str, at_ms: i64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            e.state = agentd_protocol::TaskState::Backgrounded;
            e.backgrounded_at_ms = Some(at_ms);
        }
    }

    pub fn mark_end(&mut self, call_id: &str, ok: bool, output_preview: String, at_ms: i64) {
        if let Some(e) = self.entries.iter_mut().find(|e| e.call_id == call_id) {
            e.state = if ok {
                agentd_protocol::TaskState::Completed
            } else {
                agentd_protocol::TaskState::Failed
            };
            e.ended_at_ms = Some(at_ms);
            e.output_preview = Some(output_preview);
            e.ok = ok;
        }
    }

    pub fn snapshot(&self) -> Vec<agentd_protocol::TaskInfo> {
        self.entries.clone()
    }

    fn gc_terminal(&mut self) {
        if self.entries.len() <= TASK_REGISTRY_CAP {
            return;
        }
        // Evict oldest terminal entries first; keep running / bg.
        let mut to_remove: Vec<usize> = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    e.state,
                    agentd_protocol::TaskState::Completed
                        | agentd_protocol::TaskState::Failed
                        | agentd_protocol::TaskState::Cancelled
                )
            })
            .map(|(i, _)| i)
            .collect();
        // Oldest first (lower index = older since we push at end).
        to_remove.sort();
        while self.entries.len() > TASK_REGISTRY_CAP {
            match to_remove.first().copied() {
                Some(i) => {
                    self.entries.remove(i);
                    to_remove.remove(0);
                    for x in to_remove.iter_mut() {
                        *x = x.saturating_sub(1);
                    }
                }
                None => break, // everything live; nothing to evict
            }
        }
    }
}

#[derive(Default)]
struct PtyInputCapture {
    buf: String,
    /// 0 = not in an escape; 1 = saw ESC; 2 = saw ESC[ (CSI); 3 = saw ESC O (SS3).
    esc: u8,
    triggered: bool,
}

impl SessionEntry {
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::SeqCst)
    }
    /// Cheap async read of the session's current SessionState —
    /// used by the loop scheduler to skip firing into a terminal
    /// session.
    pub async fn snapshot_state(&self) -> agentd_protocol::SessionState {
        self.summary.read().await.state
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
            self.ring.extend(&bytes[bytes.len() - PTY_RING_CAP..]);
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
    adapter_runtime_dir: PathBuf,
    sessions: RwLock<HashMap<String, Arc<SessionEntry>>>,
    groups: RwLock<HashMap<String, Arc<GroupEntry>>>,
    broadcast: broadcast::Sender<BroadcastMsg>,
    /// Recurring-prompt loops attached to sessions. The scheduler
    /// task (`crate::loops::run_scheduler`) iterates these.
    pub(crate) loops: Arc<crate::loops::LoopRegistry>,
}

impl SessionManager {
    pub async fn new(
        storage: Arc<Storage>,
        config: Arc<Config>,
        runtime_dir: PathBuf,
    ) -> Result<Self> {
        let summaries = storage.list_summaries()?;
        let mut sessions = HashMap::new();
        for s in summaries {
            // Preserve the prior state in the entry. `resume_running_sessions`
            // (called from main after construction) tries to respawn each
            // resumable session and falls back to marking Errored on failure.
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
                // A title set on the previous incarnation lives in the
                // loaded summary; flagging "attempted" here stops the
                // restart from re-running title-gen for already-titled
                // sessions and is harmless for the rest.
                title_gen_attempted: AtomicBool::new(s.title.is_some()),
                pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
                tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
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
        // Load each session's persisted loops into the in-memory
        // registry. Missing or unreadable per-session loop files
        // are logged + skipped.
        let session_ids: Vec<String> = sessions.keys().cloned().collect();
        let loops = Arc::new(crate::loops::LoopRegistry::new(
            storage.data_dir().to_path_buf(),
        ));
        loops.hydrate_from_disk(&session_ids).await;
        let adapter_runtime_dir = runtime_dir.join("adapters");
        std::fs::create_dir_all(&adapter_runtime_dir).ok();
        Ok(Self {
            storage,
            config,
            adapter_runtime_dir,
            sessions: RwLock::new(sessions),
            groups: RwLock::new(groups),
            broadcast,
            loops,
        })
    }

    fn adapter_socket_path(&self, id: &str) -> PathBuf {
        self.adapter_runtime_dir.join(format!("{id}.sock"))
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
        let want_worktree = params.worktree || self.config.defaults.worktree.unwrap_or(false);
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
        let effective_cwd = worktree_path.clone().unwrap_or_else(|| cwd_path.clone());

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
            worktree: worktree_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            pending_input: false,
            last_prompt: params.prompt.clone(),
            event_count: 0,
            has_pty: false,
            mode: params.mode.clone(),
            pinned: false,
            // Negative timestamp so newer sessions sort to the top by default.
            position: -now.timestamp_millis(),
            group_id: params.group_id.clone(),
            last_pty_at_ms: None,
            automode: false,
            kind: params.kind,
        };
        self.storage.save_summary(&summary)?;

        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);
        let combined_args = {
            let mut a = adapter_cfg.args.clone();
            a.extend(params.args.clone());
            a
        };

        // Build the full env (adapter-config + user-provided + daemon
        // meta) BEFORE spawn so the adapter process inherits
        // AGENTD_SESSION_DATA_DIR / AGENTD_SESSION_KIND — not just
        // the session.start params.env. The codex adapter (and
        // claude) reads these via std::env::var, so leaving them
        // only in session.start meant their first-spawn bookkeeping
        // (originator-tagged rollout capture, session-id minting)
        // silently no-op'd; respawn already merged them in time, so
        // the bug only surfaced on initial create.
        //
        // Precedence: `[adapters.<name>].env` is the per-harness
        // baseline (operator-set default model, etc.), overridden
        // by the per-session `params.env` (explicit `agent new
        // --env KEY=VAL`), overridden in turn by daemon-meta. So a
        // CLI flag always wins over config.toml, and daemon meta
        // always wins over both.
        let mut env_with_meta = adapter_cfg.env.clone();
        for (k, v) in &params.env {
            env_with_meta.insert(k.clone(), v.clone());
        }
        env_with_meta.insert(
            "AGENTD_SESSION_DATA_DIR".to_string(),
            self.storage.session_dir(&id).to_string_lossy().to_string(),
        );
        env_with_meta.insert(
            "AGENTD_SESSION_KIND".to_string(),
            match params.kind {
                agentd_protocol::SessionKind::User => "user",
                agentd_protocol::SessionKind::Orchestrator => "orchestrator",
            }
            .to_string(),
        );

        let (adapter, info) = Adapter::spawn_reconnectable(
            params.harness.clone(),
            binary,
            combined_args,
            env_with_meta.clone(),
            self.adapter_socket_path(&id),
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
        let start_params = SessionStartParams {
            session_id: id.clone(),
            cwd: summary.cwd.clone(),
            prompt: params.prompt.clone(),
            model: summary.model.clone(),
            mode: params.mode.clone(),
            pty_size: params.pty_size,
            env: env_with_meta,
            args: params.args.clone(),
        };
        // Persist so a daemon restart can re-spawn with the same shape.
        let _ = self.storage.save_start_params(&id, &start_params);
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
            title_gen_attempted: AtomicBool::new(summary.title.is_some()),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
        });

        // Record the user's initial prompt as the first transcript event so
        // the transcript reads coherently (user → assistant) for every adapter.
        // Auto-title is triggered inside handle_event for any User message.
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

        self.sessions
            .write()
            .await
            .insert(id.clone(), entry.clone());

        // Spawn drain task for adapter messages.
        let manager = self.clone();
        let entry_for_drain = entry.clone();
        tokio::spawn(async move {
            manager.drain_adapter(entry_for_drain, msg_rx).await;
        });

        // Broadcast initial state.
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: summary,
            }));

        Ok(id)
    }

    /// Ensure the daemon-owned orchestrator session exists. Called once
    /// at startup, after `resume_running_sessions`. Three outcomes:
    ///
    /// 1. Orchestrator disabled in config → no-op.
    /// 2. Orchestrator session already exists (created on a previous
    ///    run and rehydrated) → no-op; `resume_running_sessions`
    ///    already brought it back online.
    /// 3. No orchestrator session yet → create one with the configured
    ///    harness. Failures (binary missing, capability negotiation,
    ///    initial prompt rejected) are logged and the daemon proceeds
    ///    without an orchestrator — clients see palette mode.
    pub async fn ensure_orchestrator(self: Arc<Self>) {
        let harness = match self.config.orchestrator.effective_harness() {
            Some(h) => h.to_string(),
            None => {
                tracing::info!("orchestrator disabled in config");
                return;
            }
        };
        // Already have a *live* one? Persisted summaries with
        // `kind: Orchestrator` in any non-terminal state are reused.
        // Terminal orchestrators (Errored / Done — usually from a
        // previous run when no API key was set) are left in place
        // for forensics but a fresh one is created so the user gets
        // a working panel.
        {
            let guard = self.sessions.read().await;
            for entry in guard.values() {
                let s = entry.summary.read().await;
                if s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal() {
                    tracing::info!(
                        id = %s.id,
                        harness = %s.harness,
                        state = ?s.state,
                        "orchestrator session already exists"
                    );
                    return;
                }
            }
        }
        // Create fresh. Use the daemon process cwd so the orchestrator's
        // shell tools resolve relative paths from wherever the user
        // started agentd. Interactive mode gives the orchestrator a
        // PTY-backed REPL — the TUI renders it in the minibuffer panel
        // and gets the line-editor / queue / slash popup polish from
        // zarvis interactive for free. The initial 80×10 pty_size is
        // a placeholder; the TUI sends a pty_resize as soon as it
        // attaches the panel.
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/".to_string());
        let params = agentd_protocol::CreateSessionParams {
            harness: harness.clone(),
            cwd,
            prompt: None,
            model: None,
            title: Some("orchestrator".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(agentd_protocol::PtySize { cols: 80, rows: 10 }),
            worktree: false,
            env: Default::default(),
            args: Vec::new(),
            kind: agentd_protocol::SessionKind::Orchestrator,
            group_id: None,
        };
        match self.create(params).await {
            Ok(id) => tracing::info!(
                id = %id,
                harness = %harness,
                "orchestrator session created"
            ),
            Err(e) => tracing::warn!(
                harness = %harness,
                error = %e,
                "orchestrator session create failed; clients fall back to palette mode"
            ),
        }
    }

    /// Re-spawn the adapter for every persisted session whose state is
    /// resumable at daemon startup. `Done` sessions stay closed; `Errored`
    /// sessions are retried because an error can mean the previous adapter or
    /// machine died rather than the underlying agent conversation ending.
    /// Each adapter
    /// receives `AGENTD_RESUME=1` in its env plus the same start params it
    /// was originally launched with (cwd, model, prompt, etc.) — the
    /// adapter decides what "resume" means for its harness. Sessions that
    /// can't be re-spawned (missing start.json, missing adapter binary,
    /// spawn failure) are marked Errored.
    pub async fn resume_running_sessions(self: Arc<Self>) {
        let ids: Vec<String> = {
            let guard = self.sessions.read().await;
            let mut v = Vec::new();
            for (id, entry) in guard.iter() {
                let s = entry.summary.read().await;
                if should_resume_on_startup(s.state) {
                    v.push(id.clone());
                }
            }
            v
        };
        for id in ids {
            if let Err(e) = self.clone().respawn(&id).await {
                tracing::warn!(session = %id, error = ?e, "resume failed; marking Errored");
                if let Some(entry) = self.get_entry(&id).await {
                    let snapshot = {
                        let mut s = entry.summary.write().await;
                        s.state = SessionState::Errored;
                        s.clone()
                    };
                    let _ = self.storage.save_summary(&snapshot);
                    let _ = self
                        .broadcast
                        .send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                }
            }
        }
    }

    /// Spawn an adapter for an already-existing session entry (i.e. on
    /// daemon restart). Reuses the start params persisted at create time
    /// and signals `AGENTD_RESUME=1` so the adapter can pull its own
    /// prior state from `AGENTD_SESSION_DATA_DIR`.
    async fn respawn(self: Arc<Self>, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {id}"))?;
        let mut start_params = self.storage.load_start_params(id)?;
        start_params
            .env
            .insert("AGENTD_RESUME".to_string(), "1".to_string());
        // Make sure the data-dir env is present even if start.json predates
        // the meta env injection.
        start_params.env.insert(
            "AGENTD_SESSION_DATA_DIR".to_string(),
            self.storage.session_dir(id).to_string_lossy().to_string(),
        );
        // Use the last-known PTY size so the resumed adapter (which
        // sizes its PTY off start_params on session.start) doesn't draw
        // its banner / resume content at the stale creation default.
        if let Some(size) = self.storage.load_pty_size(id) {
            start_params.pty_size = Some(size);
        }

        let harness = {
            let s = entry.summary.read().await;
            s.harness.clone()
        };
        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);

        match Adapter::attach(
            harness.clone(),
            self.adapter_socket_path(id),
            msg_tx.clone(),
        )
        .await
        {
            Ok((adapter, _info)) => {
                *entry.adapter.lock().await = Some(adapter);
                let snapshot = {
                    let mut s = entry.summary.write().await;
                    s.state = SessionState::Running;
                    s.pending_input = false;
                    s.clone()
                };
                let _ = self.storage.save_summary(&snapshot);
                let _ = self
                    .broadcast
                    .send(BroadcastMsg::State(StateNotificationPayload {
                        session: snapshot,
                    }));
                let manager = self.clone();
                let entry_for_drain = entry.clone();
                tokio::spawn(async move {
                    manager.drain_adapter(entry_for_drain, msg_rx).await;
                });
                tracing::info!(session = %id, %harness, "reattached adapter");
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(session = %id, %harness, error = ?e, "adapter attach failed; respawning");
            }
        }

        let adapter_cfg = self
            .config
            .adapters
            .get(&harness)
            .ok_or_else(|| anyhow!("unknown harness on resume: {harness}"))?
            .clone();
        let binary_spec = adapter_cfg
            .binary
            .clone()
            .unwrap_or_else(|| harness.clone());
        let binary = locate_binary(&binary_spec)
            .ok_or_else(|| anyhow!("adapter binary not found: {binary_spec}"))?;
        let combined_args = {
            let mut a = adapter_cfg.args.clone();
            a.extend(start_params.args.clone());
            a
        };

        // Merge `[adapters.<name>].env` underneath the persisted
        // start-params env so config.toml-driven defaults apply on
        // respawn too. Per-session env (from `agent new --env`)
        // still wins because start_params.env was constructed with
        // it on top of adapter_cfg.env at create time and gets the
        // same treatment again here.
        let respawn_env = {
            let mut e = adapter_cfg.env.clone();
            for (k, v) in &start_params.env {
                e.insert(k.clone(), v.clone());
            }
            e
        };

        let (adapter, info) = Adapter::spawn_reconnectable(
            harness.clone(),
            binary,
            combined_args,
            respawn_env,
            self.adapter_socket_path(id),
            msg_tx.clone(),
        )
        .await
        .with_context(|| format!("respawn adapter for {harness}"))?;

        // Drop stale PTY bytes from the previous incarnation BEFORE the
        // new child can start emitting. Without this, the in-memory ring
        // (rehydrated from pty.log at Manager::new) and the on-disk
        // pty.log both hold the old child's TUI state — when a TUI
        // client reconnects and calls pty_replay it gets that history
        // merged with the new child's startup escapes, and vt100 lands
        // in a weird half-rendered state (often appearing blank with
        // just a cursor) until a SIGWINCH forces a redraw.
        //
        // Adapters that advertise `supports_silent_resume` promise to
        // emit nothing on resume (zarvis does this), so we keep the
        // prior PTY history visible after a daemon restart instead of
        // wiping it.
        if !info.capabilities.supports_silent_resume {
            let mut pty = entry.pty.lock().await;
            pty.ring.clear();
            drop(pty);
            if let Err(e) = self.storage.truncate_pty_log(id) {
                tracing::warn!(session = %id, error = ?e, "truncate_pty_log on respawn failed");
            }
        }

        adapter
            .request(
                ahp_method::SESSION_START,
                serde_json::to_value(&start_params)?,
            )
            .await
            .context("adapter session.start (resume) failed")?;

        *entry.adapter.lock().await = Some(adapter.clone());

        // Notify clients that this session is alive again. A resumed
        // `Errored` session must stop looking terminal immediately so startup
        // code (notably orchestrator creation) does not treat it as dead while
        // waiting for the adapter's first Status event.
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.state = SessionState::Running;
            s.pending_input = false;
            s.clone()
        };
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));

        // Drain adapter messages just like a fresh create.
        let manager = self.clone();
        let entry_for_drain = entry.clone();
        tokio::spawn(async move {
            manager.drain_adapter(entry_for_drain, msg_rx).await;
        });

        // Force-redraw cycle for PTY-backed adapters that don't
        // silently resume. Codex / claude / shell only repaint past
        // content when their PTY's SIGWINCH fires, and the child was
        // just spawned at the cached pty_size — so any pty_resize a
        // TUI sends with the same dimensions is a kernel no-op
        // (ioctl(TIOCSWINSZ) only signals on actual size change),
        // leaving the pane stuck on whatever the child happened to
        // paint at startup (often just a banner / cursor) until the
        // user manually resizes their terminal.
        //
        // We schedule a "bump by 1 col → restore" sequence on a
        // background task. The 250 ms delay gives the child time to
        // settle into its initial draw; the two ioctls then force
        // two SIGWINCH'es, the second of which leaves the PTY at the
        // correct cached size. zarvis (silent_resume) is skipped —
        // it explicitly emits nothing on resume.
        if let Some(size) = force_redraw_size_on_resume(&info.capabilities, start_params.pty_size) {
            let manager_for_redraw = self.clone();
            let id_owned = id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(RESPAWN_REDRAW_DELAY).await;
                let bumped_cols = size.cols.saturating_add(1);
                let _ = manager_for_redraw
                    .pty_resize(&id_owned, bumped_cols, size.rows)
                    .await;
                let _ = manager_for_redraw
                    .pty_resize(&id_owned, size.cols, size.rows)
                    .await;
            });
        }

        tracing::info!(session = %id, %harness, "resumed");
        Ok(())
    }

    /// Public entry point for "bring a terminated session back to
    /// life". Used by the TUI's restart-confirm flow: the user
    /// pressed `y` on a `Done`/`Errored` session and wants to keep
    /// typing. Refuses sessions that already have a live adapter
    /// — those are running, not done.
    ///
    /// Internally just calls [`Manager::respawn`], which sets
    /// `AGENTD_RESUME=1` in the adapter env so harnesses that
    /// persist conversation state (zarvis) reload it on the new
    /// process.
    pub async fn restart(self: Arc<Self>, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        if entry.adapter.lock().await.is_some() {
            return Err(anyhow!(
                "session already has a live adapter (state is not terminal)"
            ));
        }
        self.respawn(id).await
    }

    /// Gracefully stop every live adapter. Used for intentional daemon
    /// termination; restart-oriented signals skip this so reconnectable
    /// adapters can survive the daemon process.
    pub async fn shutdown_adapters(&self) {
        let entries: Vec<Arc<SessionEntry>> = {
            let guard = self.sessions.read().await;
            guard.values().cloned().collect()
        };
        for entry in entries {
            if let Some(adapter) = entry.adapter.lock().await.clone() {
                let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
            }
        }
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
                AdapterMessage::Log {
                    session_id: _,
                    line,
                } => {
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
                    let _ = self
                        .broadcast
                        .send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                    break;
                }
            }
        }
    }

    async fn handle_event(&self, entry: &Arc<SessionEntry>, event: SessionEvent) {
        // Skip everything once the session has been deleted — the drain task
        // and the adapter can still feed us events for a beat.
        if entry.is_deleted() {
            return;
        }
        if matches!(event, SessionEvent::Reset) {
            if let Err(e) = self.storage.truncate_transcript(&entry.id) {
                tracing::warn!(session = %entry.id, error = ?e, "truncate_transcript on reset failed");
            }
            if let Err(e) = self.storage.truncate_pty_log(&entry.id) {
                tracing::warn!(session = %entry.id, error = ?e, "truncate_pty_log on reset failed");
            }
            entry.transcript_count.store(0, Ordering::Relaxed);
            entry.pty.lock().await.ring.clear();
            entry.tasks.lock().await.clear();
            let now = Utc::now();
            let snapshot = {
                let mut s = entry.summary.write().await;
                s.last_event_at = Some(now);
                s.event_count = 0;
                s.last_pty_at_ms = None;
                s.state = SessionState::AwaitingInput;
                s.pending_input = true;
                s.clone()
            };
            let _ = self.storage.save_summary(&snapshot);
            let _ = self
                .broadcast
                .send(BroadcastMsg::State(StateNotificationPayload {
                    session: snapshot,
                }));
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq: 0,
                }));
            return;
        }
        // AgentStatus is ephemeral live UI state. The CLI may render
        // inactive statuses as display-only history rows, but they
        // should not enter the structured transcript or PTY log.
        if let SessionEvent::AgentStatus(_) = &event {
            let now = Utc::now();
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
                SessionEvent::AgentStatus(_) => {}
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
                SessionEvent::Reset
                | SessionEvent::Message { .. }
                | SessionEvent::Reasoning { .. }
                | SessionEvent::ToolUse { .. }
                | SessionEvent::ToolResult { .. }
                | SessionEvent::Diff { .. }
                | SessionEvent::Pty { .. }
                | SessionEvent::ToolApprovalRequest { .. }
                | SessionEvent::TaskStart { .. }
                | SessionEvent::TaskBackgrounded { .. }
                | SessionEvent::TaskEnd { .. }
                | SessionEvent::ContextCompacted { .. }
                | SessionEvent::EditorState { .. } => {
                    // Task-lifecycle, editor-state, and compaction
                    // events are recorded by other handlers — they
                    // don't move the session's top-level state.
                }
            }
            let snapshot = s.clone();
            drop(s);
            let _ = self.storage.save_summary(&snapshot);
        }
        // Update the per-session task registry from lifecycle events
        // so `session.list_tasks` has live state to return.
        match &event {
            SessionEvent::TaskStart {
                call_id,
                tool,
                args_summary,
            } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.upsert_start(
                    call_id.clone(),
                    tool.clone(),
                    args_summary.clone(),
                    now.timestamp_millis(),
                );
            }
            SessionEvent::TaskBackgrounded { call_id } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.mark_backgrounded(call_id, now.timestamp_millis());
            }
            SessionEvent::TaskEnd {
                call_id,
                ok,
                output_preview,
            } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.mark_end(call_id, *ok, output_preview.clone(), now.timestamp_millis());
            }
            _ => {}
        }

        // Auto-title hook: trigger on the FIRST User message we record
        // regardless of where it came from (the daemon's create()
        // prompt-as-event, send_input, or an adapter that re-emits the
        // user's typed prompt — zarvis interactive does this). The
        // `title_gen_attempted` AtomicBool inside maybe_spawn_auto_title
        // ensures only the first one wins.
        if let SessionEvent::Message {
            role: MessageRole::User,
            text,
        } = &event
        {
            self.maybe_spawn_auto_title(entry.clone(), text.clone());
        }

        let _ = self
            .broadcast
            .send(BroadcastMsg::Event(EventNotificationPayload {
                session_id: entry.id.clone(),
                at: now,
                event,
                seq,
            }));

        // Also push a state snapshot so list views update without explicit refresh.
        let summary = entry.summary().await;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
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
        // Auto-title is triggered inside handle_event for any User message.
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

    /// Kick off auto-title generation in the background if (a) the user
    /// has not set a title yet (i.e. the `title` field is `None` — the
    /// hash shown in the UI is just `primary_label`'s display fallback),
    /// (b) we haven't already attempted this incarnation, (c) the
    /// prompt is non-empty, and (d) the zarvis adapter binary is
    /// configured + locatable. Silently no-ops on any miss.
    fn maybe_spawn_auto_title(&self, entry: Arc<SessionEntry>, prompt: String) {
        // Cheap checks first so we don't burn the per-session attempt
        // budget (the AtomicBool flip is one-way until a daemon
        // restart) on inputs that wouldn't have produced a title
        // anyway.
        if prompt.trim().is_empty() {
            return;
        }
        let Some(zarvis_cfg) = self.config.adapters.get("zarvis").cloned() else {
            return;
        };
        let binary_spec = zarvis_cfg
            .binary
            .clone()
            .unwrap_or_else(|| "agentd-adapter-zarvis".to_string());
        let Some(binary) = locate_binary(&binary_spec) else {
            return;
        };
        // Now claim the attempt. `swap` is the one place we mark this
        // session as "tried"; the user-renamed path is handled by
        // `title_gen_attempted` being initialized to `title.is_some()`
        // when the entry is constructed (both at create-time and when
        // loaded from disk on daemon restart).
        if entry.title_gen_attempted.swap(true, Ordering::SeqCst) {
            return;
        }
        let storage = self.storage.clone();
        let broadcast_tx = self.broadcast.clone();
        tokio::spawn(async move {
            generate_auto_title(binary, entry, prompt, storage, broadcast_tx).await;
        });
    }

    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Capture the user's first typed line for auto-title (shell /
        // claude / codex interactive sessions don't echo a Message event
        // back to the daemon; this is the only place we see their input).
        self.feed_pty_input_capture(&entry, &bytes).await;
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionPtyInputParams::from_bytes(
            id, &bytes,
        ))?;
        adapter
            .request(ahp_method::SESSION_PTY_INPUT, params)
            .await?;
        Ok(())
    }

    /// Feed PTY-input bytes through a minimal terminal-input parser
    /// (printable ASCII + backspace + CR/LF; CSI/SS3 sequences skipped).
    /// On the first CR/LF, hand the accumulated line to the auto-title
    /// path. After the first trigger this becomes a no-op for the
    /// session's lifetime.
    async fn feed_pty_input_capture(&self, entry: &Arc<SessionEntry>, bytes: &[u8]) {
        // Cheap early-outs before taking the per-session lock.
        if entry.title_gen_attempted.load(Ordering::SeqCst) {
            return;
        }
        let mut cap = entry.pty_input_capture.lock().await;
        if cap.triggered {
            return;
        }
        for &b in bytes {
            match cap.esc {
                0 => match b {
                    b'\r' | b'\n' => {
                        let s = cap.buf.trim().to_string();
                        cap.triggered = true;
                        cap.buf.clear();
                        drop(cap);
                        if s.chars().count() >= 2 {
                            self.maybe_spawn_auto_title(entry.clone(), s);
                        }
                        return;
                    }
                    0x1b => cap.esc = 1,
                    0x08 | 0x7f => {
                        cap.buf.pop();
                    }
                    _ if (0x20..0x7f).contains(&b) => cap.buf.push(b as char),
                    _ => {}
                },
                1 => match b {
                    b'[' => cap.esc = 2,
                    b'O' => cap.esc = 3,
                    _ => cap.esc = 0,
                },
                2 => {
                    // CSI: parameter bytes + final byte in `@`..=`~`.
                    if (0x40..=0x7e).contains(&b) {
                        cap.esc = 0;
                    }
                }
                3 => {
                    // SS3: one byte.
                    cap.esc = 0;
                }
                _ => cap.esc = 0,
            }
        }
    }

    pub async fn pty_resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = PtySize { cols, rows };
        // Dedup: if the adapter's PTY is already at this size, skip
        // the SIGWINCH. A no-op resize on a normal-screen TUI like
        // codex still causes the child to redraw its viewport (which
        // for codex means re-emitting its full transcript), so every
        // spurious resize looks like a "history replay" to the user.
        // Sources of spurious resizes: TUI bootstrap calling
        // `pty_resize` with the same dims it already sent, and
        // multiple SIGWINCH'd frames during a terminal-window drag
        // that all land on the same final size.
        {
            let mut pty = entry.pty.lock().await;
            if pty.size == Some(size) {
                return Ok(());
            }
            pty.size = Some(size);
        }
        // Cache the size so the next daemon respawn can re-spawn the
        // adapter's PTY at the right dimensions from the start.
        if let Err(e) = self.storage.save_pty_size(id, size) {
            tracing::warn!(session = %id, error = ?e, "save_pty_size failed");
        }
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
        adapter
            .request(ahp_method::SESSION_INTERRUPT, params)
            .await?;
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

        // Loops are attached to the session — drop them from the
        // in-memory registry. The on-disk loops.json sits inside
        // sessions/<id>/ so it goes with the next call.
        self.loops.drop_session(id).await;

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
                let Some(prev_region) = prev else {
                    return Ok(());
                };
                self.move_session_into_region(
                    &me.id,
                    &prev_region,
                    RegionEdge::Bottom,
                    &all_sessions,
                )
                .await
            }
            MoveDirection::Down => {
                if pos_in_region + 1 < region.len() {
                    let other = region[pos_in_region + 1];
                    return self.swap_session_positions(&me.id, &other.id).await;
                }
                // At bottom of region — try to enter the next region.
                let next = region_below(me.group_id.as_deref(), &all_groups);
                let Some(next_region) = next else {
                    return Ok(());
                };
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
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snap_a,
            }));
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snap_b,
            }));
        Ok(())
    }

    /// Re-tag a session into a new region (group_id) and set its position
    /// so it lands at the top or bottom of that region.
    /// Public wrapper around [`move_session_into_region`] for clients
    /// that want to change a session's group membership (or ungroup
    /// it) without first having to fetch the sessions list themselves.
    pub async fn set_session_group(
        &self,
        session_id: &str,
        new_group_id: Option<String>,
        position: agentd_protocol::SessionGroupPosition,
    ) -> Result<()> {
        let all_sessions = self.list().await;
        let edge = match position {
            agentd_protocol::SessionGroupPosition::Top => RegionEdge::Top,
            agentd_protocol::SessionGroupPosition::Bottom => RegionEdge::Bottom,
        };
        self.move_session_into_region(session_id, &new_group_id, edge, &all_sessions)
            .await
    }

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
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
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
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: summary,
            }));
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
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snapshot,
            }));
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
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snapshot,
            }));
        Ok(())
    }

    /// Delete a group. When `delete_members` is false (default), member
    /// sessions are orphaned: their `group_id` clears to `None` and they
    /// survive. When true, every member session is fully deleted first
    /// (adapter killed, on-disk session dir removed, worktree torn down)
    /// before the group itself is removed.
    pub async fn delete_group(&self, id: &str, delete_members: bool) -> Result<()> {
        // Collect member ids BEFORE we drop the group entry so we don't
        // race with a concurrent set_session_group that might re-parent
        // them under a different group while we're working.
        let member_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            let mut ids = Vec::new();
            for (sid, entry) in sessions.iter() {
                let s = entry.summary.read().await;
                if s.group_id.as_deref() == Some(id) {
                    ids.push(sid.clone());
                }
            }
            ids
        };

        let entry = self.groups.write().await.remove(id);
        if entry.is_none() {
            return Err(anyhow!("group not found: {}", id));
        }

        if delete_members {
            // Cascade-delete: tear down each member session. Errors are
            // logged but don't abort the cascade — a single broken
            // session shouldn't strand the rest in a now-missing group.
            for sid in &member_ids {
                if let Err(e) = self.delete(sid).await {
                    tracing::warn!(
                        group = %id,
                        session = %sid,
                        error = %e,
                        "group cascade-delete: member delete failed",
                    );
                }
            }
        } else {
            // Orphan members: clear their group_id and rebroadcast.
            for sid in &member_ids {
                let Some(s_entry) = self.sessions.read().await.get(sid).cloned() else {
                    continue;
                };
                let snapshot = {
                    let mut s = s_entry.summary.write().await;
                    s.group_id = None;
                    s.clone()
                };
                let _ = self.storage.save_summary(&snapshot);
                let _ = self
                    .broadcast
                    .send(BroadcastMsg::State(StateNotificationPayload {
                        session: snapshot,
                    }));
            }
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
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snap_a,
            }));
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snap_b,
            }));
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

    pub async fn tool_decision(&self, id: &str, call_id: String, decision: String) -> Result<()> {
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

    /// Snapshot the per-session task registry (running, backgrounded,
    /// and recent terminal states). Returns an empty list when the
    /// session has no entry — adapters that don't emit `TaskStart`
    /// (claude / codex / shell today) simply never populate it.
    pub async fn loop_create(
        &self,
        params: agentd_protocol::LoopCreateParams,
    ) -> Result<agentd_protocol::Loop> {
        // Reject on unknown session — the daemon's source of truth
        // for "is this session real" is sessions map.
        if self.get_entry(&params.session_id).await.is_none() {
            return Err(anyhow!("session not found: {}", params.session_id));
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let next = crate::loops::next_fire_after_ms(&params.spec, now_ms);
        let l = agentd_protocol::Loop {
            id: String::new(), // assigned in registry
            session_id: params.session_id,
            spec: params.spec,
            prompt: params.prompt,
            created_at_ms: now_ms,
            next_fire_at_ms: next,
            expires_at_ms: params.expires_at_ms,
            last_fired_at_ms: None,
            fire_count: 0,
        };
        self.loops.create(l).await
    }

    pub async fn loop_list(&self, session_id: Option<&str>) -> Vec<agentd_protocol::Loop> {
        self.loops.list(session_id).await
    }

    pub async fn loop_update(
        &self,
        params: agentd_protocol::LoopUpdateParams,
    ) -> Result<agentd_protocol::Loop> {
        self.loops
            .update(
                &params.loop_id,
                params.spec,
                params.prompt,
                params.expires_at_ms,
            )
            .await
    }

    pub async fn loop_remove(&self, loop_id: &str) -> Result<()> {
        self.loops.remove(loop_id).await
    }

    pub async fn list_tasks(&self, id: &str) -> Result<Vec<agentd_protocol::TaskInfo>> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let g = entry.tasks.lock().await;
        Ok(g.snapshot())
    }

    /// Forward a client-initiated tool action (`"kill"` / `"background"`)
    /// to the adapter. Adapters that don't know the action ignore it
    /// with a debug log; adapters that don't know the `call_id`
    /// likewise no-op. No daemon-side state changes — the adapter is
    /// authoritative for the running-tasks registry.
    pub async fn tool_action(&self, id: &str, call_id: String, action: String) -> Result<()> {
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
        let params = serde_json::to_value(&agentd_protocol::SessionToolActionParams {
            session_id: id.to_string(),
            call_id,
            action,
        })?;
        adapter
            .request(ahp_method::SESSION_TOOL_ACTION, params)
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

/// Shell out to `agentd-adapter-zarvis --title-mode "<prompt>"`, capture
/// stdout, and apply the title to the session summary. Best-effort:
/// any failure (zarvis missing keys, network error, non-zero exit,
/// empty output) leaves the session's title unset.
async fn generate_auto_title(
    binary: PathBuf,
    entry: Arc<SessionEntry>,
    prompt: String,
    storage: Arc<Storage>,
    broadcast: tokio::sync::broadcast::Sender<BroadcastMsg>,
) {
    use std::process::Stdio;
    let output = tokio::process::Command::new(&binary)
        .arg("--title-mode")
        .arg(&prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    let out = match output {
        Ok(o) => o,
        Err(e) => {
            tracing::warn!(error = ?e, "auto-title spawn failed");
            return;
        }
    };
    if !out.status.success() {
        tracing::info!(
            session = %entry.id,
            stderr = %String::from_utf8_lossy(&out.stderr),
            "auto-title exit non-zero; skipping",
        );
        return;
    }
    let title = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if title.is_empty() {
        return;
    }
    if entry.is_deleted() {
        return;
    }
    let snapshot = {
        let mut s = entry.summary.write().await;
        // Don't clobber a title the user set after we kicked this off.
        if s.title
            .as_ref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false)
        {
            return;
        }
        s.title = Some(title.clone());
        s.clone()
    };
    if let Err(e) = storage.save_summary(&snapshot) {
        tracing::warn!(session = %entry.id, error = ?e, "auto-title save_summary failed");
        return;
    }
    let _ = broadcast.send(BroadcastMsg::State(StateNotificationPayload {
        session: snapshot,
    }));
    tracing::info!(session = %entry.id, %title, "auto-title applied");
}

/// Decide whether to schedule the bump+restore SIGWINCH cycle after a
/// session.start succeeds on respawn. Returns the size to restore to
/// (we always restore to the cached size, then bump by one column for
/// the first leg of the cycle). Returns `None` when no force-redraw
/// is warranted:
///   * the adapter advertises `supports_silent_resume` (zarvis paints
///     nothing on resume — any forced SIGWINCH would corrupt its
///     custom render);
///   * no cached pty_size to restore to (fresh creates skip this);
///   * the adapter doesn't expose a PTY at all.
fn force_redraw_size_on_resume(
    caps: &agentd_protocol::Capabilities,
    cached: Option<agentd_protocol::PtySize>,
) -> Option<agentd_protocol::PtySize> {
    if caps.supports_silent_resume {
        return None;
    }
    if !caps.supports_pty {
        return None;
    }
    let size = cached?;
    if size.cols == 0 || size.rows == 0 {
        return None;
    }
    Some(size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{Capabilities, PtySize};

    #[test]
    fn startup_resume_retries_errored_sessions() {
        assert!(should_resume_on_startup(SessionState::Pending));
        assert!(should_resume_on_startup(SessionState::Running));
        assert!(should_resume_on_startup(SessionState::AwaitingInput));
        assert!(should_resume_on_startup(SessionState::Paused));
        assert!(should_resume_on_startup(SessionState::Errored));
        assert!(!should_resume_on_startup(SessionState::Done));
    }

    fn pty_caps() -> Capabilities {
        Capabilities {
            supports_pty: true,
            supports_silent_resume: false,
            ..Default::default()
        }
    }

    /// Regression: codex / claude / shell sessions painted only their
    /// startup banner after a daemon restart because the PTY was
    /// spawned at the cached size and no SIGWINCH ever fired (kernel
    /// dedup on `ioctl(TIOCSWINSZ)` when new size == current size).
    /// The respawn path must schedule a bump+restore for these.
    #[test]
    fn force_redraw_runs_for_pty_adapters_with_cached_size() {
        let caps = pty_caps();
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(
            force_redraw_size_on_resume(&caps, size),
            Some(PtySize {
                cols: 160,
                rows: 50
            })
        );
    }

    /// Zarvis advertises `supports_silent_resume = true` because its
    /// `interactive.rs` deliberately paints nothing on resume — the
    /// PTY ring carries the prior screen forward and the next
    /// keystroke triggers a redraw. A forced SIGWINCH here would
    /// double-paint the editor pane and confuse the line editor's
    /// stored cursor.
    #[test]
    fn force_redraw_skipped_for_silent_resume_adapters() {
        let mut caps = pty_caps();
        caps.supports_silent_resume = true;
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(force_redraw_size_on_resume(&caps, size), None);
    }

    /// No cached size on disk (e.g., fresh session never had its
    /// pty_size persisted) → nothing to restore to, skip the redraw.
    /// The TUI's normal first-render pty_resize handles sizing.
    #[test]
    fn force_redraw_skipped_without_cached_size() {
        let caps = pty_caps();
        assert_eq!(force_redraw_size_on_resume(&caps, None), None);
    }

    /// Headless / non-PTY adapters (anything zarvis-headless-only or
    /// future structured-only harnesses) shouldn't get a SIGWINCH.
    #[test]
    fn force_redraw_skipped_for_non_pty_adapters() {
        let caps = Capabilities {
            supports_pty: false,
            supports_silent_resume: false,
            ..Default::default()
        };
        let size = Some(PtySize {
            cols: 160,
            rows: 50,
        });
        assert_eq!(force_redraw_size_on_resume(&caps, size), None);
    }

    /// Degenerate size (0×anything or anything×0) is unrepresentable
    /// to `ioctl(TIOCSWINSZ)` — skip instead of forwarding garbage.
    #[test]
    fn force_redraw_skipped_for_degenerate_size() {
        let caps = pty_caps();
        assert_eq!(
            force_redraw_size_on_resume(&caps, Some(PtySize { cols: 0, rows: 50 })),
            None
        );
        assert_eq!(
            force_redraw_size_on_resume(&caps, Some(PtySize { cols: 160, rows: 0 })),
            None
        );
    }
}
