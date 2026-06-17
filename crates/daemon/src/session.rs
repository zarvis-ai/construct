//! Session management: lifecycle, adapter binding, event ingestion, broadcast.

use crate::adapter::{locate_binary, Adapter, AdapterMessage};
use crate::config::Config;
use crate::storage::Storage;
use crate::worktree;
use agentd_protocol::{
    ahp_method, ClientView, CreateSessionParams, DeletedNotificationPayload,
    EventNotificationPayload, GroupDeletedNotificationPayload, GroupStateNotificationPayload,
    GroupSummary, HarnessInfo, MessageRole, MoveDirection, PtyReplayResult, PtySize,
    SessionAttachClipboardParams, SessionAttachClipboardResult, SessionDetail,
    SessionEmitEventParams, SessionEvent, SessionStartParams, SessionState, SessionSummary,
    SessionWidgetDeleteParams, StateNotificationPayload, TimestampedEvent, TranscriptResult,
};
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, RwLock};

const BROADCAST_CAP: usize = 4096;
const ADAPTER_DRAIN_CAP: usize = 256;
/// Tail size (in bytes) of each session's `pty.log` returned to a TUI
/// client on attach. The client feeds these bytes through a vt100 parser
/// that retains only `SCROLLBACK_MAX` rows of formatted scrollback
/// (`crates/cli/src/app.rs`), so the practical scrollback ceiling is the
/// row cap, not this byte cap — this number just needs to be generous
/// enough that the row cap is the binding constraint on typical
/// codex/claude/antigravity sessions. 8 MiB covers ~40-80k rows of dense
/// PTY content, well above the vt100 row budget.
const PTY_REPLAY_CAP: usize = 8 * 1024 * 1024;
/// The post-resume force-redraw (a bump+restore SIGWINCH that nudges a
/// non-silent-resume child into repainting) waits until the child's PTY
/// output has *settled* rather than firing on a fixed delay. A fixed
/// delay was too short for a slow resume — codex loading a large
/// conversation — so the bump landed before the child had drawn anything
/// and the pane stayed blank until the user manually resized. We poll the
/// child's last-output timestamp every [`RESPAWN_REDRAW_POLL`] and fire
/// the bump once output has been quiet for [`RESPAWN_REDRAW_SETTLE`] (the
/// child finished its resume draw), or after [`RESPAWN_REDRAW_MAX_WAIT`]
/// as a hard cap.
const RESPAWN_REDRAW_POLL: Duration = Duration::from_millis(100);
const RESPAWN_REDRAW_SETTLE: Duration = Duration::from_millis(400);
const RESPAWN_REDRAW_MAX_WAIT: Duration = Duration::from_secs(6);

/// Whether the post-resume force-redraw should fire now: the child has
/// produced PTY output and then gone quiet for [`RESPAWN_REDRAW_SETTLE`],
/// or [`RESPAWN_REDRAW_MAX_WAIT`] has elapsed (so a child that streams
/// forever, or never draws, still gets a redraw). `last_pty_at_ms` is the
/// child's most recent PTY-output timestamp (`None` = nothing yet).
fn resume_redraw_ready(last_pty_at_ms: Option<i64>, now_ms: i64, elapsed: Duration) -> bool {
    if elapsed >= RESPAWN_REDRAW_MAX_WAIT {
        return true;
    }
    match last_pty_at_ms {
        Some(t) => now_ms.saturating_sub(t) >= RESPAWN_REDRAW_SETTLE.as_millis() as i64,
        None => false,
    }
}
const MAX_CLIPBOARD_ATTACHMENT_BYTES: usize = 50 * 1024 * 1024;
const ENV_GLOBAL_MEMORY_FILE: &str = "CONSTRUCT_GLOBAL_MEMORY_FILE";
const ENV_PROJECT_MEMORY_FILE: &str = "CONSTRUCT_PROJECT_MEMORY_FILE";
const ENV_PROJECT_ID: &str = "CONSTRUCT_PROJECT_ID";
const WIDGET_WATCH_INTERVAL: Duration = Duration::from_millis(700);

fn should_resume_on_startup(state: SessionState) -> bool {
    !matches!(state, SessionState::Done)
}

fn sanitized_file_stem(name: &str) -> Option<String> {
    let raw = std::path::Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let mut out = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
        if out.len() >= 48 {
            break;
        }
    }
    let out = out.trim_matches(['-', '.', '_']).to_string();
    (!out.is_empty()).then_some(out)
}

fn extension_for_attachment(filename: Option<&str>, mime: Option<&str>, bytes: &[u8]) -> String {
    if let Some(ext) = filename
        .and_then(|f| std::path::Path::new(f).extension())
        .and_then(|s| s.to_str())
        .map(|s| sanitize_extension(s))
        .filter(|s| !s.is_empty())
    {
        return ext;
    }
    if let Some(ext) = mime.and_then(extension_for_mime) {
        return ext.to_string();
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png".to_string()
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg".to_string()
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif".to_string()
    } else if bytes.starts_with(b"%PDF-") {
        "pdf".to_string()
    } else if std::str::from_utf8(bytes).is_ok() {
        "txt".to_string()
    } else {
        "bin".to_string()
    }
}

fn extension_for_mime(mime: &str) -> Option<&'static str> {
    match mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some("png"),
        "image/jpeg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/markdown" => Some("md"),
        "application/json" => Some("json"),
        _ => None,
    }
}

fn sanitize_extension(ext: &str) -> String {
    ext.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(12)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn is_user_session_kind(s: &SessionSummary) -> bool {
    matches!(s.kind, agentd_protocol::SessionKind::User)
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

/// True if the group `id` exists and is currently collapsed.
fn group_collapsed(id: &str, groups: &[GroupSummary]) -> bool {
    groups
        .iter()
        .find(|g| g.id == id)
        .map(|g| g.collapsed)
        .unwrap_or(false)
}

/// Like [`region_above`], but skips over collapsed groups. A collapsed
/// project hides its member sessions, so reordering a visible session past it
/// should jump the entire project in one step rather than swapping with each
/// hidden member. Returns the first non-collapsed region above (the ungrouped
/// region is never collapsed), or `None` if there is nothing above.
fn region_above_skipping_collapsed(
    region: Option<&str>,
    groups: &[GroupSummary],
) -> Option<Option<String>> {
    let mut target = region_above(region, groups);
    loop {
        match target {
            Some(Some(gid)) if group_collapsed(&gid, groups) => {
                target = region_above(Some(gid.as_str()), groups);
            }
            other => return other,
        }
    }
}

/// Like [`region_below`], but skips over collapsed groups so a reorder jumps
/// the whole collapsed project in one step. See
/// [`region_above_skipping_collapsed`].
fn region_below_skipping_collapsed(
    region: Option<&str>,
    groups: &[GroupSummary],
) -> Option<Option<String>> {
    let mut target = region_below(region, groups);
    loop {
        match target {
            Some(Some(gid)) if group_collapsed(&gid, groups) => {
                target = region_below(Some(gid.as_str()), groups);
            }
            other => return other,
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
    /// Aggregate state for the remote WS transport. Emitted by
    /// `server::handle_ws_connection` on every accept/drop so the
    /// local TUI can show a "remote attached" badge.
    RemoteState(agentd_protocol::RemoteStateNotificationPayload),
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
    /// "Active client wins" PTY-size policy. A POSIX PTY can only
    /// have one size, so when both the TUI and the remote web
    /// client are attached to the same session we resize the OS
    /// PTY to whichever kind most recently sent a `pty_input` or
    /// `pty_resize`. Switching attention (typing on TUI →
    /// `last_active = Tui`; typing on phone → `Remote`) flips the
    /// size. The other client's view temporarily looks wrong until
    /// they re-engage. See `SessionManager::note_pty_activity`.
    pub pty_client_policy: std::sync::Mutex<PtyClientPolicy>,
}

/// Tracking state for the per-session "active client wins" PTY
/// resize policy. Kept on `SessionEntry`. `std::sync::Mutex` (not
/// tokio) is deliberate — every critical section is tiny and we
/// never want to hold this across an .await.
#[derive(Debug, Default)]
pub struct PtyClientPolicy {
    /// Last viewport the TUI client claimed for this session.
    pub tui_size: Option<(u16, u16)>,
    /// Last viewport a remote (WS) client claimed for this session.
    pub remote_size: Option<(u16, u16)>,
    /// The kind whose viewport currently owns the OS PTY size. On
    /// any further activity from a *different* kind, the daemon
    /// resizes to that kind's stored viewport.
    pub last_active: Option<crate::server::ClientKind>,
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
    last_was_cr: bool,
}

fn should_record_pty_user_message(harness: &str) -> bool {
    matches!(harness, "claude" | "antigravity")
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

/// Per-session PTY metadata. Used to hold the last known PTY dimensions
/// (so a fresh TUI attach can size its parsers correctly) and previously
/// also a 256 KiB in-memory ring of bytes for replay. Scrollback is now
/// served from the on-disk `pty.log` tail (see `pty_replay`), so the
/// in-memory ring is gone and this is just the size.
#[derive(Default)]
struct PtyState {
    size: Option<PtySize>,
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

#[derive(Debug, Clone)]
struct WidgetSnapshot {
    files: HashMap<String, agentd_protocol::UiPanel>,
}

fn ui_panel_changed(
    previous: Option<&agentd_protocol::UiPanel>,
    next: &agentd_protocol::UiPanel,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.source != next.source
        || previous.title != next.title
        || previous.created_at_ms != next.created_at_ms
        || previous.placement != next.placement
        || previous.markdown != next.markdown
}

impl WidgetSnapshot {
    fn read(storage: &Storage, session_id: &str) -> Self {
        let files = storage
            .read_widgets(session_id)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %session_id, error = ?e, "read widgets failed");
                Vec::new()
            })
            .into_iter()
            .map(|panel| (panel.id.clone(), panel))
            .collect();
        Self { files }
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
    /// Set by [`Self::shutdown_adapters`] before it tells each
    /// adapter to exit, so the `drain_adapter` task can tell the
    /// resulting `AdapterMessage::Closed` events apart from real
    /// adapter crashes and *not* transition the session to `Done`.
    /// Sessions need to keep their pre-shutdown state on disk so
    /// `resume_running_sessions` picks them up on the next boot —
    /// otherwise a graceful `kill -TERM` of the daemon would mark
    /// every live session terminal and skip them on restart.
    is_shutting_down: AtomicBool,
    /// Remote-WS transport: `None` until `start_remote` is called
    /// (either by env-var-at-boot in `main.rs` or by the
    /// `remote.start` IPC method invoked from the TUI's
    /// `/remote-control` slash). Subsequent calls return the same
    /// `RemoteState` so the URL + token stay stable for the
    /// daemon's lifetime.
    ///
    /// Uses `std::sync::Mutex` deliberately — we never want to
    /// hold this guard across an `.await`, so an explicitly non-
    /// `Send` guard makes the compiler enforce that invariant.
    /// All critical sections are tiny snapshot reads / single
    /// writes.
    remote: std::sync::Mutex<Option<RemoteHandle>>,
    /// Outbound side of the channel to the remote supervisor task
    /// (`crate::remote_supervisor::run`). `start_remote` posts
    /// requests here and awaits the reply rather than spawning
    /// `serve_ws_on` directly — see the comment on
    /// `remote_supervisor` for why that indirection is mandatory.
    remote_starter: tokio::sync::mpsc::UnboundedSender<crate::remote_supervisor::SupervisorMsg>,
    /// Where the supervisor writes (and the next-boot supervisor
    /// reads) the `RemoteSnapshot`. Lives under `runtime_dir`
    /// because it's tightly coupled to the live cloudflared PID;
    /// `XDG_RUNTIME_DIR` is the natural home for such files.
    remote_snapshot_path: PathBuf,
    /// Sender side of the daemon-restart channel. Holding `Some`
    /// means `daemon.restart` has been issued and main's
    /// `tokio::select!` should observe it and `exec()` the current
    /// binary. `RestartCommand` carries the resolved exe path so
    /// the reply to the RPC caller can echo what's about to load.
    restart_tx: tokio::sync::mpsc::UnboundedSender<RestartCommand>,
    /// Dev-only: when `Some`, the remote web server serves
    /// `index.html` + `static/*` from this directory (with a live-reload
    /// poller injected) instead of the binary's embedded assets. Set via
    /// the `dev.set_assets` IPC method (debug builds only) or the
    /// `CONSTRUCT_ASSETS_DIR` env var at boot. Lets you iterate on the web
    /// UI in a worktree against a running daemon without rebuilding.
    dev_assets: std::sync::Mutex<Option<PathBuf>>,
    widget_snapshots: tokio::sync::Mutex<HashMap<String, WidgetSnapshot>>,
    /// Monotonic id handed to each client connection so its current
    /// view can be tracked and cleared on disconnect.
    next_conn_id: AtomicU64,
    /// Which session + surface each live client connection is currently
    /// viewing (`conn_id -> (session_id, view)`). Drives
    /// `chat_viewer_active`, which the `AskUserQuestion` chat-gate hook
    /// queries. A `std::sync::Mutex` is fine — every critical section is a
    /// tiny insert/remove/scan never held across an `.await`.
    conn_views: std::sync::Mutex<HashMap<u64, (String, ClientView)>>,
}

/// Payload of a `daemon.restart` request, sent from the IPC
/// handler to the main loop. Main resolves the exe path + args
/// before the runtime tear-down so the reply can echo what's
/// about to load.
#[derive(Debug, Clone)]
pub struct RestartCommand {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

/// Executable path captured once at daemon startup, before any
/// on-disk binary upgrade can unlink the original inode. See
/// [`capture_startup_exe`].
static STARTUP_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Record the daemon's executable path at startup. Call once,
/// early in `main`, before serving any requests.
///
/// This exists because `std::env::current_exe()` is unreliable at
/// *restart* time: the primary `/agentd restart` use case is
/// picking up an upgraded binary, and upgrades replace the file
/// via atomic rename (a new inode at the same path). On Linux,
/// `current_exe()` reads `/proc/self/exe`, which after that
/// replacement resolves to `"/path/agentd (deleted)"` — and
/// `exec()`ing that literal path fails with `ENOENT`, so the
/// daemon would never come back. Capturing the clean path at
/// startup (when the file definitely still exists) and `exec()`ing
/// *that* loads the new binary now sitting at the same path.
pub fn capture_startup_exe() {
    if let Ok(p) = std::env::current_exe() {
        let _ = STARTUP_EXE.set(p);
    }
}

/// Validate a caller-supplied restart binary: resolve it to an absolute
/// path (relative paths resolve against the *daemon's* cwd), confirm it
/// exists, is a regular file, and is executable. Returns the canonical
/// path to exec, or an error that's surfaced to the caller so a typo
/// never leaves the daemon trying to exec() a missing binary.
fn validate_restart_exe(path: &std::path::Path) -> Result<PathBuf> {
    let abs = std::fs::canonicalize(path)
        .with_context(|| format!("restart binary not found: {}", path.display()))?;
    let meta = std::fs::metadata(&abs)
        .with_context(|| format!("cannot stat restart binary: {}", abs.display()))?;
    if !meta.is_file() {
        anyhow::bail!("restart binary is not a file: {}", abs.display());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("restart binary is not executable: {}", abs.display());
        }
    }
    Ok(abs)
}

/// Best exe path for re-`exec()` on restart: the startup-captured
/// path if available, else `current_exe()` with any trailing
/// `" (deleted)"` marker stripped (defensive — the startup capture
/// should always win in practice).
fn restart_exe_path() -> Result<PathBuf> {
    if let Some(p) = STARTUP_EXE.get() {
        return Ok(p.clone());
    }
    let p = std::env::current_exe()?;
    if let Some(s) = p.to_str() {
        if let Some(stripped) = s.strip_suffix(" (deleted)") {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(p)
}

/// Daemon-local sidecar for an active remote-WS deployment. Holds
/// the immutable `RemoteState` plus the listener-port we picked
/// (so we can construct the localhost URL without re-querying the
/// socket). Lives inside `SessionManager::remote` once the
/// listener is spawned. Visible to `remote_supervisor` because
/// that module's `handle_one` is the only place that ever
/// installs one.
pub(crate) struct RemoteHandle {
    pub(crate) state: crate::remote::RemoteState,
    pub(crate) port: u16,
}

impl SessionManager {
    /// Construct the manager along with the receiver side of the
    /// remote-start channel. The caller (`main.rs`) spawns the
    /// supervisor task with that receiver so on-demand
    /// `/remote-control` calls work without static recursion
    /// between `dispatch` and `serve_ws_on`.
    ///
    /// `runtime_dir` is where per-adapter Unix sockets land for
    /// the reconnectable-adapter path (PR #69); we layer the
    /// `adapters/` subdir under it.
    pub async fn new(
        storage: Arc<Storage>,
        config: Arc<Config>,
        runtime_dir: PathBuf,
    ) -> Result<(
        Self,
        tokio::sync::mpsc::UnboundedReceiver<crate::remote_supervisor::SupervisorMsg>,
        tokio::sync::mpsc::UnboundedReceiver<RestartCommand>,
    )> {
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
            // Scrollback survives daemon restarts because `pty_replay`
            // serves it from the on-disk `pty.log` directly; no in-memory
            // rehydration needed.
            let pty_state = PtyState::default();
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
                pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
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
        let (remote_tx, remote_rx) = tokio::sync::mpsc::unbounded_channel();
        let (restart_tx, restart_rx) = tokio::sync::mpsc::unbounded_channel();
        let remote_snapshot_path = runtime_dir.join("remote.json");
        // Honor CONSTRUCT_ASSETS_DIR at boot in debug builds only — release
        // always serves the embedded, tamper-proof assets.
        let dev_assets = if cfg!(debug_assertions) {
            std::env::var_os("CONSTRUCT_ASSETS_DIR").map(PathBuf::from)
        } else {
            None
        };
        let widget_snapshots = session_ids
            .iter()
            .map(|id| (id.clone(), WidgetSnapshot::read(&storage, id)))
            .collect();
        Ok((
            Self {
                storage,
                config,
                adapter_runtime_dir,
                sessions: RwLock::new(sessions),
                groups: RwLock::new(groups),
                broadcast,
                loops,
                is_shutting_down: AtomicBool::new(false),
                remote: std::sync::Mutex::new(None),
                remote_starter: remote_tx,
                remote_snapshot_path,
                restart_tx,
                dev_assets: std::sync::Mutex::new(dev_assets),
                widget_snapshots: tokio::sync::Mutex::new(widget_snapshots),
                next_conn_id: AtomicU64::new(1),
                conn_views: std::sync::Mutex::new(HashMap::new()),
            },
            remote_rx,
            restart_rx,
        ))
    }

    /// Allocate a monotonic id for a new client connection. The connection
    /// uses it for `set_conn_view` / `clear_conn`.
    pub fn alloc_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Record which session + surface a connection is currently viewing.
    /// A connection views one session at a time, so this overwrites any prior
    /// entry for `conn_id`.
    pub fn set_conn_view(&self, conn_id: u64, session_id: String, view: ClientView) {
        if let Ok(mut m) = self.conn_views.lock() {
            m.insert(conn_id, (session_id, view));
        }
    }

    /// Drop a connection's view registration when it disconnects.
    pub fn clear_conn(&self, conn_id: u64) {
        if let Ok(mut m) = self.conn_views.lock() {
            m.remove(&conn_id);
        }
    }

    /// Whether any live connection is currently watching `session_id` in the
    /// chat view. The `AskUserQuestion` chat-gate degrades the picker to text
    /// when this is true.
    pub fn chat_viewer_active(&self, session_id: &str) -> bool {
        self.conn_views
            .lock()
            .map(|m| {
                m.values()
                    .any(|(s, v)| s == session_id && *v == ClientView::Chat)
            })
            .unwrap_or(false)
    }

    fn install_memory_env(&self, env: &mut HashMap<String, String>, project_id: Option<&str>) {
        env.remove(ENV_GLOBAL_MEMORY_FILE);
        env.remove(ENV_PROJECT_MEMORY_FILE);
        env.remove(ENV_PROJECT_ID);

        match self.storage.ensure_global_memory() {
            Ok(path) => {
                env.insert(
                    ENV_GLOBAL_MEMORY_FILE.to_string(),
                    path.to_string_lossy().to_string(),
                );
            }
            Err(e) => tracing::warn!(error = ?e, "global memory file setup failed"),
        }

        let Some(project_id) = project_id else {
            return;
        };
        match self.storage.ensure_project_memory(project_id) {
            Ok(path) => {
                env.insert(ENV_PROJECT_ID.to_string(), project_id.to_string());
                env.insert(
                    ENV_PROJECT_MEMORY_FILE.to_string(),
                    path.to_string_lossy().to_string(),
                );
            }
            Err(e) => {
                tracing::warn!(project_id, error = ?e, "project memory file setup failed");
            }
        }
    }

    /// The dev-mode web-UI asset directory, if one is active. `None`
    /// means serve the embedded assets.
    pub fn dev_assets(&self) -> Option<PathBuf> {
        self.dev_assets.lock().unwrap().clone()
    }

    /// Point the remote web server at `dir` (or revert to embedded with
    /// `None`). No-op in release builds — the override is dev-only.
    pub fn set_dev_assets(&self, dir: Option<PathBuf>) {
        if cfg!(debug_assertions) {
            *self.dev_assets.lock().unwrap() = dir;
        }
    }

    /// Path where the supervisor reads / writes the remote
    /// `RemoteSnapshot`. Exposed so the supervisor can hand it to
    /// `RemoteState::with_snapshot_path`.
    pub(crate) fn remote_snapshot_path(&self) -> PathBuf {
        self.remote_snapshot_path.clone()
    }

    /// Request a daemon restart. Resolves the exe path + args,
    /// sends a `RestartCommand` to main's restart channel, and
    /// returns the command so the IPC handler can echo it back to
    /// the caller before the runtime tears down. Returns `Err` if
    /// the exe path can't be resolved or the receiver was dropped
    /// (which shouldn't happen — main holds it for the daemon's
    /// lifetime).
    pub fn request_daemon_restart(&self, exe_override: Option<PathBuf>) -> Result<RestartCommand> {
        let exe = match exe_override {
            // Validate a caller-supplied binary BEFORE tearing the
            // daemon down — exec()ing a bad path would never come back.
            Some(p) => validate_restart_exe(&p)?,
            None => restart_exe_path().context("resolve restart exe")?,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let cmd = RestartCommand { exe, args };
        self.restart_tx
            .send(cmd.clone())
            .map_err(|_| anyhow::anyhow!("restart channel closed"))?;
        Ok(cmd)
    }

    /// Access to the remote-handle slot. Used by the supervisor
    /// task to install the handle after a successful bind, and by
    /// `start_remote`'s fast path to snapshot the existing state.
    pub(crate) fn remote_slot(
        &self,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'_, Option<RemoteHandle>>> {
        self.remote.lock()
    }

    fn adapter_socket_path(&self, id: &str) -> PathBuf {
        self.adapter_runtime_dir.join(format!("{id}.sock"))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<BroadcastMsg> {
        self.broadcast.subscribe()
    }

    pub fn spawn_widget_watcher(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(WIDGET_WATCH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                manager.poll_widget_files().await;
            }
        });
    }

    async fn poll_widget_files(&self) {
        let session_ids: Vec<String> = self.sessions.read().await.keys().cloned().collect();
        let mut snapshots = self.widget_snapshots.lock().await;
        for session_id in &session_ids {
            let next = WidgetSnapshot::read(&self.storage, session_id);
            let previous = snapshots
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| WidgetSnapshot {
                    files: HashMap::new(),
                });
            for (id, panel) in &next.files {
                if !ui_panel_changed(previous.files.get(id), panel) {
                    continue;
                }
                self.broadcast_widget_event(&session_id, SessionEvent::UiPanel(panel.clone()));
            }
            for id in previous.files.keys() {
                if !next.files.contains_key(id) {
                    self.broadcast_widget_event(
                        &session_id,
                        SessionEvent::UiDelete { id: id.clone() },
                    );
                }
            }
            snapshots.insert(session_id.clone(), next);
        }
        snapshots.retain(|id, _| session_ids.contains(id));
    }

    fn broadcast_widget_event(&self, session_id: &str, event: SessionEvent) {
        let _ = self
            .broadcast
            .send(BroadcastMsg::Event(EventNotificationPayload {
                session_id: session_id.to_string(),
                at: Utc::now(),
                event,
                seq: 0,
            }));
    }

    /// Send a `remote/state` broadcast announcing the current remote-
    /// WS client count. Best-effort — silently skipped if no
    /// subscribers (the broadcast channel is the same one all
    /// notifications flow through).
    pub fn broadcast_remote_state(&self, clients: u32) {
        let _ = self.broadcast.send(BroadcastMsg::RemoteState(
            agentd_protocol::RemoteStateNotificationPayload { clients },
        ));
    }

    /// Start (or look up) the remote WS listener + cloudflared
    /// tunnel and return a URL + QR ready for the user. Idempotent
    /// — calling more than once returns the existing token+URL so
    /// the QR code stays stable for the daemon's lifetime.
    ///
    /// `port_hint` is honored when set (env-var-at-boot path);
    /// otherwise an ephemeral localhost port is bound. The cloudflared
    /// supervisor is also launched on first call (skipped when
    /// `CONSTRUCT_REMOTE_NO_TUNNEL` is set, same as the boot path).
    ///
    /// `wait_for_tunnel` caps how long we wait for cloudflared to
    /// publish its `*.trycloudflare.com` URL before returning the
    /// localhost URL with a "still warming up" hint. ~3s is enough
    /// for a typical fresh cloudflared start; the user can refresh
    /// to grab the public URL once it lands.
    pub async fn start_remote(
        self: Arc<Self>,
        port_hint: Option<u16>,
        params: agentd_protocol::RemoteStartParams,
    ) -> anyhow::Result<agentd_protocol::RemoteStartResult> {
        use anyhow::Context as _;

        // Always-on bind path: ask the supervisor to ensure the
        // listener is up (and, if requested, to start cloudflared
        // too). Static call edge from here goes through an mpsc
        // channel, NOT a direct call to `serve_ws_on`, which
        // keeps the dispatch-loop Send inference from going into
        // a cycle. Idempotent — repeat requests are no-ops on the
        // bind side; the tunnel is spawned at most once per
        // daemon lifetime.
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_starter
            .send(crate::remote_supervisor::SupervisorMsg::Start(
                crate::remote_supervisor::StartRequest {
                    port_hint,
                    spawn_tunnel: !params.local_only,
                    password: params.password.clone(),
                    respond: tx,
                },
            ))
            .map_err(|_| anyhow::anyhow!("remote supervisor task is not running"))?;
        let outcome = rx
            .await
            .context("remote supervisor dropped reply channel")??;

        Ok(self
            .build_remote_result(
                outcome.state,
                outcome.port,
                params.local_only,
                params.wait_for_tunnel,
            )
            .await?)
    }

    /// Tear down the remote WS listener + cloudflared tunnel via
    /// the supervisor. Idempotent — calling when nothing is running
    /// is not an error; the result's `was_running` field tells the
    /// caller whether anything was actually torn down. Token
    /// rotates on the next `start_remote` so the old QR is dead.
    pub async fn stop_remote(self: Arc<Self>) -> anyhow::Result<agentd_protocol::RemoteStopResult> {
        use anyhow::Context as _;
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.remote_starter
            .send(crate::remote_supervisor::SupervisorMsg::Stop(
                crate::remote_supervisor::StopRequest { respond: tx },
            ))
            .map_err(|_| anyhow::anyhow!("remote supervisor task is not running"))?;
        let outcome = rx
            .await
            .context("remote supervisor dropped reply channel")??;
        Ok(agentd_protocol::RemoteStopResult {
            was_running: outcome.was_running,
        })
    }

    /// Render the final `RemoteStartResult` for either mode.
    ///
    /// Local-only mode: return the `http://127.0.0.1:<port>` URL
    /// immediately, no waiting. Tunnel mode: poll for the
    /// `*.trycloudflare.com` URL up to ~15s and either return it
    /// (`tunnel_ready = true`) or fail with a JSON-RPC error that
    /// tells the user exactly what's wrong — never silently fall
    /// back to the local URL the way the old single-mode shape did
    /// (that's `/remote-control-debug`'s job).
    async fn build_remote_result(
        &self,
        state: crate::remote::RemoteState,
        port: u16,
        local_only: bool,
        wait_for_tunnel: bool,
    ) -> anyhow::Result<agentd_protocol::RemoteStartResult> {
        use std::time::Duration;

        if local_only {
            // Same reasoning as the tunnel-mode URL: this is the
            // URL the user opens in a browser. The HTML's JS does
            // the `http` → `ws` swap for its WebSocket back to
            // this same daemon. Showing `ws://` here would mean
            // the URL can't be pasted into a browser at all,
            // which defeats `/remote-control-debug`'s whole
            // point.
            let url = format!("http://127.0.0.1:{port}/");
            let qr = crate::remote::render_qr_dense1x2(&url).unwrap_or_default();
            return Ok(agentd_protocol::RemoteStartResult {
                url,
                qr,
                tunnel_ready: false,
                password: state.password().to_string(),
                hint: None,
            });
        }

        if !wait_for_tunnel {
            let url = format!("http://127.0.0.1:{port}/");
            let qr = crate::remote::render_qr_dense1x2(&url).unwrap_or_default();
            return Ok(agentd_protocol::RemoteStartResult {
                url,
                qr,
                tunnel_ready: false,
                password: state.password().to_string(),
                hint: Some(
                    "Starting public tunnel… QR will update when cloudflared publishes a URL."
                        .to_string(),
                ),
            });
        }

        // Tunnel mode: poll the shared tunnel-url slot. 15s
        // covers a typical cloudflared cold start (1–3s) plus
        // slack for slow networks. We poll rather than wire a
        // notifier because the call shape is request/reply over
        // IPC — the caller already blocks on this future anyway.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(u) = state.tunnel_url().await {
                let qr = crate::remote::render_qr_dense1x2(&u).unwrap_or_default();
                return Ok(agentd_protocol::RemoteStartResult {
                    url: u,
                    qr,
                    tunnel_ready: true,
                    password: state.password().to_string(),
                    hint: None,
                });
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Timeout: emit an error with the most useful diagnostic
        // we can muster. The CLI surfaces this verbatim in the
        // popup so the user knows why the tunnel didn't come up.
        let cloudflared_available = which::which("cloudflared").is_ok();
        let no_tunnel_env = std::env::var("CONSTRUCT_REMOTE_NO_TUNNEL").is_ok();
        let msg = if no_tunnel_env {
            "CONSTRUCT_REMOTE_NO_TUNNEL is set; unset it and rerun \
             `/remote-control`. Use `/remote-control debug` for the \
             local-only URL."
        } else if !cloudflared_available {
            "cloudflared not on PATH. Install with `brew install \
             cloudflared` (or from \
             github.com/cloudflare/cloudflared/releases) and rerun \
             `/remote-control`. Use `/remote-control debug` for the \
             local-only URL."
        } else {
            "cloudflared is running but hasn't published a \
             *.trycloudflare.com URL within 15s. Check the daemon \
             log (RUST_LOG=info,agentd=debug) for cloudflared's \
             stderr, or use `/remote-control debug` for the local \
             URL."
        };
        anyhow::bail!("{msg}")
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
                    capabilities: builtin_harness_capabilities(name),
                }
            })
            .collect()
    }

    pub async fn list(&self) -> Vec<SessionSummary> {
        let guard = self.sessions.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for entry in guard.values() {
            let summary = entry.summary().await;
            out.push(summary);
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
        let ui_panels = self.storage.read_widgets(id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "read widgets failed");
            Vec::new()
        });
        Ok(SessionDetail {
            summary,
            events: transcript.events,
            ui_panels,
        })
    }

    pub async fn transcript(
        &self,
        id: &str,
        from: u64,
        limit: Option<usize>,
        tail: Option<usize>,
    ) -> Result<TranscriptResult> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        if let Some(n) = tail {
            // Tail mode: take `total` from the live counter (cheap, no file
            // scan) and read only the last `n` events from disk.
            let total = entry.transcript_count.load(Ordering::Relaxed);
            let events = self.storage.read_transcript_tail(id, n)?;
            return Ok(TranscriptResult { events, total });
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
        let harness = params.harness.as_str();
        let adapter_cfg = self
            .config
            .adapters
            .get(harness)
            .ok_or_else(|| anyhow!("unknown harness: {}", params.harness))?
            .clone();
        let binary_spec = adapter_cfg
            .binary
            .clone()
            .unwrap_or_else(|| harness.to_string());
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
            harness: harness.to_string(),
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
            mode: Some(effective_mode(&params)),
            pinned: false,
            // Negative timestamp so newer sessions sort to the top by default.
            position: -now.timestamp_millis(),
            group_id: params.group_id.clone(),
            parent_session_id: params
                .parent_session_id
                .clone()
                .or_else(|| params.env.get("CONSTRUCT_PARENT_SESSION_ID").cloned()),
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: params.kind,
            archived: false,
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
        // CONSTRUCT_SESSION_DATA_DIR / CONSTRUCT_SESSION_KIND — not just
        // the session.start params.env. The codex adapter (and
        // claude) reads these via std::env::var, so leaving them
        // only in session.start meant their first-spawn bookkeeping
        // (originator-tagged rollout capture, session-id minting)
        // silently no-op'd; respawn already merged them in time, so
        // the bug only surfaced on initial create.
        //
        // Precedence: `[adapters.<name>].env` is the per-harness
        // baseline (operator-set default model, etc.), overridden
        // by the per-session `params.env` (explicit `construct new
        // --env KEY=VAL`), overridden in turn by daemon-meta. So a
        // CLI flag always wins over config.toml, and daemon meta
        // always wins over both.
        let mut env_with_meta = adapter_cfg.env.clone();
        for (k, v) in &params.env {
            env_with_meta.insert(k.clone(), v.clone());
        }
        let session_dir = self.storage.session_dir(&id);
        let widgets_dir = self.storage.ensure_widgets_dir(&id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "ensure widgets dir failed");
            self.storage.widgets_dir(&id)
        });
        env_with_meta.insert(
            "CONSTRUCT_SESSION_DATA_DIR".to_string(),
            session_dir.to_string_lossy().to_string(),
        );
        env_with_meta.insert(
            "CONSTRUCT_SESSION_WIDGETS_DIR".to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        // Single auto-approval policy the daemon defines once; each adapter
        // translates it into its harness's native permission mechanism. See
        // `agentd_protocol::adapter::policy`.
        env_with_meta.insert(
            agentd_protocol::adapter::policy::ENV_AUTO_APPROVE_PATHS.to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        env_with_meta.insert(
            "CONSTRUCT_SESSION_KIND".to_string(),
            match params.kind {
                agentd_protocol::SessionKind::User => "user",
                agentd_protocol::SessionKind::Orchestrator => "orchestrator",
                agentd_protocol::SessionKind::Subagent => "subagent",
            }
            .to_string(),
        );
        self.install_memory_env(&mut env_with_meta, params.group_id.as_deref());

        let (adapter, info) = Adapter::spawn_reconnectable(
            harness.to_string(),
            binary,
            combined_args,
            env_with_meta.clone(),
            self.adapter_socket_path(&id),
            msg_tx.clone(),
        )
        .await
        .with_context(|| format!("spawn adapter for {}", harness))?;

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
                size: params.pty_size,
            }),
            deleted: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(summary.title.is_some()),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
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
        // smith interactive for free. The initial 80×10 pty_size is
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
            parent_session_id: None,
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
    /// receives `CONSTRUCT_RESUME=1` in its env plus the same start params it
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
                // Archived sessions stay down across daemon restarts — the
                // user terminated them on purpose and brings them back with an
                // explicit restart, not auto-resume.
                if should_resume_on_startup(s.state) && !s.archived {
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
    /// and signals `CONSTRUCT_RESUME=1` so the adapter can pull its own
    /// prior state from `CONSTRUCT_SESSION_DATA_DIR`.
    async fn respawn(self: Arc<Self>, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {id}"))?;
        let mut start_params = self.storage.load_start_params(id)?;
        start_params
            .env
            .insert("CONSTRUCT_RESUME".to_string(), "1".to_string());
        // Make sure the data-dir env is present even if start.json predates
        // the meta env injection.
        start_params.env.insert(
            "CONSTRUCT_SESSION_DATA_DIR".to_string(),
            self.storage.session_dir(id).to_string_lossy().to_string(),
        );
        let project_id = {
            let s = entry.summary.read().await;
            s.group_id.clone()
        };
        self.install_memory_env(&mut start_params.env, project_id.as_deref());
        let widgets_dir = self.storage.ensure_widgets_dir(id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "ensure widgets dir failed");
            self.storage.widgets_dir(id)
        });
        start_params.env.insert(
            "CONSTRUCT_SESSION_WIDGETS_DIR".to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        start_params.env.insert(
            agentd_protocol::adapter::policy::ENV_AUTO_APPROVE_PATHS.to_string(),
            widgets_dir.to_string_lossy().to_string(),
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
                    // Restarting an archived session brings it back to life:
                    // it returns to the active list.
                    s.archived = false;
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

        // Attach failed. The probe's reader task in `Adapter::attach`
        // still holds a clone of `msg_tx` and, when its hung-connection
        // read finally errors out, will push a spurious
        // `AdapterMessage::Closed` into the channel. If we kept the
        // same `msg_rx` for the post-spawn `drain_adapter`, that
        // `Closed` would arrive seconds after the freshly-spawned
        // adapter is up and immediately mark the resumed session
        // `Done` — defeating the whole resume. Replace the channel
        // here so the leaked sender's `send` lands in a dropped
        // receiver (and silently fails) instead.
        drop(msg_tx);
        drop(msg_rx);
        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);

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
        // respawn too. Per-session env (from `construct new --env`)
        // still wins because start_params.env was constructed with
        // it on top of adapter_cfg.env at create time and gets the
        // same treatment again here.
        let respawn_env = {
            let mut e = adapter_cfg.env.clone();
            for (k, v) in &start_params.env {
                e.insert(k.clone(), v.clone());
            }
            self.install_memory_env(&mut e, project_id.as_deref());
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
        // emit nothing on resume (smith does this), so we keep the
        // prior PTY history visible after a daemon restart instead of
        // wiping it.
        if !info.capabilities.supports_silent_resume {
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
            // Restarting an archived session brings it back to life: it
            // returns to the active list.
            s.archived = false;
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
        // correct cached size. smith (silent_resume) is skipped —
        // it explicitly emits nothing on resume.
        if let Some(size) = force_redraw_size_on_resume(&info.capabilities, start_params.pty_size) {
            let manager_for_redraw = self.clone();
            let id_owned = id.to_string();
            let entry_for_redraw = entry.clone();
            tokio::spawn(async move {
                // Wait for the resumed child's PTY output to settle (it
                // produced its resume draw and went quiet) before forcing
                // the redraw, so the SIGWINCH lands after the child has
                // loaded its conversation rather than on a half-drawn
                // banner. Falls back to a hard cap if it never settles.
                let started = tokio::time::Instant::now();
                loop {
                    tokio::time::sleep(RESPAWN_REDRAW_POLL).await;
                    let last = entry_for_redraw.summary.read().await.last_pty_at_ms;
                    if resume_redraw_ready(last, Utc::now().timestamp_millis(), started.elapsed()) {
                        break;
                    }
                }
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
    /// `CONSTRUCT_RESUME=1` in the adapter env so harnesses that
    /// persist conversation state (smith) reload it on the new
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
    ///
    /// Sets [`Self::is_shutting_down`] before sending the SHUTDOWN
    /// RPCs so the `drain_adapter` task knows to keep the session's
    /// pre-shutdown state on disk (instead of marking it `Done` from
    /// the resulting `AdapterMessage::Closed`). Without that,
    /// `resume_running_sessions` would skip every session on the
    /// next boot because they'd all be terminal.
    pub async fn shutdown_adapters(&self) {
        self.is_shutting_down.store(true, Ordering::Release);
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
                    // Operator-initiated shutdown (SIGINT/SIGTERM →
                    // `shutdown_adapters`): the adapter exiting is
                    // *expected*, not a session ending. Leave the
                    // session's persisted state untouched so it's
                    // resumable on the next daemon boot. Without
                    // this guard a graceful daemon restart marks
                    // every live session `Done` and the next start's
                    // `resume_running_sessions` skips them all.
                    if self.is_shutting_down.load(Ordering::Acquire) {
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
        // Operator-initiated shutdown: the adapter exiting may still
        // flush a `Done` / `Error` event (e.g. the shell adapter's
        // PTY emits `Done` when the wrapped process dies). Letting
        // those land would transition the session to terminal and
        // make `resume_running_sessions` skip it on the next boot,
        // defeating the whole point of the reconnectable-adapters
        // shutdown path. Drop all events during shutdown.
        if self.is_shutting_down.load(Ordering::Acquire) {
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
        // Persist smith/chat PTY bytes in the transcript as lightweight
        // ordering markers. PTY replay still comes from pty.log, but these
        // markers let a fresh TUI interleave transcript-only items (tool
        // blocks) with the raw byte stream at the right point after restart.
        if let SessionEvent::Pty { .. } = &event {
            let seq = entry.transcript_count.fetch_add(1, Ordering::Relaxed) + 1;
            let now = Utc::now();
            let ts = TimestampedEvent {
                seq,
                at: now,
                event: event.clone(),
            };
            if let Err(e) = self.storage.append_event(&entry.id, &ts) {
                tracing::warn!(session = %entry.id, error = ?e, "append PTY marker failed");
            }
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
        // BrowserPreview is ephemeral, live-only UI: a base64 PNG that
        // clients render as an overlay/wallpaper but never replay from the
        // transcript. Persisting it would bloat transcript.jsonl with
        // full-size screenshots (slowing every load, since `read_transcript`
        // parses every line) for no consumer, and leak the image into the
        // model via `agentd_get_transcript`. So broadcast to live clients
        // and return before `append_event` — same treatment as AgentStatus.
        if let SessionEvent::BrowserPreview(_) = &event {
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
        // ToolApprovalResolved is a transient UI dismissal signal: it tells
        // passive viewers (web approval dialog, TUI minibuffer) that a
        // pending approval was answered — by any client — so they can close
        // their prompt. Like AgentStatus/BrowserPreview, broadcast it live
        // but never persist it to the transcript.
        if let SessionEvent::ToolApprovalResolved { .. } = &event {
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
        // ApprovalModeChanged updates durable per-session state. The state
        // notification is enough for clients; do not record a transcript row.
        if let SessionEvent::ApprovalModeChanged { mode } = &event {
            if let Err(e) = self.persist_approval_mode(entry, *mode).await {
                tracing::warn!(
                    session = %entry.id,
                    error = ?e,
                    "persist approval mode from adapter event failed"
                );
            }
            return;
        }
        // PTY events take a fast path: append to the on-disk pty.log + a
        // live broadcast. A copy was also appended to the transcript above
        // as an ordering marker. Replay reads back from `pty.log` directly
        // when a TUI attaches, so we no longer keep a parallel in-memory
        // ring of bytes.
        if let SessionEvent::Pty { .. } = &event {
            if let Some(bytes) = event.pty_bytes() {
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
                | SessionEvent::PtyResize { .. }
                | SessionEvent::ToolApprovalRequest { .. }
                // Transient; handled by the broadcast-only fast path above.
                | SessionEvent::ToolApprovalResolved { .. }
                | SessionEvent::ApprovalModeChanged { .. }
                | SessionEvent::TaskStart { .. }
                | SessionEvent::TaskBackgrounded { .. }
                | SessionEvent::TaskEnd { .. }
                | SessionEvent::ContextCompacted { .. }
                | SessionEvent::BrowserPreview(_)
                | SessionEvent::UiPanel(_)
                | SessionEvent::UiDelete { .. }
                | SessionEvent::EditorState { .. }
                // ClientCommand is a UI-control action; it never moves the
                // session's top-level state. (Prototype: persistence still
                // goes through the default append above — the policy-driven
                // gate on `slash::TranscriptPolicy` is the follow-up wiring.)
                | SessionEvent::ClientCommand { .. } => {
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
        // user's typed prompt — smith interactive does this). The
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

    pub async fn emit_session_event(&self, p: SessionEmitEventParams) -> Result<()> {
        let entry = self
            .get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        self.handle_event(&entry, p.event).await;
        Ok(())
    }

    pub async fn delete_widget(&self, p: SessionWidgetDeleteParams) -> Result<()> {
        self.get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        self.storage.delete_widget(&p.session_id, &p.panel_id)?;
        self.broadcast_widget_event(&p.session_id, SessionEvent::UiDelete { id: p.panel_id });
        Ok(())
    }

    pub async fn attach_clipboard(
        &self,
        p: SessionAttachClipboardParams,
    ) -> Result<SessionAttachClipboardResult> {
        self.get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", p.session_id))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&p.data)
            .context("decode clipboard attachment")?;
        if bytes.is_empty() {
            anyhow::bail!("clipboard attachment is empty");
        }
        if bytes.len() > MAX_CLIPBOARD_ATTACHMENT_BYTES {
            anyhow::bail!(
                "clipboard attachment is too large: {} bytes (max {})",
                bytes.len(),
                MAX_CLIPBOARD_ATTACHMENT_BYTES
            );
        }

        let dir = self
            .storage
            .data_dir()
            .join("sessions")
            .join(&p.session_id)
            .join("attachments");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("create {}", dir.display()))?;

        let ext = extension_for_attachment(p.filename.as_deref(), p.mime.as_deref(), &bytes);
        let stem = p
            .filename
            .as_deref()
            .and_then(sanitized_file_stem)
            .unwrap_or_else(|| "clipboard".to_string());
        let ts = Utc::now().format("%Y%m%d-%H%M%S%.3f");
        let mut path = dir.join(format!("{stem}-{ts}.{ext}"));
        let mut suffix = 1usize;
        while tokio::fs::try_exists(&path).await.unwrap_or(false) {
            path = dir.join(format!("{stem}-{ts}-{suffix}.{ext}"));
            suffix += 1;
        }
        tokio::fs::write(&path, &bytes)
            .await
            .with_context(|| format!("write {}", path.display()))?;

        let path_str = path.display().to_string();
        let reference = format!("[#file:{}]", path_str);
        Ok(SessionAttachClipboardResult {
            path: path_str,
            reference,
        })
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
    /// prompt is non-empty, and (d) the smith adapter binary is
    /// configured + locatable. Silently no-ops on any miss.
    fn maybe_spawn_auto_title(&self, entry: Arc<SessionEntry>, prompt: String) {
        // Cheap checks first so we don't burn the per-session attempt
        // budget (the AtomicBool flip is one-way until a daemon
        // restart) on inputs that wouldn't have produced a title
        // anyway.
        if prompt.trim().is_empty() {
            return;
        }
        let Some(smith_adapter) = self.config.adapters.get("smith").cloned() else {
            return;
        };
        let binary_spec = smith_adapter
            .binary
            .clone()
            .unwrap_or_else(|| "construct-adapter-smith".to_string());
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
        // Capture submitted PTY lines before forwarding them. Some interactive
        // harnesses do not echo user text as structured `Message` events, so
        // chat-mode transcript history otherwise loses those turns.
        let input_lines = self.capture_pty_input_lines(&entry, &bytes).await;
        let harness = entry.summary.read().await.harness.clone();
        for line in input_lines {
            if should_record_pty_user_message(&harness) {
                self.handle_event(
                    &entry,
                    SessionEvent::Message {
                        role: MessageRole::User,
                        text: line,
                    },
                )
                .await;
            } else if !entry.title_gen_attempted.load(Ordering::SeqCst) && line.chars().count() >= 2
            {
                self.maybe_spawn_auto_title(entry.clone(), line);
            }
        }
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

    /// Feed PTY-input bytes through a minimal terminal-input parser (printable
    /// ASCII + backspace + CR/LF; CSI/SS3 sequences skipped) and return every
    /// submitted non-empty line. The parser is intentionally small: it is for
    /// transcript/user-title capture, not full terminal editing semantics.
    async fn capture_pty_input_lines(
        &self,
        entry: &Arc<SessionEntry>,
        bytes: &[u8],
    ) -> Vec<String> {
        let mut cap = entry.pty_input_capture.lock().await;
        let mut lines = Vec::new();
        for &b in bytes {
            match cap.esc {
                0 => match b {
                    b'\n' if cap.last_was_cr => {
                        cap.last_was_cr = false;
                    }
                    b'\r' | b'\n' => {
                        let s = cap.buf.trim().to_string();
                        cap.last_was_cr = b == b'\r';
                        cap.buf.clear();
                        if s.chars().count() >= 2 {
                            lines.push(s);
                        }
                    }
                    0x1b => cap.esc = 1,
                    0x08 | 0x7f => {
                        cap.last_was_cr = false;
                        cap.buf.pop();
                    }
                    _ if (0x20..0x7f).contains(&b) => {
                        cap.last_was_cr = false;
                        cap.buf.push(b as char);
                    }
                    _ => {
                        cap.last_was_cr = false;
                    }
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
        lines
    }

    /// Record that a given client kind just acted on a session's
    /// PTY (typed input or sent a resize). Updates the kind's
    /// last-known viewport (if `resize_to` was supplied), flips
    /// `last_active` to that kind, and — if the kind switched
    /// since last time — issues a `pty_resize` to match the kind's
    /// stored viewport. No-op when only one kind is attached.
    ///
    /// This is the daemon-side half of the "active client wins"
    /// PTY-size policy. The complementary half lives in
    /// `server::dispatch`'s `SESSION_PTY_INPUT` and
    /// `SESSION_PTY_RESIZE` arms, which call this method before
    /// forwarding the actual request to the PTY.
    pub async fn note_pty_activity(
        self: &Arc<Self>,
        id: &str,
        kind: crate::server::ClientKind,
        resize_to: Option<(u16, u16)>,
    ) {
        let Some(entry) = self.get_entry(id).await else {
            return;
        };
        let to_apply = {
            let mut policy = entry
                .pty_client_policy
                .lock()
                .expect("pty_client_policy mutex poisoned");
            if let Some(sz) = resize_to {
                match kind {
                    crate::server::ClientKind::Tui => policy.tui_size = Some(sz),
                    crate::server::ClientKind::Remote => policy.remote_size = Some(sz),
                }
            }
            let switched = policy.last_active != Some(kind);
            policy.last_active = Some(kind);
            // Only re-resize on a *switch*, or when this call was
            // itself a pty_resize. Plain pty_input from the same
            // kind that's already active is a no-op for the size
            // policy (the per-call pty_resize handler still runs
            // separately).
            if switched || resize_to.is_some() {
                match kind {
                    crate::server::ClientKind::Tui => policy.tui_size,
                    crate::server::ClientKind::Remote => policy.remote_size,
                }
            } else {
                None
            }
        };
        if let Some((cols, rows)) = to_apply {
            // Best-effort. The pty_resize dedup inside
            // `SessionManager::pty_resize` handles the case where
            // the OS PTY is already at this size.
            if let Err(e) = self.pty_resize(id, cols, rows).await {
                tracing::debug!(session = %id, error = %e, "policy-driven pty_resize failed");
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
        // Tell other attached clients the new geometry (transient, not
        // persisted) so a passive viewer (e.g. a narrower web terminal) can
        // render at the real width instead of wrapping. Only fires on an
        // actual change — the dedup above already returned for a no-op.
        self.broadcast_widget_event(id, SessionEvent::PtyResize { cols, rows });
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
        self.pty_replay_range(id, None, None).await
    }

    pub async fn pty_replay_range(
        &self,
        id: &str,
        max_bytes: Option<usize>,
        before_offset: Option<u64>,
    ) -> Result<PtyReplayResult> {
        use base64::Engine;
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = entry.pty.lock().await.size;
        // Pull scrollback from the on-disk `pty.log`, not the (now-removed)
        // in-memory ring. Requests are capped by `PTY_REPLAY_CAP`; clients can
        // ask for older adjacent ranges and replay their local chunks in order.
        let requested = max_bytes.unwrap_or(PTY_REPLAY_CAP).min(PTY_REPLAY_CAP);
        let (bytes, start_offset, end_offset, total_bytes) = self
            .storage
            .read_pty_range_before(id, requested, before_offset)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %id, error = ?e, "pty_log range read failed");
                (Vec::new(), 0, 0, 0)
            });
        Ok(PtyReplayResult {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            start_offset,
            end_offset,
            total_bytes,
            size,
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

    /// Archive a session: terminate its adapter (if any) but keep the
    /// transcript, worktree, and start params on disk so it can be restarted
    /// later. The session is marked `archived` (hidden from the list by
    /// default and skipped by startup auto-resume) and persisted. Archiving an
    /// already-terminal session just sets the flag. Reversed by `restart`,
    /// which clears `archived` and brings the session back to the active list.
    pub async fn archive(&self, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Gracefully terminate the live adapter, if there is one. The adapter's
        // Closed event clears `entry.adapter` so a later restart sees no live
        // adapter. Tolerate sessions that are already terminal (no adapter).
        if let Some(adapter) = entry.adapter.lock().await.clone() {
            let params = serde_json::to_value(&agentd_protocol::SessionIdParams {
                session_id: id.to_string(),
            })?;
            let _ = tokio::time::timeout(
                Duration::from_secs(10),
                adapter.request(ahp_method::SESSION_STOP, params),
            )
            .await;
            let _ = tokio::time::timeout(Duration::from_secs(3), adapter.shutdown()).await;
        }
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.archived = true;
            // A live session we just stopped should read as cleanly terminated,
            // not mid-run; leave an already-terminal state (Done/Errored) as-is.
            if !s.state.is_terminal() {
                s.state = SessionState::Done;
            }
            s.pending_input = false;
            s.clone()
        };
        let _ = self.storage.save_summary(&snapshot);
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
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
        // have written for an injected `construct-mcp` server.
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
    /// Collapsed groups are skipped at boundaries: their members are hidden,
    /// so the session jumps the whole project in one step instead of swapping
    /// with each hidden member.
    ///
    /// No-op at the absolute top (ungrouped session #0) or bottom (last
    /// member of last group), or when the only regions in the move direction
    /// are collapsed groups.
    pub async fn move_session(&self, id: &str, dir: MoveDirection) -> Result<()> {
        let all_sessions: Vec<SessionSummary> = self.list().await;
        let all_groups: Vec<GroupSummary> = self.list_groups().await;
        let me = all_sessions
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("session not found: {}", id))?;

        // Find neighbors in `me`'s visible reorder region (same group_id,
        // user sessions only), sorted by position. The daemon list includes
        // hidden orchestrator/subagent records so clients can render them in
        // specialized places, but the TUI's session list filters those out. If
        // reorder considers hidden records, a visible session surrounded by
        // subagents can appear stuck or jump unpredictably because it swaps
        // with rows the user cannot see.
        let region: Vec<&SessionSummary> = all_sessions
            .iter()
            .filter(|s| s.group_id == me.group_id && is_user_session_kind(s))
            .collect();
        let pos_in_region = region.iter().position(|s| s.id == id).unwrap();

        match dir {
            MoveDirection::Up => {
                if pos_in_region > 0 {
                    // Same-region swap.
                    let other = region[pos_in_region - 1];
                    return self.swap_session_positions(&me.id, &other.id).await;
                }
                // At top of region — try to exit into the previous region,
                // skipping collapsed projects.
                let prev = region_above_skipping_collapsed(me.group_id.as_deref(), &all_groups);
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
                // At bottom of region — try to enter the next region,
                // skipping collapsed projects.
                let next = region_below_skipping_collapsed(me.group_id.as_deref(), &all_groups);
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

    async fn persist_approval_mode(
        &self,
        entry: &Arc<SessionEntry>,
        mode: agentd_protocol::ApprovalMode,
    ) -> Result<()> {
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.approval_mode = mode;
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

    pub async fn set_approval_mode(
        &self,
        id: &str,
        mode: agentd_protocol::ApprovalMode,
    ) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        self.persist_approval_mode(&entry, mode).await?;
        // Forward to the adapter so it picks up the change for the next tool
        // classification. If the adapter is gone (session ended), skip.
        if let Some(adapter) = entry.adapter.lock().await.clone() {
            let params = serde_json::to_value(&agentd_protocol::SessionSetApprovalModeParams {
                session_id: id.to_string(),
                mode,
            })?;
            // Best-effort: don't fail the call if the adapter doesn't recognize
            // the method (e.g. claude/codex, which don't gate tools).
            let _ = adapter
                .request(ahp_method::SESSION_SET_APPROVAL_MODE, params)
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
        let mode = match decision.as_str() {
            "auto_review" => Some(agentd_protocol::ApprovalMode::AutoReview),
            "unsafe_auto" => Some(agentd_protocol::ApprovalMode::UnsafeAuto),
            _ => None,
        };
        if let Some(mode) = mode {
            self.persist_approval_mode(&entry, mode).await?;
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

/// Shell out to `construct-adapter-smith --title-mode "<prompt>"`, capture
/// stdout, and apply the title to the session summary. Best-effort:
/// any failure (smith missing keys, network error, non-zero exit,
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

fn effective_mode(params: &CreateSessionParams) -> String {
    match params.mode.as_ref() {
        Some(mode) => mode.clone(),
        None if params.pty_size.is_some() => "interactive".to_string(),
        None => "headless".to_string(),
    }
}

fn builtin_harness_capabilities(name: &str) -> agentd_protocol::Capabilities {
    match name {
        "shell" | "claude" | "codex" | "smith" => agentd_protocol::Capabilities {
            supports_pty: true,
            ..Default::default()
        },
        _ => Default::default(),
    }
}

/// Decide whether to schedule the bump+restore SIGWINCH cycle after a
/// session.start succeeds on respawn. Returns the size to restore to
/// (we always restore to the cached size, then bump by one column for
/// the first leg of the cycle). Returns `None` when no force-redraw
/// is warranted:
///   * the adapter advertises `supports_silent_resume` (smith paints
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
    fn validate_restart_exe_accepts_executable_rejects_bad_paths() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();

        // An executable file → returns the canonical (absolute) path.
        let good = dir.path().join("agentd");
        std::fs::write(&good, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
        let resolved = validate_restart_exe(&good).expect("executable accepted");
        assert!(resolved.is_absolute());
        assert_eq!(resolved, std::fs::canonicalize(&good).unwrap());

        // Missing path → error.
        assert!(validate_restart_exe(&dir.path().join("nope")).is_err());

        // A directory → error (not a regular file).
        assert!(validate_restart_exe(dir.path()).is_err());

        // A non-executable file → error.
        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"data").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_restart_exe(&plain).is_err());
    }

    #[test]
    fn startup_resume_retries_errored_sessions() {
        assert!(should_resume_on_startup(SessionState::Pending));
        assert!(should_resume_on_startup(SessionState::Running));
        assert!(should_resume_on_startup(SessionState::AwaitingInput));
        assert!(should_resume_on_startup(SessionState::Paused));
        assert!(should_resume_on_startup(SessionState::Errored));
        assert!(!should_resume_on_startup(SessionState::Done));
    }

    #[test]
    fn clipboard_attachment_names_are_safe_and_typed() {
        assert_eq!(
            sanitized_file_stem("../../My Screen Shot.png").as_deref(),
            Some("My-Screen-Shot")
        );
        assert_eq!(sanitized_file_stem("😵").as_deref(), None);
        assert_eq!(
            extension_for_attachment(
                Some("photo.jpeg"),
                Some("application/octet-stream"),
                b"plain"
            ),
            "jpeg"
        );
        assert_eq!(
            extension_for_attachment(None, Some("image/png; charset=binary"), b"plain"),
            "png"
        );
        assert_eq!(
            extension_for_attachment(None, None, b"\x89PNG\r\n\x1a\nrest"),
            "png"
        );
        assert_eq!(extension_for_attachment(None, None, b"hello"), "txt");
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

    /// Smith advertises `supports_silent_resume = true` because its
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

    /// The settle gate: don't fire while the child is still drawing
    /// (recent output) or hasn't drawn at all, but do fire once it goes
    /// quiet, and always fire past the hard cap.
    #[test]
    fn resume_redraw_settle_gate() {
        let now = 1_000_000i64;
        let settle = RESPAWN_REDRAW_SETTLE.as_millis() as i64;
        // Nothing drawn yet, well under the cap → wait.
        assert!(!resume_redraw_ready(None, now, Duration::from_millis(0)));
        // Output 50ms ago (< settle) → still drawing, wait.
        assert!(!resume_redraw_ready(
            Some(now - 50),
            now,
            Duration::from_secs(1)
        ));
        // Quiet for exactly the settle window → fire.
        assert!(resume_redraw_ready(
            Some(now - settle),
            now,
            Duration::from_secs(1)
        ));
        // Quiet well past settle → fire.
        assert!(resume_redraw_ready(
            Some(now - 5_000),
            now,
            Duration::from_secs(1)
        ));
        // Never settles (recent output) but hit the hard cap → fire anyway.
        assert!(resume_redraw_ready(Some(now), now, RESPAWN_REDRAW_MAX_WAIT));
        // Never drew anything, but hit the hard cap → fire anyway.
        assert!(resume_redraw_ready(None, now, RESPAWN_REDRAW_MAX_WAIT));
    }

    /// Headless / non-PTY adapters (anything smith-headless-only or
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

    fn synthetic_entry(
        id: &str,
        kind: agentd_protocol::SessionKind,
        position: i64,
    ) -> Arc<SessionEntry> {
        synthetic_entry_with_group(id, kind, position, None)
    }

    fn synthetic_entry_with_group(
        id: &str,
        kind: agentd_protocol::SessionKind,
        position: i64,
        group_id: Option<String>,
    ) -> Arc<SessionEntry> {
        use chrono::Utc;
        use std::sync::atomic::AtomicU64;
        use tokio::sync::RwLock;

        Arc::new(SessionEntry {
            id: id.to_string(),
            summary: RwLock::new(agentd_protocol::SessionSummary {
                id: id.to_string(),
                harness: "shell".into(),
                cwd: "/tmp".into(),
                title: None,
                state: SessionState::Running,
                created_at: Utc::now(),
                last_event_at: None,
                cost_usd: None,
                model: None,
                worktree: None,
                pending_input: false,
                last_prompt: None,
                event_count: 0,
                has_pty: true,
                mode: None,
                pinned: false,
                position,
                group_id,
                parent_session_id: None,
                last_pty_at_ms: None,
                approval_mode: agentd_protocol::ApprovalMode::Manual,
                kind,
                archived: false,
            }),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
        })
    }

    #[tokio::test]
    async fn move_session_ignores_hidden_subagents_in_reorder_region() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Hidden subagents can share the same ungrouped/group region as visible
        // user sessions. Reordering must use visible user-session neighbors,
        // not these hidden records, otherwise a TUI row can appear not to move.
        for (id, kind, position) in [
            ("ssub-before", agentd_protocol::SessionKind::Subagent, 0),
            ("suser-a", agentd_protocol::SessionKind::User, 10),
            ("ssub-between", agentd_protocol::SessionKind::Subagent, 20),
            ("suser-b", agentd_protocol::SessionKind::User, 30),
            ("ssub-after", agentd_protocol::SessionKind::Subagent, 40),
        ] {
            mgr.sessions
                .write()
                .await
                .insert(id.into(), synthetic_entry(id, kind, position));
        }

        mgr.move_session("suser-b", agentd_protocol::MoveDirection::Up)
            .await
            .expect("move up");

        let sessions = mgr.list().await;
        let a = sessions.iter().find(|s| s.id == "suser-a").unwrap();
        let b = sessions.iter().find(|s| s.id == "suser-b").unwrap();
        let hidden = sessions.iter().find(|s| s.id == "ssub-between").unwrap();
        assert_eq!(b.position, 10);
        assert_eq!(a.position, 30);
        assert_eq!(hidden.position, 20);
    }

    #[tokio::test]
    async fn archive_marks_terminal_and_keeps_session() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "s1".into(),
            synthetic_entry("s1", agentd_protocol::SessionKind::User, 0),
        );

        mgr.archive("s1").await.expect("archive");

        let entry = mgr.get_entry("s1").await.expect("entry still present");
        {
            let s = entry.summary.read().await;
            assert!(s.archived, "session should be marked archived");
            assert!(
                s.state.is_terminal(),
                "a running session should read as terminated after archive",
            );
        }
        // Archived sessions stay in the manager (unlike delete) so they can be
        // listed when the toggle is on and later restarted.
        assert!(
            mgr.list().await.iter().any(|s| s.id == "s1"),
            "archived session must remain in the manager",
        );
        // The persisted meta.json carries the archived flag across restarts.
        let persisted = mgr.storage.load_summary("s1").expect("load meta");
        assert!(persisted.archived, "archived flag must be persisted");
    }

    async fn insert_group(mgr: &SessionManager, id: &str, position: i64, collapsed: bool) {
        use chrono::Utc;
        use tokio::sync::RwLock;
        mgr.groups.write().await.insert(
            id.into(),
            Arc::new(GroupEntry {
                summary: RwLock::new(GroupSummary {
                    id: id.into(),
                    name: id.into(),
                    created_at: Utc::now(),
                    position,
                    collapsed,
                }),
            }),
        );
    }

    #[tokio::test]
    async fn move_session_jumps_over_collapsed_project() {
        use agentd_protocol::{MoveDirection, SessionKind};
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        // Display order: ungrouped (su-top, su-mover), then collapsed group
        // `gcol` (members hidden), then expanded group `gexp`.
        insert_group(&mgr, "gcol", 0, true).await;
        insert_group(&mgr, "gexp", 1, false).await;
        for (id, position, group) in [
            ("su-top", 0, None),
            ("su-mover", 10, None),
            ("gc-1", 0, Some("gcol".to_string())),
            ("gc-2", 1, Some("gcol".to_string())),
            ("ge-1", 0, Some("gexp".to_string())),
        ] {
            mgr.sessions.write().await.insert(
                id.into(),
                synthetic_entry_with_group(id, SessionKind::User, position, group),
            );
        }

        // Moving down past the collapsed group jumps the whole project in one
        // step: the session skips `gcol`'s hidden members and lands at the top
        // of the next visible region (`gexp`) without interleaving with them.
        mgr.move_session("su-mover", MoveDirection::Down)
            .await
            .expect("move down");

        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "su-mover").unwrap();
        assert_eq!(mover.group_id.as_deref(), Some("gexp"));
        assert!(mover.position < 0, "should land above ge-1 (pos 0)");
        // The collapsed project's members are untouched.
        let gc1 = sessions.iter().find(|s| s.id == "gc-1").unwrap();
        let gc2 = sessions.iter().find(|s| s.id == "gc-2").unwrap();
        assert_eq!(gc1.group_id.as_deref(), Some("gcol"));
        assert_eq!(gc2.group_id.as_deref(), Some("gcol"));
        assert_eq!(gc1.position, 0);
        assert_eq!(gc2.position, 1);

        // Moving back up jumps the collapsed group the other way, returning the
        // session to the bottom of the ungrouped region.
        mgr.move_session("su-mover", MoveDirection::Up)
            .await
            .expect("move up");
        let sessions = mgr.list().await;
        let mover = sessions.iter().find(|s| s.id == "su-mover").unwrap();
        assert_eq!(mover.group_id, None);
        assert!(mover.position > 0, "should land below su-top (pos 0)");
    }

    #[tokio::test]
    async fn install_memory_env_sets_global_and_project_paths() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut env = HashMap::new();

        mgr.install_memory_env(&mut env, Some("g123"));

        assert_eq!(
            env.get(ENV_GLOBAL_MEMORY_FILE),
            Some(&storage.global_memory_path().to_string_lossy().to_string())
        );
        assert_eq!(
            env.get(ENV_PROJECT_MEMORY_FILE),
            Some(
                &storage
                    .project_memory_path("g123")
                    .to_string_lossy()
                    .to_string()
            )
        );
        assert_eq!(env.get(ENV_PROJECT_ID).map(String::as_str), Some("g123"));
    }

    #[tokio::test]
    async fn install_memory_env_ungrouped_sets_global_only() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let mut env = HashMap::from([
            (ENV_PROJECT_ID.to_string(), "old".to_string()),
            (
                ENV_PROJECT_MEMORY_FILE.to_string(),
                "/old/memory.md".to_string(),
            ),
        ]);

        mgr.install_memory_env(&mut env, None);

        assert!(env.contains_key(ENV_GLOBAL_MEMORY_FILE));
        assert!(!env.contains_key(ENV_PROJECT_MEMORY_FILE));
        assert!(!env.contains_key(ENV_PROJECT_ID));
    }

    #[tokio::test]
    async fn attach_clipboard_writes_session_attachment_and_reference() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        mgr.sessions.write().await.insert(
            "spaste".into(),
            synthetic_entry("spaste", agentd_protocol::SessionKind::User, 0),
        );

        let result = mgr
            .attach_clipboard(SessionAttachClipboardParams {
                session_id: "spaste".into(),
                data: base64::engine::general_purpose::STANDARD.encode(b"hello paste"),
                filename: Some("../../screen shot.png".into()),
                mime: Some("image/png".into()),
            })
            .await
            .expect("attach clipboard");

        assert!(result.reference.starts_with("[#file:"));
        assert!(result.reference.ends_with(']'));
        assert!(result.path.contains("/sessions/spaste/attachments/"));
        assert!(result.path.ends_with(".png"));
        assert_eq!(
            tokio::fs::read(&result.path)
                .await
                .expect("read attachment"),
            b"hello paste"
        );
    }

    #[tokio::test]
    async fn list_includes_subagent_sessions_for_clients_to_nest() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        mgr.sessions.write().await.insert(
            "suser".into(),
            synthetic_entry("suser", agentd_protocol::SessionKind::User, 0),
        );
        mgr.sessions.write().await.insert(
            "ssub".into(),
            synthetic_entry("ssub", agentd_protocol::SessionKind::Subagent, -1),
        );

        let sessions = mgr.list().await;
        assert_eq!(sessions.len(), 2);
        assert!(sessions
            .iter()
            .any(|s| s.id == "suser" && s.kind == agentd_protocol::SessionKind::User));
        assert!(sessions
            .iter()
            .any(|s| s.id == "ssub" && s.kind == agentd_protocol::SessionKind::Subagent));
    }

    /// Browser previews are ephemeral, live-only UI (a base64 PNG shown as
    /// an overlay / matrix-rain wallpaper). They must NEVER reach the
    /// transcript: persisting full-size screenshots would bloat
    /// transcript.jsonl and slow every load (`read_transcript` parses each
    /// line), with no transcript consumer — clients render them only from
    /// the live broadcast. Normal structured events must still persist.
    #[tokio::test]
    async fn browser_preview_is_not_persisted_to_transcript() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sbrowser";
        let entry = synthetic_entry(id, agentd_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Control: a normal structured event MUST be persisted.
        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: agentd_protocol::MessageRole::Assistant,
                text: "hi".into(),
            },
        )
        .await;

        // The browser preview (with a stand-in base64 image) MUST NOT be.
        mgr.handle_event(
            &entry,
            SessionEvent::BrowserPreview(agentd_protocol::BrowserPreview {
                url: "https://example.test".into(),
                title: Some("Example".into()),
                image: "QUJD".into(), // base64("ABC")
                width: 2,
                height: 1,
            }),
        )
        .await;

        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::BrowserPreview(_))),
            "BrowserPreview must not be written to the transcript"
        );
        assert!(
            transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::Message { .. })),
            "control: a normal Message event should still be persisted"
        );
    }

    /// `ToolApprovalResolved` is a transient UI-dismissal signal: it must
    /// be broadcast live (so passive clients can close a stale approval
    /// prompt) but never written to the transcript — same treatment as
    /// `BrowserPreview` / `AgentStatus`.
    #[tokio::test]
    async fn tool_approval_resolved_is_not_persisted_to_transcript() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sresolved";
        let entry = synthetic_entry(id, agentd_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Control: a normal structured event MUST be persisted.
        mgr.handle_event(
            &entry,
            SessionEvent::Message {
                role: agentd_protocol::MessageRole::Assistant,
                text: "hi".into(),
            },
        )
        .await;

        // The transient approval-resolved signal MUST NOT be.
        mgr.handle_event(
            &entry,
            SessionEvent::ToolApprovalResolved {
                call_id: "call-1".into(),
            },
        )
        .await;

        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ToolApprovalResolved { .. })),
            "ToolApprovalResolved must not be written to the transcript"
        );
        assert!(
            transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::Message { .. })),
            "control: a normal Message event should still be persisted"
        );
    }

    /// Inline PTY approval prompts can change the approval mode locally
    /// inside the adapter (`a` / `f`). The adapter reports that state change
    /// back to the daemon with `ApprovalModeChanged`; the daemon must update
    /// the session summary so modelines and other clients stop showing the
    /// stale mode, without recording a transcript row.
    #[tokio::test]
    async fn approval_mode_changed_updates_summary_without_transcript_row() {
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage.clone(), config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sapprovalmode";
        let entry = synthetic_entry(id, agentd_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        mgr.handle_event(
            &entry,
            SessionEvent::ApprovalModeChanged {
                mode: agentd_protocol::ApprovalMode::UnsafeAuto,
            },
        )
        .await;

        let summary = storage.load_summary(id).expect("summary");
        assert_eq!(
            summary.approval_mode,
            agentd_protocol::ApprovalMode::UnsafeAuto
        );
        let transcript = storage
            .read_transcript(id, 0, None)
            .expect("read transcript");
        assert!(
            !transcript
                .events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ApprovalModeChanged { .. })),
            "ApprovalModeChanged must not be written to the transcript"
        );
    }

    /// Regression for the post-#69 "all sessions go to `done` after
    /// graceful daemon restart" bug: when `shutdown_adapters` is in
    /// flight, any `SessionEvent::Done` or `AdapterMessage::Closed`
    /// that flushes out of a dying adapter must NOT transition the
    /// session to a terminal state. Otherwise `resume_running_sessions`
    /// on the next boot skips the session and the user has to restart
    /// it manually.
    #[tokio::test]
    async fn handle_event_preserves_state_during_shutdown() {
        use chrono::Utc;
        use std::sync::atomic::Ordering;
        use tempfile::tempdir;
        use tokio::sync::RwLock;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");
        let manager = Arc::new(mgr);

        // Synthetic session in `Running` (what a live shell / smith
        // session looks like just before the user hits Ctrl-C on the
        // daemon).
        let id = "stest_shutdown".to_string();
        let summary = agentd_protocol::SessionSummary {
            id: id.clone(),
            harness: "shell".into(),
            cwd: "/tmp".into(),
            title: None,
            state: SessionState::Running,
            created_at: Utc::now(),
            last_event_at: None,
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 0,
            has_pty: true,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: None,
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: agentd_protocol::SessionKind::User,
            archived: false,
        };
        let entry = Arc::new(SessionEntry {
            id: id.clone(),
            summary: RwLock::new(summary),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(None),
            pty: tokio::sync::Mutex::new(PtyState::default()),
            deleted: AtomicBool::new(false),
            title_gen_attempted: AtomicBool::new(false),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
        });
        manager
            .sessions
            .write()
            .await
            .insert(id.clone(), entry.clone());

        // Pre-shutdown: a `Done` event WOULD transition state.
        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Done,
            "sanity: without the shutdown flag, Done transitions state",
        );

        // Reset and flip the shutdown flag (what `shutdown_adapters`
        // does before sending SHUTDOWN to each adapter).
        entry.summary.write().await.state = SessionState::Running;
        manager.is_shutting_down.store(true, Ordering::Release);

        // Same `Done` event during shutdown must be dropped — the
        // session needs to keep its `Running` state on disk so the
        // next boot's `resume_running_sessions` picks it up.
        manager
            .handle_event(&entry, SessionEvent::Done { exit_code: 0 })
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Running,
            "Done during shutdown must NOT transition state — that's \
             the resume regression we're guarding against",
        );

        // Error events are the same shape and must also be dropped.
        manager
            .handle_event(
                &entry,
                SessionEvent::Error {
                    message: "adapter died".into(),
                },
            )
            .await;
        assert_eq!(
            entry.summary.read().await.state,
            SessionState::Running,
            "Error during shutdown must NOT transition state either",
        );
    }

    fn create_params(mode: Option<&str>, pty: Option<PtySize>) -> CreateSessionParams {
        CreateSessionParams {
            harness: "shell".into(),
            cwd: "/tmp".into(),
            prompt: None,
            model: None,
            title: None,
            mode: mode.map(str::to_string),
            pty_size: pty,
            worktree: false,
            env: Default::default(),
            args: Vec::new(),
            kind: agentd_protocol::SessionKind::User,
            parent_session_id: None,
            group_id: None,
        }
    }

    /// An explicit `mode` from the client always wins, regardless of
    /// whether a PTY size was supplied.
    #[test]
    fn effective_mode_honors_explicit_mode() {
        assert_eq!(
            effective_mode(&create_params(Some("headless"), None)),
            "headless"
        );
        assert_eq!(
            effective_mode(&create_params(Some("interactive"), None)),
            "interactive"
        );
        // Explicit mode wins even when a PTY size is also present.
        assert_eq!(
            effective_mode(&create_params(
                Some("headless"),
                Some(PtySize { cols: 80, rows: 24 })
            )),
            "headless"
        );
    }

    /// No explicit mode, but a PTY size was requested → the session is
    /// interactive (matches the adapters' own default heuristic).
    #[test]
    fn effective_mode_defaults_to_interactive_with_pty() {
        assert_eq!(
            effective_mode(&create_params(None, Some(PtySize { cols: 80, rows: 24 }))),
            "interactive"
        );
    }

    /// No explicit mode and no PTY size → headless. This is the case
    /// the PR fixes: previously `mode` stayed `None` on disk, so the
    /// remote UI couldn't tell a headless session apart from an
    /// interactive one and rendered it as a terminal instead of chat.
    #[test]
    fn effective_mode_defaults_to_headless_without_pty() {
        assert_eq!(effective_mode(&create_params(None, None)), "headless");
    }

    #[tokio::test]
    async fn transcript_tail_returns_last_n_with_live_total() {
        use chrono::Utc;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "stail";
        let entry = synthetic_entry(id, agentd_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());

        // Simulate 1234 persisted events. The live transcript_count is what
        // `transcript(.., tail: …)` must surface as `total` — that's the
        // signal the webui uses to decide whether to background-load older
        // pages above the tail.
        for seq in 1..=1234u64 {
            let ev = agentd_protocol::TimestampedEvent {
                seq,
                at: Utc::now(),
                event: agentd_protocol::SessionEvent::Message {
                    role: agentd_protocol::MessageRole::Assistant,
                    text: format!("e{seq}"),
                },
            };
            storage_handle.append_event(id, &ev).expect("append event");
            entry
                .transcript_count
                .store(seq, std::sync::atomic::Ordering::Relaxed);
        }

        let result = mgr
            .transcript(id, 0, None, Some(50))
            .await
            .expect("transcript tail");

        assert_eq!(result.total, 1234, "total must come from the live counter");
        assert_eq!(result.events.len(), 50);
        assert_eq!(result.events.first().unwrap().seq, 1185);
        assert_eq!(result.events.last().unwrap().seq, 1234);
    }

    #[tokio::test]
    async fn pty_replay_returns_full_disk_tail_not_just_old_ring() {
        use base64::Engine;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sreplay";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, agentd_protocol::SessionKind::User, 0),
        );

        // Write 1 MiB to pty.log — that's 4× the size of the old in-memory
        // ring. Previously pty_replay would have returned only the tail
        // 256 KiB; now it must return the whole file.
        let bytes: Vec<u8> = (0..1024u32 * 1024).map(|i| (i % 251) as u8).collect();
        storage_handle
            .append_pty_bytes(id, &bytes)
            .expect("append pty bytes");

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert_eq!(
            decoded, bytes,
            "pty_replay must return the full on-disk tail, not a truncated window"
        );
    }

    #[tokio::test]
    async fn pty_replay_returns_empty_when_pty_log_missing() {
        use base64::Engine;
        use tempfile::tempdir;

        // No bytes have ever been written for this session. pty_replay must
        // return an empty body (not error) and surface the stored PTY size
        // so the TUI can still size its parsers on attach.
        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "snopty";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, agentd_protocol::SessionKind::User, 0),
        );

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert!(
            decoded.is_empty(),
            "no pty.log → empty replay, got {} bytes",
            decoded.len()
        );
    }

    #[tokio::test]
    async fn pty_replay_preserves_pty_size_through_round_trip() {
        use tempfile::tempdir;

        // Refactor moved pty_replay off `PtyState` for the bytes but it
        // still reads `size` from the same lock. Lock that the change
        // didn't accidentally start returning None.
        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "ssize";
        let entry = synthetic_entry(id, agentd_protocol::SessionKind::User, 0);
        mgr.sessions.write().await.insert(id.into(), entry.clone());
        entry.pty.lock().await.size = Some(PtySize {
            cols: 132,
            rows: 50,
        });

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        assert_eq!(
            result.size,
            Some(PtySize {
                cols: 132,
                rows: 50
            })
        );
    }

    #[tokio::test]
    async fn pty_replay_caps_at_replay_max_for_huge_logs() {
        use base64::Engine;
        use tempfile::tempdir;

        let tmp = tempdir().expect("tempdir");
        let storage =
            Arc::new(crate::storage::Storage::new(tmp.path().join("data")).expect("storage"));
        let storage_handle = storage.clone();
        let config = Arc::new(crate::config::Config::default());
        let (mgr, _remote_rx, _restart_rx) =
            SessionManager::new(storage, config, tmp.path().join("run"))
                .await
                .expect("session manager");

        let id = "sreplaybig";
        mgr.sessions.write().await.insert(
            id.into(),
            synthetic_entry(id, agentd_protocol::SessionKind::User, 0),
        );

        // Write PTY_REPLAY_CAP + 1 MiB. Replay must return at most
        // PTY_REPLAY_CAP, and the bytes returned must be the *tail* (most
        // recent) of the file — older content is what we're willing to
        // drop, not newer.
        let extra: usize = 1024 * 1024;
        let total: usize = PTY_REPLAY_CAP + extra;
        let bytes: Vec<u8> = (0..total as u32).map(|i| (i % 251) as u8).collect();
        storage_handle
            .append_pty_bytes(id, &bytes)
            .expect("append pty bytes");

        let result = mgr.pty_replay(id).await.expect("pty_replay");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&result.data)
            .expect("base64 decode");
        assert_eq!(
            decoded.len(),
            PTY_REPLAY_CAP,
            "replay must cap at PTY_REPLAY_CAP"
        );
        assert_eq!(
            decoded,
            bytes[extra..],
            "replay must be the tail of the file (most recent bytes), not the head"
        );
    }
}
