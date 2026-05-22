//! TUI app state and event loop.

use crate::keymap::{self, ChordState, KeyAction, Keymap, KeymapResult, Profile};
use crate::ui;
use agentd_client::Client;
use agentd_protocol::{
    EventNotificationPayload, GroupSummary, HarnessInfo, Notification, Request, SessionEvent,
    SessionSummary, StateNotificationPayload, TimestampedEvent,
};
use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Stdout, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Which pane currently owns the keyboard. `View` covers both the transcript
/// and the terminal renderer — when the view shows a PTY-backed session and
/// View has focus, keystrokes are captured by the PTY (with `C-x` as the
/// escape prefix back to agentd commands).
/// Max scrollback rows kept by each [`vt100::Parser`]. Mouse-wheel can scroll
/// up to this many lines into history.
pub const SCROLLBACK_MAX: usize = 5_000;
pub const MINIBUFFER_PANEL_H_DEFAULT: u16 = 13;
pub const MINIBUFFER_PANEL_H_MIN: u16 = 3;
pub const MINIBUFFER_PANEL_H_MAX: u16 = 80;

/// A row in the rendered list view. Sessions and group headers share the
/// list; key dispatch and selection are typed.
#[derive(Debug, Clone)]
pub enum ListItem {
    Session {
        summary: SessionSummary,
        indented: bool,
    },
    GroupHeader {
        group: GroupSummary,
        member_count: usize,
    },
}

fn is_list_visible_session(s: &SessionSummary) -> bool {
    matches!(s.kind, agentd_protocol::SessionKind::User)
}

impl ListItem {
    pub fn matches(&self, sel: &Selection) -> bool {
        match (self, sel) {
            (ListItem::Session { summary, .. }, Selection::Session(id)) => summary.id == *id,
            (ListItem::GroupHeader { group, .. }, Selection::Group(id)) => group.id == *id,
            _ => false,
        }
    }
}

/// What's currently focused in the list pane.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Selection {
    #[default]
    None,
    Session(String),
    Group(String),
}

impl Selection {
    pub fn session_id(&self) -> Option<&str> {
        if let Self::Session(id) = self {
            Some(id)
        } else {
            None
        }
    }
    pub fn group_id(&self) -> Option<&str> {
        if let Self::Group(id) = self {
            Some(id)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneFocus {
    List,
    View,
}

/// What the right pane is currently showing for the selected session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Structured transcript renderer (default for headless / non-PTY sessions).
    Transcript,
    /// Live PTY emulator (default for sessions whose adapter has supports_pty).
    Terminal,
}

/// Which pane (if any) currently takes the entire screen. Zoom mirrors
/// tmux's `prefix z`: a single key collapses the rest of the layout
/// onto a single pane and back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ZoomMode {
    #[default]
    None,
    /// Session list fills the screen (minibuffer + modeline still
    /// visible). `C-x o` from here flips to `View`.
    List,
    /// Session view fills the screen. `C-x o` from here flips to
    /// `List`.
    View,
}

#[derive(Debug, Clone)]
pub enum MinibufferIntent {
    SendInput {
        session_id: String,
    },
    NewSessionHarness,
    /// Second stage of the new-session wizard when the user typed `group`:
    /// asks for the group's name.
    NewGroupName,
    DeleteConfirm {
        session_id: String,
    },
    /// Confirmation prompt for restarting a terminated (`Done` /
    /// `Errored`) session. Single-key dispatch: `y`/Enter respawns
    /// the adapter (with `AGENTD_RESUME=1` so persistent harnesses
    /// reload state); anything else cancels.
    RestartConfirm {
        session_id: String,
    },
    Rename {
        session_id: String,
    },
    GroupDeleteConfirm {
        group_id: String,
    },
    GroupRename {
        group_id: String,
    },
    CommandPalette,
    /// Persistent orchestrator session input. Unlike other intents
    /// this one stays open across Enter — the panel re-opens with an
    /// empty input after each submission. Slash-prefixed input is
    /// dispatched locally (no LLM cost); non-slash input is sent to
    /// the orchestrator session via `session.send_input`.
    Orchestrator,
    /// Approval prompt for a Risky tool call from an agent harness
    /// (currently zarvis). Single-key dispatch: `y`/Enter approve,
    /// `n`/Esc deny, `a` approve + flip automode.
    ApproveTool {
        session_id: String,
        call_id: String,
        tool: String,
        args_summary: String,
        risk: agentd_protocol::ToolRisk,
    },
}

#[derive(Debug, Clone)]
pub struct Minibuffer {
    pub prompt: String,
    pub input: String,
    pub cursor: usize,
    pub intent: MinibufferIntent,
    /// Inline status appended after the input. Examples: "no such harness",
    /// "matches: claude, codex". Cleared by the next text edit.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct ScreenPoint {
    pub col: u16,
    pub row: u16,
}

#[derive(Debug, Clone)]
pub struct TextSelection {
    pub anchor: ScreenPoint,
    pub head: ScreenPoint,
    pub dragged: bool,
    pub bounds: Option<ratatui::layout::Rect>,
}

#[derive(Debug, Clone, Copy)]
pub struct TextSelectionRange {
    pub start: ScreenPoint,
    pub end: ScreenPoint,
}

pub struct App {
    pub client: Arc<Client>,
    pub sessions: Vec<SessionSummary>,
    pub groups: Vec<GroupSummary>,
    pub selection: Selection,
    pub focus: PaneFocus,
    pub transcript: Vec<TimestampedEvent>,
    pub transcript_session: Option<String>,
    pub transcript_scroll: u16,
    pub minibuffer: Option<Minibuffer>,
    pub harnesses: Vec<HarnessInfo>,
    pub theme: crate::theme::Theme,
    pub help_visible: bool,
    pub profile: Profile,
    pub keymap: Keymap,
    pub chord_state: ChordState,
    pub chord_label: String,
    pub status: Option<(String, Instant)>,
    pub last_diff: Option<String>,
    pub should_quit: bool,
    pub connected: bool,
    /// How many remote WS clients are currently attached to the
    /// daemon. Surfaced as a "● remote" badge in the modeline so
    /// the local user can see when the phone (or any future remote
    /// client) is also driving sessions. Updated by the
    /// `remote/state` notification handler.
    pub remote_clients: u32,
    // Terminal-pane state.
    pub view: ViewMode,
    /// Per-session items history. Replaces the old direct
    /// `vt100::Parser` cache: each session's PTY bytes and tool
    /// events feed an [`ItemHistory`] that the render path replays
    /// onto a fresh parser. This enables expand/collapse of tool
    /// blocks via height mutation rather than vt100 cursor-edit
    /// gymnastics. Non-zarvis sessions degrade to a single
    /// `PtyChunk` and render identically to the old pipeline.
    pub histories: HashMap<String, crate::pty_render::ItemHistory>,
    /// Per-session cached block hit-test ranges (call_id, row range
    /// within the rendered pane). Refreshed by the render functions
    /// after each `replay`. Mouse clicks in the PTY pane consult
    /// this to toggle the right block.
    pub block_hits: HashMap<String, Vec<crate::pty_render::BlockHitRect>>,
    /// The orchestrator panel's most recent inner (cols, rows) as
    /// computed during render. Written by `ui::render`, consumed by
    /// `run_loop`'s debounce — once the value stays stable for
    /// `RESIZE_DEBOUNCE_MS`, a single `pty_resize` IPC fires.
    pub orchestrator_desired_size: Option<(u16, u16)>,
    pub terminal_pane_size: (u16, u16), // (cols, rows) of the right pane.
    /// Zoom: hide list / pin strip / modeline; the session view fills the
    /// screen except for the minibuffer line at the bottom. Toggled with
    /// `C-x z` (emacs) / `z` (vim), matching tmux's prefix-z.
    pub zoom: ZoomMode,
    /// User-controlled scroll offset for the session list. 0 = first item at
    /// top. Mouse wheel over the list adjusts this; keyboard selection still
    /// lets ratatui pull the selected item back into view when needed.
    pub list_scroll_offset: usize,
    /// Scrollback offset (in rows) applied to the *focused* session's PTY
    /// parser when rendering. 0 = live view. Increased by mouse-wheel up,
    /// decreased by mouse-wheel down. Reset to 0 on user keystroke into
    /// the PTY or on session change.
    pub view_scrollback: usize,
    /// Scrollback offset for the daemon-owned orchestrator panel rendered in
    /// the minibuffer. Kept separate from `view_scrollback` so reading god
    /// history does not leave the main session view scrolled when the panel
    /// closes.
    pub orchestrator_scrollback: usize,
    /// User-preferred height for the daemon-owned orchestrator panel rendered
    /// in the minibuffer. Clamped by terminal height at render time.
    pub orchestrator_panel_h: Option<u16>,
    /// `Some((anchor_row, anchor_height))` while the user drags the
    /// orchestrator panel's top border.
    pub resizing_orchestrator_panel: Option<(u16, u16)>,
    /// Per-session "last PTY byte" timestamp, updated locally from incoming
    /// Pty events. Used to drive the "session looks busy" spinner via a
    /// short quiescence window. Daemon's `SessionSummary.last_pty_at_ms`
    /// covers cold-start / freshly-connected clients; this map covers the
    /// live high-frequency case.
    pub pty_activity: HashMap<String, Instant>,
    /// Monotonic clock anchor; spinner frame index is computed against this.
    pub start_instant: Instant,
    /// Snapshot of last frame's pane geometry — used by the mouse-click
    /// handler to map terminal coordinates back to UI regions. Filled
    /// by `ui::render` each frame; `None` until the first render lands.
    pub layout: LayoutSnapshot,
    /// Most-recently observed mouse cursor position (terminal cell).
    /// `None` until the first `MouseEventKind::Moved` arrives — and stays
    /// `None` on terminals that don't forward motion events (e.g. macOS
    /// Terminal.app, which ignores `\x1b[?1003h` even though crossterm
    /// requests it).
    pub mouse_pos: Option<(u16, u16)>,
    /// Whether terminal mouse capture is enabled. When false, agentd
    /// stops receiving mouse events so the user's terminal can perform
    /// native drag selection/copy.
    pub mouse_capture_enabled: bool,
    /// ID of the daemon-owned orchestrator session, if one is present
    /// in the sessions list. The orchestrator runs as a zarvis
    /// interactive (PTY) session; the TUI renders its PTY in the
    /// minibuffer panel and routes keystrokes there when the panel is
    /// focused. `None` falls back to the static palette UX.
    pub orchestrator_id: Option<String>,
    /// Width (in terminal cells) of the session-list pane in the
    /// normal (non-zoomed) layout. Adjustable by dragging the right
    /// border with the mouse; clamped at render time to
    /// `[LIST_PANEL_W_MIN, terminal_w - LIST_PANEL_W_VIEW_MIN]`.
    pub list_panel_w: u16,
    /// `Some((anchor_col, anchor_width))` while the user is
    /// mid-drag on the list/view divider — `anchor_col` is the
    /// column where Mouse-Down landed, `anchor_width` is the list
    /// pane's width at drag start. On each `Drag` event we apply
    /// the column delta to the anchor width, so it doesn't matter
    /// whether the user grabbed the list's right border, the
    /// view's left border, or the first pin tile's left border —
    /// the divider follows the cursor either way. Cleared on
    /// `Up(Left)`.
    pub resizing_list: Option<(u16, u16)>,
    /// User-preferred pin strip height in cells. `None` =
    /// auto-compute via `ui::pin_strip_height(total)` (≈ ⅓ of the
    /// right pane, clamped to 7..=18). Adjustable by dragging the
    /// bottom border of the main view (= top border of the pin
    /// strip). Persisted across launches.
    pub pin_strip_h: Option<u16>,
    /// `Some((anchor_row, anchor_height))` while the user is
    /// mid-drag on the view/pin-strip horizontal divider — mirrors
    /// the `resizing_list` model but for the vertical axis.
    pub resizing_pin_strip: Option<(u16, u16)>,
    /// User-preferred Matrix-rain panel height in cells. `None` =
    /// default to about 200px worth of terminal rows, clamped to the empty
    /// space below the list items.
    pub matrix_rain_h: Option<u16>,
    /// `Some((anchor_row, anchor_height))` while the user drags the
    /// Matrix-rain title bar to resize the panel.
    pub resizing_matrix_rain: Option<(u16, u16)>,
    /// User has collapsed the session list pane via the `−` button
    /// on its title bar. Effective only when the list pane doesn't
    /// have focus — when focus is on the list (e.g. via `C-x o`),
    /// the list temporarily renders at its full width so the user
    /// can interact with it, then re-collapses when focus leaves.
    /// Persisted across launches.
    pub list_collapsed: bool,
    /// /tasks popup state: `None` = closed, `Some(...)` = open with
    /// a snapshot of the session's task registry.
    pub tasks_popup: Option<TasksPopup>,
    /// Live `/remote-control` modal — URL + QR for the active
    /// remote-WS deployment. `Some` while open, `None` otherwise.
    /// Dismissed with Esc the same way `tasks_popup` is.
    pub remote_control_popup: Option<RemoteControlPopup>,
    pub remote_control_task:
        Option<tokio::task::JoinHandle<(bool, Result<agentd_protocol::RemoteStartResult>)>>,
    /// Per-session input editor state, fed by `SessionEvent::EditorState`
    /// from the adapter (currently zarvis interactive). Drives the
    /// fixed bottom input pane.
    pub editor_states: HashMap<String, EditorState>,
    /// Per-session live agent status, fed by `SessionEvent::AgentStatus`
    /// and rendered above queued input while a turn is active.
    pub agent_statuses: HashMap<String, agentd_protocol::AgentStatus>,
    /// Short visual transition when the main view switches to a different
    /// session.
    pub session_transition: Option<SessionTransition>,
    /// Short visual transitions for newly visible pinned-session tiles.
    pub pin_transitions: HashMap<String, Instant>,
    /// Ambient Matrix-rain panel state for empty rows in the session list.
    pub matrix_rain: crate::matrix_rain::MatrixRain,
    /// Smoothed 0..1 foreground intensity for Matrix rain. The render path
    /// eases this toward current fleet activity so rain ramps up and decays
    /// instead of snapping between idle and active states.
    pub matrix_rain_intensity: f32,
    pub matrix_rain_intensity_updated_at: Instant,
    pub matrix_rain_foreground_epoch: Instant,
    /// Matrix-rain drop cycle keys that already spawned. Intensity decay stops
    /// future cycles from entering this set; existing drops finish their fall.
    pub matrix_rain_active_drops: HashMap<u64, u16>,
    /// User-hidden Matrix-rain panel. Toggle with `/rain`; close with the
    /// panel's `x` button.
    pub matrix_rain_hidden: bool,
    /// Hide left, right, and bottom border lines for list/view/pin panes.
    pub hide_pane_side_borders: bool,
    /// Last rendered frame, one string per terminal row. Mouse drag
    /// selection copies out of this snapshot, so it works across the
    /// whole TUI without every widget implementing text export.
    pub frame_text: Vec<String>,
    /// In-app text selection driven by left-drag while mouse capture is on.
    pub text_selection: Option<TextSelection>,
    /// Copied selection text. After mouse release we re-find this text in
    /// the latest rendered frame so the highlight follows content shifts.
    pub selected_text: Option<String>,
    pub selected_text_bounds: Option<ratatui::layout::Rect>,
    pub selected_text_range: Option<TextSelectionRange>,
    pty_input_tx: mpsc::UnboundedSender<PtyInputJob>,
    pty_input_errors: mpsc::UnboundedReceiver<String>,
}

struct ReconnectState {
    next_attempt: Instant,
    backoff: Duration,
}

impl ReconnectState {
    fn new(now: Instant) -> Self {
        Self {
            next_attempt: now,
            backoff: Duration::from_millis(250),
        }
    }

    fn schedule_next(&mut self, now: Instant) {
        self.next_attempt = now + self.backoff;
        self.backoff = (self.backoff * 2).min(Duration::from_secs(5));
    }
}

struct SessionHydration {
    session_id: String,
    transcript: Vec<TimestampedEvent>,
    history: Option<crate::pty_render::ItemHistory>,
    editor_state: Option<EditorState>,
    agent_status: Option<agentd_protocol::AgentStatus>,
    status_messages: Vec<String>,
}

struct SessionHydrationRequest {
    socket: std::path::PathBuf,
    session_id: String,
    needs_history: bool,
    terminal_pane_size: (u16, u16),
}

struct PtyInputJob {
    session_id: String,
    bytes: Vec<u8>,
    label: &'static str,
}

#[derive(Debug, Clone)]
pub struct SessionTransition {
    pub started_at: Instant,
}

/// Adapter-owned input editor state, mirrored from
/// `SessionEvent::EditorState` and rendered as a fixed bottom pane.
#[derive(Debug, Clone, Default)]
pub struct EditorState {
    pub queued: Vec<String>,
    pub buf: String,
    pub cursor: usize,
    pub completions: Vec<String>,
}

fn agent_status_history_line(status: &agentd_protocol::AgentStatus) -> Option<Vec<u8>> {
    if status.active || status.started_at_ms <= 0 || status.status.trim().is_empty() {
        return None;
    }
    let line = format!(
        "\r\n\r\n\x1b[2m* {} ({})\x1b[0m\r\n",
        status.status.trim(),
        format_elapsed(status.started_at_ms)
    );
    Some(line.into_bytes())
}

fn format_elapsed(started_at_ms: i64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(started_at_ms);
    let secs = now_ms.saturating_sub(started_at_ms).max(0) / 1000;
    let minutes = secs / 60;
    let seconds = secs % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

async fn load_session_hydration(req: SessionHydrationRequest) -> Result<SessionHydration> {
    tokio::task::spawn_blocking(move || {
        let mut status_messages = Vec::new();
        let transcript: agentd_protocol::TranscriptResult = blocking_request(
            &req.socket,
            agentd_protocol::ipc_method::SESSION_TRANSCRIPT,
            &agentd_protocol::TranscriptParams {
                session_id: req.session_id.clone(),
                from: 0,
                limit: None,
            },
        )?;

        let history = if req.needs_history {
            let mut h = crate::pty_render::ItemHistory::new();
            let pty: Result<agentd_protocol::PtyReplayResult> = blocking_request(
                &req.socket,
                agentd_protocol::ipc_method::SESSION_PTY_REPLAY,
                &agentd_protocol::SessionIdParams {
                    session_id: req.session_id.clone(),
                },
            );
            match pty {
                Ok(snap) => {
                    let (cols, rows) = snap
                        .size
                        .as_ref()
                        .map(|s| (s.cols, s.rows))
                        .unwrap_or(req.terminal_pane_size);
                    h.set_pty_size(cols, rows);
                    use base64::Engine;
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&snap.data)
                    {
                        h.feed_pty(&bytes);
                    }
                }
                Err(e) => status_messages.push(format!("pty_replay: {e}")),
            }

            let mut editor_state = None;
            let mut agent_status = None;
            if transcript
                .events
                .iter()
                .any(|ev| matches!(ev.event, SessionEvent::Pty { .. }))
            {
                // New daemons persist PTY events in the transcript as ordering
                // markers. Prefer rebuilding from those markers so transcript-only
                // items (zarvis tool blocks) are interleaved with the raw bytes in
                // chronological order. The pty_replay path above remains the
                // fallback for older sessions whose transcripts do not contain PTY.
                h.clear_items();
            }
            apply_transcript_to_local_state(
                &transcript.events,
                &mut h,
                &mut editor_state,
                &mut agent_status,
            );
            let (cols, rows) = req.terminal_pane_size;
            let _ = h.replay(cols.max(1), rows.max(1), 0);
            (Some(h), editor_state, agent_status)
        } else {
            (None, None, None)
        };

        Ok(SessionHydration {
            session_id: req.session_id,
            transcript: transcript.events,
            history: history.0,
            editor_state: history.1,
            agent_status: history.2,
            status_messages,
        })
    })
    .await
    .context("join session hydration worker")?
}

fn blocking_request<P, R>(socket: &std::path::Path, method: &str, params: &P) -> Result<R>
where
    P: serde::Serialize + ?Sized,
    R: serde::de::DeserializeOwned,
{
    use anyhow::anyhow;
    use std::io::{BufRead, Write};

    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .with_context(|| format!("connect {}", socket.display()))?;
    let req = Request::new(
        serde_json::json!(1),
        method.to_string(),
        Some(serde_json::to_value(params)?),
    );
    serde_json::to_writer(&mut stream, &req)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let mut reader = std::io::BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        return Err(anyhow!("daemon disconnected"));
    }
    let resp: agentd_protocol::Response = serde_json::from_str(line.trim())?;
    if let Some(err) = resp.error {
        return Err(anyhow!("daemon error: {}", err.message));
    }
    Ok(serde_json::from_value(
        resp.result.unwrap_or(serde_json::Value::Null),
    )?)
}

fn spawn_pty_input_pump(
    client: Arc<Client>,
) -> (
    mpsc::UnboundedSender<PtyInputJob>,
    mpsc::UnboundedReceiver<String>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<PtyInputJob>();
    let (err_tx, err_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            if let Err(e) = client.pty_input(&job.session_id, job.bytes).await {
                let _ = err_tx.send(format!("{} failed: {e}", job.label));
            }
        }
    });
    (tx, err_rx)
}

/// State for the `/tasks` modal popup. v1 is read-only at the UI
/// layer (Esc closes; clicks outside close); re-typing `/tasks`
/// refreshes the snapshot.
#[derive(Debug, Clone)]
pub struct TasksPopup {
    pub session_id: String,
    pub tasks: Vec<agentd_protocol::TaskInfo>,
}

/// Live state of the `/remote-control` (or `/remote-control-debug`)
/// modal. The `url` and `qr` are served verbatim by the daemon
/// (`remote.start` IPC); the popup just displays them.
///
/// `Ok` variant: tunnel mode that succeeded, or local-only mode.
/// `Err` variant: tunnel mode that timed out — the daemon returned
/// a diagnostic explaining why (cloudflared missing, slow network,
/// etc.). Renderer paints the diagnostic instead of a fake URL,
/// which is the fix for the "tunnel is warming up" UX trap.
#[derive(Debug, Clone)]
pub enum RemoteControlPopup {
    Starting(RemoteControlOk),
    Ok(RemoteControlOk),
    Err {
        /// Which slash was invoked, so the title still reads
        /// "/remote-control" vs "/remote-control-debug".
        local_only: bool,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct RemoteControlOk {
    pub url: String,
    pub qr: String,
    pub tunnel_ready: bool,
    /// HTTP Basic auth password for the phone to enter when the
    /// browser prompts. Displayed verbatim in the popup — copying
    /// is the easy path on macOS Terminal.app via mouse drag.
    pub password: String,
    pub hint: Option<String>,
    /// Mode the user invoked. `false` for `/remote-control` (the
    /// public-tunnel happy path), `true` for `/remote-control-debug`.
    pub local_only: bool,
}

/// Smallest list-pane width that still leaves room for the session
/// status glyph + a couple chars of name. Below this drag is clamped.
pub const LIST_PANEL_W_MIN: u16 = 18;
/// Smallest right-pane width we'll preserve while dragging — anything
/// less and the view pane stops being usable.
pub const LIST_PANEL_W_VIEW_MIN: u16 = 20;
/// Default list-pane width on first launch.
pub const LIST_PANEL_W_DEFAULT: u16 = 40;

/// Width of the list pane in collapsed state. Zero — the pane is
/// hidden entirely and the main view expands to occupy the full
/// horizontal span. The uncollapse affordance is a `›` glyph on
/// the main view's left border (see `view_uncollapse_glyph_pos`).
pub const LIST_PANEL_W_COLLAPSED: u16 = 0;

/// Bounds for the pin strip's user-adjustable height. The minimum
/// must keep the top + bottom border + one row of content visible
/// (3 cells); the maximum keeps the main session view from being
/// crushed below ~10 rows on a typical terminal — the upper end is
/// also clamped at render time against `right_area.height − 10` so
/// we never starve the main view on a small terminal regardless of
/// what was persisted.
pub const PIN_STRIP_H_MIN: u16 = 3;
pub const PIN_STRIP_H_MAX: u16 = 40;

/// Matrix-rain panel height in terminal rows. The product request was
/// "about 200px"; terminal UIs do not know pixel height, so the default is
/// a compact 12-row panel and render-time clamping shrinks it on short panes.
pub const MATRIX_RAIN_H_MIN: u16 = 4;
pub const MATRIX_RAIN_H_DEFAULT: u16 = 12;

/// Minimum number of session-list rows the layout keeps visible when
/// the matrix-rain panel is shown. Below this the list takes the
/// entire pane and the matrix is hidden — preserving the ability to
/// see and select sessions in a very short terminal.
pub const SESSION_LIST_H_MIN: u16 = 3;

/// A clickable / hoverable text segment in the minibuffer hint line —
/// e.g. "C-x z unzoom" or "? help" — that dispatches a KeyAction when
/// clicked. Geometry is filled by `render_minibuffer` so the click
/// handler can hit-test against the live last-frame layout.
#[derive(Debug, Clone, Copy)]
pub struct HintZone {
    pub x_start: u16,
    /// Exclusive end column.
    pub x_end: u16,
    pub y: u16,
    pub action: KeyAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlHit {
    pub url: String,
    pub ranges: Vec<UrlLineHit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlLineHit {
    pub row: u16,
    pub start_col: u16,
    /// Exclusive end column.
    pub end_col: u16,
}

/// Last-frame geometry for hit-testing mouse clicks.
#[derive(Debug, Clone, Default)]
pub struct LayoutSnapshot {
    pub list_area: Option<ratatui::layout::Rect>,
    pub view_area: Option<ratatui::layout::Rect>,
    pub pin_strip_area: Option<ratatui::layout::Rect>,
    pub matrix_rain_area: Option<ratatui::layout::Rect>,
    pub minibuffer_area: Option<ratatui::layout::Rect>,
    /// Number of rows of the list pane currently in use (so a click
    /// past the last row is a no-op rather than selecting an
    /// out-of-range item). Mirrors `app.list_items().len()`.
    pub list_row_count: usize,
    /// Sub-rect of the list pane where session rows are actually
    /// drawn — the inner area minus the bottom matrix-rain panel
    /// (when shown). Click hit-testing for rows uses this so clicks
    /// inside the matrix panel don't mis-fire as row selections, and
    /// it also bounds the visible window when the list scrolls.
    pub list_items_area: Option<ratatui::layout::Rect>,
    /// Scroll offset of the session list (number of items scrolled
    /// off the top). Captured from `ListState::offset()` after the
    /// last render so click-to-row mapping stays correct when the
    /// list overflows its visible area.
    pub list_scroll_offset: usize,
    /// Clickable segments in the minibuffer hint line. Empty when a
    /// minibuffer prompt (palette / send-input / etc.) is open.
    pub minibuffer_hints: Vec<HintZone>,
    /// Clickable harness names in the new-session picker prompt
    /// (`MinibufferIntent::NewSessionHarness`). Click → submit the
    /// matching name as if the user typed it and hit Enter.
    pub minibuffer_harness_hits: Vec<HarnessHit>,
    /// Bounds of the topmost modal/dialog rendered in the last frame.
    /// Mouse clicks outside this rect dismiss the modal instead of
    /// falling through to panes underneath it.
    pub modal_area: Option<ratatui::layout::Rect>,
}

#[derive(Debug, Clone)]
pub struct HarnessHit {
    pub name: String,
    pub x_start: u16,
    /// Exclusive end column.
    pub x_end: u16,
    pub y: u16,
    /// `false` for harnesses whose adapter binary isn't on PATH —
    /// rendered dimmed + struck-through, click is a no-op + status
    /// line note, hover shows a "not installed" tooltip.
    pub available: bool,
}

fn selection_bounds_for_layout(
    layout: &LayoutSnapshot,
    pinned_count: usize,
    is_orchestrator_panel: bool,
    col: u16,
    row: u16,
) -> Option<ratatui::layout::Rect> {
    fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
        c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
    }
    fn inner(r: ratatui::layout::Rect) -> ratatui::layout::Rect {
        ratatui::layout::Rect {
            x: r.x.saturating_add(1),
            y: r.y.saturating_add(1),
            width: r.width.saturating_sub(2),
            height: r.height.saturating_sub(2),
        }
    }

    if let Some(list) = layout.list_area {
        let list_inner = inner(list);
        if contains(list_inner, col, row) {
            return Some(list_inner);
        }
    }

    if let Some(view) = layout.view_area {
        let view_inner = inner(view);
        if contains(view_inner, col, row) {
            return Some(view_inner);
        }
    }

    if let Some(strip) = layout.pin_strip_area {
        for tile in crate::ui::pin_tile_layout(strip, pinned_count) {
            let tile_inner = inner(tile);
            if contains(tile_inner, col, row) {
                return Some(tile_inner);
            }
        }
    }

    if let Some(minibuffer) = layout.minibuffer_area {
        let minibuffer_content = if is_orchestrator_panel {
            ratatui::layout::Rect {
                x: minibuffer.x,
                y: minibuffer.y.saturating_add(1),
                width: minibuffer.width,
                height: minibuffer.height.saturating_sub(1),
            }
        } else {
            minibuffer
        };
        if contains(minibuffer_content, col, row) {
            return Some(minibuffer_content);
        }
    }

    None
}

/// Window during which a session counts as "busy" after its last PTY byte.
/// Claude/codex TUIs emit a frame every ~80ms while thinking, so 600ms
/// covers a missed frame without falsely flapping to idle.
pub const PTY_QUIESCENCE: Duration = Duration::from_millis(600);
/// Spinner frame cadence — fast enough to feel alive, slow enough to keep
/// the TUI tick loop cheap.
pub const SPINNER_FRAME_MS: u128 = 120;
/// Pulsing-star spinner: a 4-glyph sparkle whose size "breathes" via a
/// palindromic frame schedule (small → big → small). Single cell wide so
/// it slots into the same column as the static state glyph.
pub const SPINNER_FRAMES: [&str; 8] = ["✦", "✧", "✶", "✷", "✸", "✷", "✶", "✧"];
/// Duration of the session-switch visual transition.
pub const SESSION_TRANSITION_MS: u128 = 200;

#[allow(dead_code)]
pub async fn run(client: Arc<Client>) -> Result<()> {
    run_with_socket(client.socket_path().to_path_buf()).await
}

pub async fn run_with_socket(socket: std::path::PathBuf) -> Result<()> {
    let client = Client::connect(&socket).await?;
    let profile = Profile::from_env();
    let keymap = keymap::default_for(profile);

    // Initial fetches.
    let sessions = client.list().await.unwrap_or_default();
    let groups = client.list_groups().await.unwrap_or_default();
    let harnesses = client.harnesses().await.unwrap_or_default();
    let (theme, theme_warning) = crate::theme::Theme::load_or_default();
    let initial_orch_id = sessions
        .iter()
        .find(|s| s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal())
        .map(|s| s.id.clone());
    // Restore the previously-selected session if it still exists,
    // else fall back to the first non-orchestrator session.
    let persisted = crate::tui_state::load();
    let initial_zoom = persisted.zoom;
    let initial_focus = match initial_zoom {
        ZoomMode::List => PaneFocus::List,
        ZoomMode::View | ZoomMode::None => PaneFocus::View,
    };
    let initial_sel = persisted
        .last_selected_session_id
        .as_ref()
        .and_then(|id| {
            sessions
                .iter()
                .find(|s| s.id == *id && is_list_visible_session(s))
                .map(|s| Selection::Session(s.id.clone()))
        })
        .or_else(|| {
            sessions
                .iter()
                .find(|s| is_list_visible_session(s))
                .map(|s| Selection::Session(s.id.clone()))
        })
        .unwrap_or(Selection::None);

    let now = Instant::now();
    let socket = client.socket_path().to_path_buf();
    let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());
    let mut app = App {
        client: client.clone(),
        sessions,
        groups,
        selection: initial_sel,
        // Default focus is the view — the selected session is usually
        // what the user wants to interact with first. List navigation
        // is one `C-x o` / `Tab` away.
        focus: initial_focus,
        transcript: Vec::new(),
        transcript_session: None,
        transcript_scroll: 0,
        minibuffer: None,
        harnesses,
        theme,
        help_visible: false,
        profile,
        keymap,
        chord_state: ChordState::default(),
        chord_label: String::new(),
        status: None,
        last_diff: None,
        should_quit: false,
        connected: true,
        remote_clients: 0,
        view: ViewMode::Transcript,
        histories: HashMap::new(),
        block_hits: HashMap::new(),
        orchestrator_desired_size: None,
        tasks_popup: None,
        remote_control_popup: None,
        remote_control_task: None,
        terminal_pane_size: (100, 30),
        zoom: initial_zoom,
        list_scroll_offset: 0,
        view_scrollback: 0,
        orchestrator_scrollback: 0,
        orchestrator_panel_h: persisted.orchestrator_panel_h,
        resizing_orchestrator_panel: None,
        pty_activity: HashMap::new(),
        start_instant: now,
        layout: LayoutSnapshot::default(),
        mouse_pos: None,
        mouse_capture_enabled: true,
        orchestrator_id: initial_orch_id,
        list_panel_w: persisted.list_panel_w.unwrap_or(LIST_PANEL_W_DEFAULT),
        resizing_list: None,
        pin_strip_h: persisted.pin_strip_h,
        resizing_pin_strip: None,
        matrix_rain_h: persisted.matrix_rain_h,
        resizing_matrix_rain: None,
        list_collapsed: persisted.list_collapsed,
        editor_states: HashMap::new(),
        agent_statuses: HashMap::new(),
        session_transition: None,
        pin_transitions: HashMap::new(),
        matrix_rain: crate::matrix_rain::MatrixRain::default(),
        matrix_rain_intensity: 0.0,
        matrix_rain_intensity_updated_at: now,
        matrix_rain_foreground_epoch: now,
        matrix_rain_active_drops: HashMap::new(),
        matrix_rain_hidden: persisted.matrix_rain_hidden,
        hide_pane_side_borders: persisted.hide_pane_side_borders,
        frame_text: Vec::new(),
        text_selection: None,
        selected_text: None,
        selected_text_bounds: None,
        selected_text_range: None,
        pty_input_tx,
        pty_input_errors,
    };
    if let Some(warning) = theme_warning {
        app.status = Some((warning, Instant::now()));
    }
    // Default to Terminal view when the currently-selected session has a PTY.
    if app.selected_session().map(|s| s.has_pty).unwrap_or(false) {
        app.view = ViewMode::Terminal;
    }

    // Subscribe to all session events.
    if let Err(e) = client.subscribe(None).await {
        app.status = Some((format!("subscribe failed: {e}"), Instant::now()));
    }
    // Load transcript for the first session if any.
    app.refresh_selected_transcript().await;
    // Bootstrap parsers for every pinned PTY session so the pin strip has
    // content from frame 0 — without this, tiles render "(no data yet)"
    // until the user focuses each one and the daemon's ring buffer is
    // pulled via pty_replay.
    app.ensure_pinned_parsers().await;

    // Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("enter alternate screen / enable mouse")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(&mut terminal, &mut app, socket).await;

    // Teardown — best effort.
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    );
    terminal.show_cursor().ok();

    crate::tui_state::save(&crate::tui_state::TuiState {
        last_selected_session_id: app.selection.session_id().map(|s| s.to_string()),
        zoom: app.zoom,
        list_panel_w: Some(app.list_panel_w),
        pin_strip_h: app.pin_strip_h,
        orchestrator_panel_h: app.orchestrator_panel_h,
        matrix_rain_h: app.matrix_rain_h,
        list_collapsed: app.list_collapsed,
        matrix_rain_hidden: app.matrix_rain_hidden,
        hide_pane_side_borders: app.hide_pane_side_borders,
    });

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    socket: std::path::PathBuf,
) -> Result<()> {
    let mut input_stream = EventStream::new();
    let mut notifications = app
        .client
        .take_notifications()
        .await
        .context("notifications channel already taken")?;
    let mut reconnect: Option<ReconnectState> = None;
    // Tick at the spinner frame boundary so each frame gets one redraw.
    let mut tick = tokio::time::interval(Duration::from_millis(SPINNER_FRAME_MS as u64));

    // Debounce window for resize events. Terminal-app drags and
    // list/view divider drags both flood `terminal_pane_size` (and
    // the orchestrator panel's size) with many close-spaced values;
    // firing `pty_resize` per frame creates IPC churn and asks every
    // child PTY to reflow repeatedly. Sitting on the size until it
    // stays stable for this window collapses the storm into one IPC.
    let resize_debounce = Duration::from_millis(100);
    let mut last_size_sent: (u16, u16) = (0, 0);
    let mut pending_size: Option<((u16, u16), Instant)> = None;
    let mut last_orch_sent: (u16, u16) = (0, 0);
    let mut pending_orch: Option<((u16, u16), Instant)> = None;
    // Track the most recent session we've sent a resize for. Switching
    // sessions counts as a resize-event-of-interest even when the
    // dimensions are unchanged — claude/codex draw their UI to the
    // PTY size they last received and don't refresh past content
    // without a SIGWINCH, so the focused session needs a fresh resize
    // every time it gains focus.
    let mut last_session_sent: Option<String> = None;
    let mut hydration_session: Option<String> = None;
    let mut hydration_task: Option<tokio::task::JoinHandle<Result<SessionHydration>>> = None;
    while !app.should_quit {
        while let Ok(msg) = app.pty_input_errors.try_recv() {
            app.set_status(msg);
        }
        if app.connected && app.client.is_disconnected() {
            app.connected = false;
            reconnect = Some(ReconnectState::new(Instant::now()));
            app.set_status("daemon disconnected — reconnecting… (press q to quit)".to_string());
        }
        app.prune_finished_transitions();
        app.poll_remote_control_task().await;
        if let Some(state) = reconnect.as_mut() {
            let now = Instant::now();
            if now >= state.next_attempt {
                match app.reconnect(&socket).await {
                    Ok(rx) => {
                        notifications = rx;
                        reconnect = None;
                        last_size_sent = (0, 0);
                        last_orch_sent = (0, 0);
                        last_session_sent = None;
                        hydration_session = None;
                        if let Some(task) = hydration_task.take() {
                            task.abort();
                        }
                    }
                    Err(e) => {
                        state.schedule_next(now);
                        app.set_status(format!(
                            "daemon disconnected — reconnecting… (press q to quit; last error: {e})"
                        ));
                    }
                }
            }
        }
        terminal.draw(|f| ui::render(f, app))?;

        // A session switch should stay interactive while history-sized
        // work runs. Selection handlers only mark the transcript as
        // stale; after the frame above has painted the new list highlight
        // / placeholder view, start transcript + PTY hydration in the
        // background. If the user switches again, abort the old task and
        // discard any stale result.
        let desired_hydration_session = app.selection.session_id().map(|s| s.to_string());
        if hydration_session != desired_hydration_session {
            if let Some(task) = hydration_task.take() {
                task.abort();
            }
            hydration_session = None;
        }
        if app.selected_needs_hydration() && hydration_task.is_none() {
            if let Some(req) = app.selected_hydration_request() {
                hydration_session = Some(req.session_id.clone());
                hydration_task = Some(tokio::spawn(load_session_hydration(req)));
            }
        }

        // Right pane (main session) resize — debounced fire. Also
        // refires if the *selected* session changed since last sent.
        let cur = app.terminal_pane_size;
        let cur_session = app.selected_id();
        let session_changed = cur_session != last_session_sent;
        if cur.0 > 0 && cur.1 > 0 && (cur != last_size_sent || session_changed) {
            match pending_size {
                Some((p, _)) if p == cur && !session_changed => {}
                _ => pending_size = Some((cur, Instant::now())),
            }
        } else {
            pending_size = None;
        }
        if let Some((size, at)) = pending_size {
            if at.elapsed() >= resize_debounce || session_changed {
                app.notify_pane_size(size.0, size.1).await;
                last_size_sent = size;
                last_session_sent = cur_session;
                pending_size = None;
            }
        }
        // Orchestrator panel resize — same debounce, separate target.
        if let Some(orch_size) = app.orchestrator_desired_size {
            if orch_size != last_orch_sent && orch_size.0 > 0 && orch_size.1 > 0 {
                match pending_orch {
                    Some((p, _)) if p == orch_size => {}
                    _ => pending_orch = Some((orch_size, Instant::now())),
                }
            } else {
                pending_orch = None;
            }
        }
        if let Some((size, at)) = pending_orch {
            if at.elapsed() >= resize_debounce {
                if let Some(orch_id) = app.orchestrator_id.clone() {
                    let _ = app.client.pty_resize(&orch_id, size.0, size.1).await;
                }
                last_orch_sent = size;
                pending_orch = None;
            }
        }
        tokio::select! {
            hydrated = async {
                match hydration_task.as_mut() {
                    Some(task) => task.await,
                    None => futures::future::pending().await,
                }
            }, if hydration_task.is_some() => {
                hydration_task = None;
                hydration_session = None;
                match hydrated {
                    Ok(Ok(h)) => app.apply_session_hydration(h).await,
                    Ok(Err(e)) => app.set_status(format!("load transcript: {e}")),
                    Err(e) if e.is_cancelled() => {}
                    Err(e) => app.set_status(format!("load transcript task failed: {e}")),
                }
            }
            ev = input_stream.next() => {
                match ev {
                    Some(Ok(ev)) => {
                        // Only enter the drag-coalesce drain when the
                        // event we just handled was itself a left-drag.
                        // Polling `input_stream.next().now_or_never()`
                        // poisons crossterm's `EventStream` wake task
                        // with a noop waker — subsequent real events
                        // call `.wake()` on the noop and never notify
                        // the main `select!`, so input sits in the
                        // buffer until something else (typically the
                        // 120 ms `tick`) wakes the loop. Gating on
                        // "was the last event a drag" keeps typing
                        // off that code path entirely; a sustained
                        // drag still coalesces because every drag
                        // event re-enters the drain.
                        let was_drag = should_drain_after(&ev);
                        app.on_term_event(ev).await;
                        if was_drag {
                            const MAX_DRAG_DRAIN: usize = 64;
                            let mut drained = 0;
                            while drained < MAX_DRAG_DRAIN {
                                match input_stream.next().now_or_never() {
                                    Some(Some(Ok(CtEvent::Mouse(m))))
                                        if matches!(
                                            m.kind,
                                            MouseEventKind::Drag(
                                                crossterm::event::MouseButton::Left
                                            )
                                        ) =>
                                    {
                                        app.on_term_event(CtEvent::Mouse(m)).await;
                                        drained += 1;
                                    }
                                    Some(Some(Ok(other_ev))) => {
                                        // Non-drag event surfaced —
                                        // handle it (so we don't drop
                                        // input) and stop draining so
                                        // it can render.
                                        app.on_term_event(other_ev).await;
                                        break;
                                    }
                                    Some(Some(Err(e))) => {
                                        app.set_status(format!("input error: {e}"));
                                        break;
                                    }
                                    // Stream ended OR no event ready.
                                    Some(None) | None => break,
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        app.set_status(format!("input error: {e}"));
                    }
                    None => break,
                }
            }
            notif = notifications.recv() => {
                match notif {
                    Some(n) => {
                        app.on_notification(n).await;
                        // Drain any additional pending notifications
                        // before looping back to the per-iteration
                        // `terminal.draw`. A burst of PtyChunks
                        // (codex's SIGWINCH redraw fragments across
                        // PTY reads, so a single redraw arrives as
                        // 4-10+ events) would otherwise produce one
                        // render per chunk — the user sees that as
                        // a "history replay" cascade animating
                        // frame-by-frame. Coalescing the burst into
                        // a single render renders only the final
                        // settled state. Capped to keep input + tick
                        // arms responsive under sustained load.
                        const MAX_DRAIN: usize = 256;
                        let mut drained = 0;
                        while drained < MAX_DRAIN {
                            match notifications.try_recv() {
                                Ok(n) => {
                                    app.on_notification(n).await;
                                    drained += 1;
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    None => {
                        if app.connected {
                            app.connected = false;
                            reconnect = Some(ReconnectState::new(Instant::now()));
                            app.set_status(
                                "daemon disconnected — reconnecting… (press q to quit)".to_string(),
                            );
                        }
                    }
                }
            }
            _ = tick.tick() => {
                if let Some((_, at)) = &app.status {
                    if at.elapsed() > Duration::from_secs(5) {
                        app.status = None;
                    }
                }
            }
        }
    }
    Ok(())
}

impl App {
    async fn reconnect(
        &mut self,
        socket: &std::path::Path,
    ) -> Result<mpsc::UnboundedReceiver<Notification>> {
        let client = Client::connect(socket).await?;
        client.subscribe(None).await?;
        let notifications = client
            .take_notifications()
            .await
            .context("notifications channel already taken")?;
        let sessions = client.list().await.unwrap_or_default();
        let groups = client.list_groups().await.unwrap_or_default();
        let harnesses = client.harnesses().await.unwrap_or_default();
        let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());

        self.client = client;
        self.pty_input_tx = pty_input_tx;
        self.pty_input_errors = pty_input_errors;
        self.sessions = sessions;
        self.groups = groups;
        self.harnesses = harnesses;
        self.connected = true;
        self.ensure_selection_valid();
        self.orchestrator_id = self
            .sessions
            .iter()
            .find(|s| {
                s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal()
            })
            .map(|s| s.id.clone());
        self.transcript_session = None;
        self.refresh_selected_transcript().await;
        self.ensure_pinned_parsers().await;
        self.set_status("reconnected to daemon".to_string());
        Ok(notifications)
    }

    pub fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    fn queue_pty_input(&mut self, session_id: String, bytes: Vec<u8>, label: &'static str) {
        if self
            .pty_input_tx
            .send(PtyInputJob {
                session_id,
                bytes,
                label,
            })
            .is_err()
        {
            self.set_status(format!("{label} failed: input pump stopped"));
        }
    }

    pub fn start_session_transition(&mut self) {
        self.session_transition = Some(SessionTransition {
            started_at: Instant::now(),
        });
    }

    pub fn start_pin_transition(&mut self, session_id: impl Into<String>) {
        self.pin_transitions
            .insert(session_id.into(), Instant::now());
    }

    pub fn select_session(&mut self, id: String) {
        if self.selection.session_id() != Some(id.as_str()) {
            self.start_session_transition();
        }
        self.selection = Selection::Session(id);
        self.transcript.clear();
        self.transcript_session = None;
        self.transcript_scroll = u16::MAX;
        self.view_scrollback = 0;
        self.view = if self.selected_session().map(|s| s.has_pty).unwrap_or(false) {
            ViewMode::Terminal
        } else {
            ViewMode::Transcript
        };
    }

    pub fn select_group(&mut self, id: String) {
        if self.selection.group_id() != Some(id.as_str()) {
            self.start_session_transition();
        }
        self.selection = Selection::Group(id);
        self.transcript.clear();
        self.transcript_session = None;
        self.transcript_scroll = u16::MAX;
        self.view_scrollback = 0;
    }

    pub fn prune_finished_transitions(&mut self) {
        let done = |started: Instant| started.elapsed().as_millis() >= SESSION_TRANSITION_MS;
        if self
            .session_transition
            .as_ref()
            .map(|t| done(t.started_at))
            .unwrap_or(false)
        {
            self.session_transition = None;
        }
        self.pin_transitions.retain(|_, started| !done(*started));
    }

    pub fn selected_session(&self) -> Option<&SessionSummary> {
        let id = self.selection.session_id()?;
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn selected_group(&self) -> Option<&GroupSummary> {
        let id = self.selection.group_id()?;
        self.groups.iter().find(|g| g.id == id)
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
    }

    fn selected_needs_hydration(&self) -> bool {
        let Some(id) = self.selection.session_id() else {
            return false;
        };
        // New selection: transcript not yet loaded for this session.
        if self.transcript_session.as_deref() != Some(id) {
            return true;
        }
        // Already transcript-hydrated, but the Terminal view has no PTY
        // history to render. The entry can go missing *after* a switch —
        // e.g. a `SessionEvent::Reset` removed it, or `has_pty` was
        // momentarily false (adapter reconnecting) when the original
        // hydration request was built, so `needs_history` came back
        // false and nothing was fetched. Without this re-trigger the
        // view stays stuck on "(no PTY history yet)" until a live PTY
        // event (e.g. the user presses a key) recreates the entry.
        self.view == ViewMode::Terminal && !self.histories.contains_key(id)
    }

    fn selected_hydration_request(&self) -> Option<SessionHydrationRequest> {
        let id = self.selection.session_id()?.to_string();
        // Fetch the PTY snapshot whenever the Terminal view lacks history
        // for this session. Driven by the view rather than `has_pty` so a
        // reconnecting adapter (transiently `has_pty == false`) can't
        // leave us without history — and so this stays consistent with
        // `selected_needs_hydration`, which guarantees `needs_history`
        // ends up true here (the fetch always inserts an entry, so the
        // re-trigger can't spin).
        let needs_history = self.view == ViewMode::Terminal && !self.histories.contains_key(&id);
        Some(SessionHydrationRequest {
            socket: self.client.socket_path().to_path_buf(),
            session_id: id,
            needs_history,
            terminal_pane_size: self.terminal_pane_size,
        })
    }

    async fn apply_session_hydration(&mut self, hydration: SessionHydration) {
        if self.selection.session_id() != Some(hydration.session_id.as_str()) {
            return;
        }

        self.transcript = hydration.transcript;
        self.transcript_session = Some(hydration.session_id.clone());
        self.transcript_scroll = u16::MAX;

        if let Some(history) = hydration.history {
            self.histories.insert(hydration.session_id.clone(), history);
            let (cols, rows) = self.terminal_pane_size;
            let _ = self
                .client
                .pty_resize(&hydration.session_id, cols, rows)
                .await;
        }
        if let Some(state) = hydration.editor_state {
            self.editor_states
                .insert(hydration.session_id.clone(), state);
        }
        if let Some(status) = hydration.agent_status {
            self.agent_statuses
                .insert(hydration.session_id.clone(), status);
        }
        if let Some(msg) = hydration.status_messages.last() {
            self.set_status(msg.clone());
        }
        if self.selection.session_id() == Some(hydration.session_id.as_str()) {
            self.start_session_transition();
        }
    }

    /// Materialize the rendered list: ungrouped sessions (sorted by
    /// position) on top, then groups in position order with each group's
    /// members indented underneath (skipped entirely when the group is
    /// collapsed).
    pub fn list_items(&self) -> Vec<ListItem> {
        let mut out: Vec<ListItem> = Vec::new();

        let orch_id = self.orchestrator_id.as_deref();
        let mut ungrouped: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| s.group_id.is_none())
            // Hide the orchestrator from the list — it's rendered in
            // the minibuffer instead. Subagents are implementation
            // details of their parent Zarvis task surface.
            .filter(|s| Some(s.id.as_str()) != orch_id)
            .filter(|s| is_list_visible_session(s))
            .collect();
        ungrouped.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        for s in ungrouped {
            out.push(ListItem::Session {
                summary: s.clone(),
                indented: false,
            });
        }

        let mut groups: Vec<&GroupSummary> = self.groups.iter().collect();
        groups.sort_by_key(|g| g.position);
        for g in groups {
            let mut members: Vec<&SessionSummary> = self
                .sessions
                .iter()
                .filter(|s| s.group_id.as_deref() == Some(g.id.as_str()))
                .filter(|s| is_list_visible_session(s))
                .collect();
            members.sort_by_key(|s| s.position);
            out.push(ListItem::GroupHeader {
                group: g.clone(),
                member_count: members.len(),
            });
            if !g.collapsed {
                for s in members {
                    out.push(ListItem::Session {
                        summary: s.clone(),
                        indented: true,
                    });
                }
            }
        }
        out
    }

    /// Find the index of the currently-selected item in the materialized
    /// list. Returns `None` if there is no selection or the item went away.
    pub fn selected_list_index(&self) -> Option<usize> {
        let items = self.list_items();
        items.iter().position(|it| it.matches(&self.selection))
    }

    async fn refresh_selected_transcript(&mut self) {
        let Some(id) = self.selected_id() else {
            self.transcript.clear();
            self.transcript_session = None;
            return;
        };
        if self.transcript_session.as_deref() == Some(&id) {
            return;
        }
        // Switching sessions snaps to live for the new one.
        self.view_scrollback = 0;
        match self.client.transcript(&id, 0, None).await {
            Ok(t) => {
                self.transcript = t.events;
                self.transcript_session = Some(id.clone());
                self.transcript_scroll = u16::MAX; // sentinel = bottom
            }
            Err(e) => {
                self.set_status(format!("load transcript: {e}"));
            }
        }
        // If this session has a PTY, prefer the live terminal view and
        // bootstrap the local emulator from the daemon's replay snapshot.
        if self.in_pty_session() {
            self.view = ViewMode::Terminal;
            self.bootstrap_terminal(&id).await;
        } else {
            self.view = ViewMode::Transcript;
        }
        if self.selection.session_id() == Some(id.as_str()) {
            self.start_session_transition();
        }
    }

    /// Bootstrap a vt100 parser for every pinned PTY-backed session that
    /// doesn't have one yet. Called at startup and whenever a session is
    /// freshly pinned (so the pin strip never shows a blank tile for a
    /// session that has had output).
    pub async fn ensure_pinned_parsers(&mut self) {
        let mut ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| s.pinned && s.has_pty && !self.histories.contains_key(&s.id))
            .map(|s| s.id.clone())
            .collect();
        // The orchestrator session is always rendered (in the
        // minibuffer panel) but never appears in `list_items` and
        // isn't pinnable — so bootstrap its history alongside the
        // pinned ones so the panel has the daemon's `pty_log`
        // backfill on TUI launch instead of starting empty until
        // the next event.
        if let Some(orch_id) = self.orchestrator_id.clone() {
            if !self.histories.contains_key(&orch_id) {
                ids.push(orch_id);
            }
        }
        for id in ids {
            self.bootstrap_terminal(&id).await;
        }
    }

    async fn bootstrap_terminal(&mut self, id: &str) {
        if self.histories.contains_key(id) {
            return;
        }
        let mut history = crate::pty_render::ItemHistory::new();
        match self.client.pty_replay(id).await {
            Ok(snap) => {
                // Size the shadow parser to match the PTY the
                // daemon last knew about (falls back to the
                // current pane size) BEFORE feeding rehydrated
                // bytes. Codex / claude / any normal-screen TUI
                // emits cursor positioning that depends on
                // terminal dims — replaying those bytes against
                // the shadow's default 80×24 leaves scrollback
                // showing clamped, incoherent fragments.
                let (cols, rows) = snap
                    .size
                    .as_ref()
                    .map(|s| (s.cols, s.rows))
                    .unwrap_or(self.terminal_pane_size);
                history.set_pty_size(cols, rows);
                use base64::Engine;
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&snap.data) {
                    history.feed_pty(&bytes);
                }
            }
            Err(e) => {
                self.set_status(format!("pty_replay: {e}"));
            }
        }
        // Rehydrate ToolBlocks. `feed_pty` parses the OSC fences in
        // pty.log and creates empty `ToolBlock` items, but the
        // structured `tool` / `args` / `output` fields live in
        // transcript events — which the live path routes via
        // `feed_tool_use` / `feed_tool_result` but the daemon does
        // NOT re-broadcast on subscribe. So after a daemon restart
        // the blocks would render as `→ ?` with no body and the
        // dim-styled args + footer the user sees during a live
        // session disappear. Replay the transcript here in the
        // SAME ORDER `feed_pty` saw the fences (FIFO) so
        // pending-hydration pairing in `ItemHistory` reattaches
        // tools to their blocks; `feed_tool_result` matches by
        // call_id and just fills `output` on the existing block.
        let mut replayed_editor_state: Option<EditorState> = None;
        let mut replayed_agent_status: Option<agentd_protocol::AgentStatus> = None;
        match self.client.transcript(id, 0, None).await {
            Ok(t) => {
                if t.events
                    .iter()
                    .any(|ev| matches!(ev.event, SessionEvent::Pty { .. }))
                {
                    // New daemons persist PTY events in the transcript as ordering
                    // markers. Prefer rebuilding from those markers so transcript-only
                    // items (zarvis tool blocks) are interleaved with the raw bytes in
                    // chronological order. The pty_replay path above remains the
                    // fallback for older sessions whose transcripts do not contain PTY.
                    history.clear_items();
                }
                apply_transcript_to_local_state(
                    &t.events,
                    &mut history,
                    &mut replayed_editor_state,
                    &mut replayed_agent_status,
                );
            }
            Err(e) => {
                self.set_status(format!("rehydrate transcript: {e}"));
            }
        }
        if let Some(state) = replayed_editor_state {
            self.editor_states.insert(id.to_string(), state);
        }
        if let Some(status) = replayed_agent_status {
            self.agent_statuses.insert(id.to_string(), status);
        }
        self.histories.insert(id.to_string(), history);
        // Tell the daemon what size we'd like.
        let (cols, rows) = self.terminal_pane_size;
        let _ = self.client.pty_resize(id, cols, rows).await;
    }

    async fn toggle_pin_on_selection(&mut self) {
        match self.selection.clone() {
            Selection::Session(id) => {
                let s = match self.sessions.iter().find(|s| s.id == id) {
                    Some(s) => s.clone(),
                    None => return,
                };
                let want = !s.pinned;
                if let Err(e) = self.client.set_pinned(&id, want).await {
                    self.set_status(format!("set_pinned failed: {e}"));
                    return;
                }
                if let Some(i) = self.sessions.iter().position(|x| x.id == id) {
                    self.sessions[i].pinned = want;
                }
                if want && s.has_pty {
                    self.start_pin_transition(id.clone());
                    self.bootstrap_terminal(&id).await;
                }
                self.set_status(if want { "pinned" } else { "unpinned" }.into());
            }
            Selection::Group(group_id) => {
                let members: Vec<SessionSummary> = self
                    .sessions
                    .iter()
                    .filter(|s| s.group_id.as_deref() == Some(group_id.as_str()))
                    .cloned()
                    .collect();
                if members.is_empty() {
                    self.set_status("group has no members".into());
                    return;
                }
                let all_pinned = members.iter().all(|s| s.pinned);
                let want = !all_pinned;
                for s in &members {
                    if s.pinned == want {
                        continue;
                    }
                    if let Err(e) = self.client.set_pinned(&s.id, want).await {
                        self.set_status(format!("set_pinned {}: {}", short_id(&s.id), e));
                    }
                }
                self.set_status(if want {
                    format!("pinned {} member(s)", members.len())
                } else {
                    format!("unpinned {} member(s)", members.len())
                });
            }
            Selection::None => {
                self.set_status("nothing selected".into());
            }
        }
    }

    async fn move_selected(&mut self, up: bool) {
        let dir = if up {
            agentd_protocol::MoveDirection::Up
        } else {
            agentd_protocol::MoveDirection::Down
        };
        match self.selection.clone() {
            Selection::Session(id) => {
                if let Err(e) = self.client.move_session(&id, dir).await {
                    self.set_status(format!("move failed: {e}"));
                }
                // Daemon broadcasts will reconcile positions/groups.
            }
            Selection::Group(id) => {
                if let Err(e) = self.client.move_group(&id, dir).await {
                    self.set_status(format!("move failed: {e}"));
                }
            }
            Selection::None => self.set_status("nothing selected".into()),
        }
    }

    async fn refresh_sessions(&mut self) {
        match self.client.list().await {
            Ok(list) => self.sessions = list,
            Err(e) => self.set_status(format!("list failed: {e}")),
        }
        match self.client.list_groups().await {
            Ok(list) => self.groups = list,
            Err(e) => self.set_status(format!("group list failed: {e}")),
        }
        self.refresh_orchestrator_id();
        self.ensure_selection_valid();
    }

    /// Re-derive `orchestrator_id` from the current sessions list.
    /// Called after any list mutation (refresh, state notification,
    /// session-deleted) so the minibuffer stays bound to the right
    /// session — and falls back to palette mode if the orchestrator
    /// goes away.
    ///
    /// Prefers a *live* (non-terminal) orchestrator. If only terminal
    /// orchestrators exist (e.g. a previous run failed to start
    /// zarvis), we behave as if there's no orchestrator so the user
    /// gets the palette fallback.
    fn refresh_orchestrator_id(&mut self) {
        self.orchestrator_id = self
            .sessions
            .iter()
            .find(|s| {
                s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal()
            })
            .map(|s| s.id.clone());
    }

    /// Move the selection up or down by one row in the materialized list,
    /// wrapping at the ends. No-op if the list is empty.
    async fn step_selection(&mut self, delta: i32) {
        let items = self.list_items();
        if items.is_empty() {
            return;
        }
        let cur = items
            .iter()
            .position(|it| it.matches(&self.selection))
            .unwrap_or(0);
        let n = items.len() as i32;
        let next = ((cur as i32 + delta).rem_euclid(n)) as usize;
        match &items[next] {
            ListItem::Session { summary, .. } => self.select_session(summary.id.clone()),
            ListItem::GroupHeader { group, .. } => self.select_group(group.id.clone()),
        }
    }

    /// After any list mutation, make sure `self.selection` still refers to
    /// an item we know about. Fall back to the first list item if not.
    fn ensure_selection_valid(&mut self) {
        let items = self.list_items();
        if items.iter().any(|it| it.matches(&self.selection)) {
            return;
        }
        self.selection = match items.first() {
            Some(ListItem::Session { summary, .. }) => Selection::Session(summary.id.clone()),
            Some(ListItem::GroupHeader { group, .. }) => Selection::Group(group.id.clone()),
            None => Selection::None,
        };
    }

    async fn on_notification(&mut self, n: agentd_protocol::Notification) {
        match n.method.as_str() {
            m if m == agentd_protocol::ipc_notif::EVENT => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<EventNotificationPayload>(p) {
                        self.matrix_rain
                            .observe_event(&payload.event, self.matrix_rain_intensity);
                        // Tool-approval prompt: if no minibuffer is in use,
                        // open the approval prompt for the matching session.
                        // Otherwise the user sees the request in the
                        // transcript and can resume via `C-x .` (future).
                        if let SessionEvent::ToolApprovalRequest {
                            call_id,
                            tool,
                            args_summary,
                            risk,
                        } = &payload.event
                        {
                            self.maybe_open_approval_prompt(
                                payload.session_id.clone(),
                                call_id.clone(),
                                tool.clone(),
                                args_summary.clone(),
                                *risk,
                            );
                            // Also fall through so the transcript records it.
                        }
                        if matches!(payload.event, SessionEvent::Reset) {
                            self.histories.remove(&payload.session_id);
                            self.block_hits.remove(&payload.session_id);
                            self.editor_states.remove(&payload.session_id);
                            self.agent_statuses.remove(&payload.session_id);
                            self.pty_activity.remove(&payload.session_id);
                            self.matrix_rain.forget_session(&payload.session_id);
                            if Some(payload.session_id.as_str())
                                == self.transcript_session.as_deref()
                            {
                                self.transcript.clear();
                                self.transcript_scroll = u16::MAX;
                            }
                            return;
                        }
                        // TUI-dispatch tool calls: any session can emit
                        // a ToolUse with the conventional `tui` tool
                        // name to fire a slash-command-style action in
                        // the client. Args shape:
                        //   {"command": "<verb>", "args": "<rest>"}
                        if let SessionEvent::ToolUse { tool, args } = &payload.event {
                            if tool == agentd_protocol::TUI_DISPATCH_TOOL {
                                let cmd =
                                    args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                                let arg_str =
                                    args.get("args").and_then(|v| v.as_str()).unwrap_or("");
                                let full = if arg_str.is_empty() {
                                    cmd.to_string()
                                } else {
                                    format!("{cmd} {arg_str}")
                                };
                                if !full.is_empty() {
                                    self.run_slash_command(&full).await;
                                }
                                // Fall through so the transcript still
                                // records the call for forensics.
                            }
                        }
                        // PTY events: feed into the per-session items history.
                        if let SessionEvent::Pty { .. } = &payload.event {
                            let now = Instant::now();
                            let bytes = payload.event.pty_bytes();
                            if let Some(b) = bytes.as_deref() {
                                let history = self
                                    .histories
                                    .entry(payload.session_id.clone())
                                    .or_default();
                                history.feed_pty(b);
                            }
                            // Mark the session as freshly active for the spinner.
                            self.pty_activity.insert(payload.session_id.clone(), now);
                            // PTY-only harnesses (codex/claude in interactive
                            // mode, shell) don't emit structured ToolUse/Status
                            // events while working, so feed the matrix-rain
                            // the byte stream too. It harvests recent words
                            // and reveals them on a per-session throttle, so
                            // the rain reflects what the harness is actually
                            // printing instead of cycling a hard-coded list.
                            self.matrix_rain.observe_pty_activity(
                                &payload.session_id,
                                bytes.as_deref().unwrap_or(&[]),
                                now,
                                self.matrix_rain_intensity,
                            );
                            return;
                        }
                        // Tool events feed the same history so the
                        // items-model renderer can synthesize block
                        // visuals from structured content. The
                        // adapter writes OSC fences around each tool
                        // block in the PTY stream; the history pairs
                        // ToolUse events to those fences by FIFO
                        // arrival order, and matches ToolResults by
                        // call_id (carried in the `tool` field by
                        // zarvis convention). Tool events from the
                        // orchestrator session also land here.
                        if let SessionEvent::ToolUse { tool, args } = &payload.event {
                            // The TUI-dispatch tool (`tui`) is a
                            // slash-command short-circuit, not a real
                            // tool — skip the items-history feed
                            // (it's handled by `run_slash_command`).
                            if tool != agentd_protocol::TUI_DISPATCH_TOOL {
                                let history = self
                                    .histories
                                    .entry(payload.session_id.clone())
                                    .or_default();
                                history.feed_tool_use(tool.clone(), summarize_tool_args(args));
                            }
                        }
                        // TaskStart is the primary block-creation
                        // event for the items model — carries an
                        // explicit call_id so the block can be
                        // hydrated immediately (no FIFO pairing
                        // required, no OSC fence needed in the PTY
                        // stream).
                        if let SessionEvent::TaskStart {
                            call_id,
                            tool,
                            args_summary,
                        } = &payload.event
                        {
                            let history = self
                                .histories
                                .entry(payload.session_id.clone())
                                .or_default();
                            history.feed_task_start(
                                call_id.clone(),
                                tool.clone(),
                                args_summary.clone(),
                            );
                        }
                        if let SessionEvent::ToolResult { tool, ok, output } = &payload.event {
                            let history = self
                                .histories
                                .entry(payload.session_id.clone())
                                .or_default();
                            history.feed_tool_result(tool, *ok, output.clone());
                        }
                        // Adapter editor state — drives the fixed
                        // bottom input pane.
                        if let SessionEvent::EditorState {
                            queued,
                            buf,
                            cursor,
                            completions,
                        } = &payload.event
                        {
                            self.editor_states.insert(
                                payload.session_id.clone(),
                                EditorState {
                                    queued: queued.clone(),
                                    buf: buf.clone(),
                                    cursor: *cursor,
                                    completions: completions.clone(),
                                },
                            );
                        }
                        if let SessionEvent::AgentStatus(status) = &payload.event {
                            if status.active {
                                self.agent_statuses
                                    .insert(payload.session_id.clone(), status.clone());
                            } else {
                                self.agent_statuses.remove(&payload.session_id);
                                if let Some(bytes) = agent_status_history_line(status) {
                                    let history = self
                                        .histories
                                        .entry(payload.session_id.clone())
                                        .or_default();
                                    history.feed_pty(&bytes);
                                }
                            }
                            return;
                        }
                        // Orchestrator session events: PTY bytes flow
                        // through the regular PTY branch above (into
                        // `terminals[id]`). Non-PTY events (Message,
                        // ToolUse, ToolResult, ...) just record into
                        // the transcript like any other session — the
                        // orchestrator is filtered from the *list*
                        // view, but its events are still useful for
                        // CLI / MCP introspection and don't hurt the
                        // TUI (the panel renders the PTY screen, not
                        // the structured events).
                        if Some(payload.session_id.as_str()) == self.transcript_session.as_deref() {
                            self.transcript.push(TimestampedEvent {
                                seq: payload.seq,
                                at: payload.at,
                                event: payload.event.clone(),
                            });
                            self.transcript_scroll = u16::MAX;
                        }
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<StateNotificationPayload>(p) {
                        let id = payload.session.id.clone();
                        let was_pinned = self
                            .sessions
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.pinned)
                            .unwrap_or(false);
                        let now_pinned = payload.session.pinned;
                        let has_pty = payload.session.has_pty;
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.sessions[i] = payload.session;
                        } else {
                            self.sessions.push(payload.session);
                            self.sessions
                                .sort_by(|a, b| b.created_at.cmp(&a.created_at));
                        }
                        self.refresh_orchestrator_id();
                        // Newly pinned PTY session: bootstrap so its tile
                        // populates immediately, even when the pin came from
                        // outside this TUI process.
                        if has_pty && now_pinned && !was_pinned {
                            self.start_pin_transition(id.clone());
                            self.bootstrap_terminal(&id).await;
                        }
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::DELETED => {
                if let Some(p) = n.params {
                    if let Ok(payload) =
                        serde_json::from_value::<agentd_protocol::DeletedNotificationPayload>(p)
                    {
                        self.on_session_deleted(&payload.session_id).await;
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::GROUP_STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) =
                        serde_json::from_value::<agentd_protocol::GroupStateNotificationPayload>(p)
                    {
                        self.on_group_state(payload.group).await;
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::GROUP_DELETED => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::GroupDeletedNotificationPayload,
                    >(p)
                    {
                        self.on_group_deleted(&payload.group_id).await;
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::REMOTE_STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) =
                        serde_json::from_value::<agentd_protocol::RemoteStateNotificationPayload>(p)
                    {
                        self.remote_clients = payload.clients;
                    }
                }
            }
            _ => {}
        }
    }

    /// Open the approval prompt if there's no other minibuffer in flight.
    /// Best-effort: if the user is already typing something, we skip and
    /// leave the request visible in the transcript only.
    fn maybe_open_approval_prompt(
        &mut self,
        session_id: String,
        call_id: String,
        tool: String,
        args_summary: String,
        risk: agentd_protocol::ToolRisk,
    ) {
        // Orchestrator approvals are rendered inline in zarvis's PTY
        // (the `? approve [risk] tool(args) — y/n/a` row). The user
        // responds with a single key inside the orchestrator panel,
        // not via a separate minibuffer prompt — so skip ours.
        if self.orchestrator_id.as_deref() == Some(session_id.as_str()) {
            return;
        }
        // Otherwise: any non-orchestrator minibuffer is shorter-lived
        // and shouldn't be clobbered by an unrelated approval. Skip
        // when busy.
        if self.minibuffer.is_some() {
            return;
        }
        let risk_label = match risk {
            agentd_protocol::ToolRisk::Safe => "safe",
            agentd_protocol::ToolRisk::Risky => "risky",
        };
        let short_args: String = args_summary.chars().take(80).collect();
        let prompt = format!(
            "approve [{risk_label}] {tool}({}) ▸ y=approve  n=deny  a=automode",
            short_args
        );
        self.minibuffer = Some(Minibuffer {
            prompt,
            input: String::new(),
            cursor: 0,
            intent: MinibufferIntent::ApproveTool {
                session_id,
                call_id,
                tool,
                args_summary,
                risk,
            },
            error: None,
        });
    }

    /// Toggle the selected session's automode flag.
    pub async fn toggle_automode(&mut self) {
        let Some(s) = self.selected_session() else {
            self.set_status("no session selected".into());
            return;
        };
        let id = s.id.clone();
        let next = !s.automode;
        match self.client.set_automode(&id, next).await {
            Ok(()) => self.set_status(format!("automode {}", if next { "ON" } else { "off" })),
            Err(e) => self.set_status(format!("set_automode failed: {e}")),
        }
    }

    async fn on_session_deleted(&mut self, id: &str) {
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions.remove(i);
        }
        if self.transcript_session.as_deref() == Some(id) {
            self.transcript.clear();
            self.transcript_session = None;
        }
        self.histories.remove(id);
        self.block_hits.remove(id);
        self.pty_activity.remove(id);
        self.matrix_rain.forget_session(id);
        // Orchestrator session went away → palette fallback after the
        // re-derive below. The orchestrator's PTY parser in
        // `terminals[id]` was already removed by the generic cleanup
        // above.
        self.refresh_orchestrator_id();
        self.ensure_selection_valid();
        self.refresh_selected_transcript().await;
    }

    /// Is this session "busy" right now — i.e. has it produced PTY bytes
    /// recently enough that we should render a spinner instead of a static
    /// dot? Falls back to the daemon-reported `last_pty_at_ms` so a freshly
    /// connected client doesn't misread an ongoing turn as idle.
    pub fn pty_active(&self, session_id: &str) -> bool {
        if let Some(t) = self.pty_activity.get(session_id) {
            if t.elapsed() < PTY_QUIESCENCE {
                return true;
            }
        }
        if let Some(s) = self.sessions.iter().find(|s| s.id == session_id) {
            if let Some(ms) = s.last_pty_at_ms {
                let now_ms = chrono::Utc::now().timestamp_millis();
                if now_ms - ms < PTY_QUIESCENCE.as_millis() as i64 {
                    return true;
                }
            }
        }
        false
    }

    /// Current spinner frame, ticking on wall-time so all sessions animate
    /// in phase.
    pub fn spinner_frame(&self) -> &'static str {
        let idx = (self.start_instant.elapsed().as_millis() / SPINNER_FRAME_MS) as usize
            % SPINNER_FRAMES.len();
        SPINNER_FRAMES[idx]
    }

    async fn on_group_state(&mut self, g: GroupSummary) {
        if let Some(i) = self.groups.iter().position(|x| x.id == g.id) {
            self.groups[i] = g;
        } else {
            self.groups.push(g);
            self.groups.sort_by_key(|g| g.position);
        }
        self.ensure_selection_valid();
    }

    async fn on_group_deleted(&mut self, id: &str) {
        self.groups.retain(|g| g.id != id);
        self.ensure_selection_valid();
    }

    async fn on_term_event(&mut self, ev: CtEvent) {
        match ev {
            CtEvent::Key(k) => self.on_key(k).await,
            CtEvent::Mouse(m) => self.on_mouse(m).await,
            CtEvent::Resize(_, _) => {
                // The TUI re-derives the pane size on next render; we trigger
                // an explicit resize for the current PTY there.
            }
            _ => {}
        }
    }

    async fn on_mouse(&mut self, ev: MouseEvent) {
        if !self.mouse_capture_enabled {
            return;
        }
        use crossterm::event::MouseButton;
        const STEP: i32 = 3;
        // Track every event's cell so hover-aware rendering (diamond
        // tooltip, etc.) has a current position to render against.
        self.mouse_pos = Some((ev.column, ev.row));
        match ev.kind {
            MouseEventKind::ScrollUp => {
                if !self.adjust_mouse_list_scroll(ev.column, ev.row, -STEP) {
                    self.adjust_mouse_scrollback(ev.column, ev.row, STEP);
                }
            }
            MouseEventKind::ScrollDown => {
                if !self.adjust_mouse_list_scroll(ev.column, ev.row, STEP) {
                    self.adjust_mouse_scrollback(ev.column, ev.row, -STEP);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // List ↔ view divider: clicking the list pane's
                // right border (col = list_width - 1), the view's
                // left border (col = list_width), or the first pin
                // tile's left border (col = list_width) starts a
                // resize-drag rather than the usual click-through
                // flow. Only meaningful in the normal split layout
                // (zoomed modes don't show the border).
                if self.is_on_list_divider(ev.column, ev.row) {
                    self.resizing_list = Some((ev.column, self.list_panel_w));
                    return;
                }
                // View ↔ pin-strip horizontal divider: clicking the
                // view's bottom border (or equivalently the pin
                // strip's top border, the same row) starts a
                // vertical resize-drag for the pin strip.
                if self.is_on_pin_strip_divider(ev.column, ev.row) {
                    let cur_h = self.layout.pin_strip_area.map(|s| s.height).unwrap_or(0);
                    self.resizing_pin_strip = Some((ev.row, cur_h));
                    return;
                }
                // God/minibuffer panel: the top border is the panel's title
                // area and acts as a vertical resize handle.
                if self.is_on_orchestrator_panel_divider(ev.column, ev.row) {
                    let cur_h = self.layout.minibuffer_area.map(|a| a.height).unwrap_or(
                        self.orchestrator_panel_h
                            .unwrap_or(MINIBUFFER_PANEL_H_DEFAULT),
                    );
                    self.resizing_orchestrator_panel = Some((ev.row, cur_h));
                    return;
                }
                // Matrix-rain panel: the title bar doubles as a height
                // handle. The panel is bottom-anchored, so dragging the top
                // edge upward grows it and dragging downward shrinks it.
                if self.is_on_matrix_rain_title_bar(ev.column, ev.row) {
                    let cur_h = self
                        .layout
                        .matrix_rain_area
                        .map(|a| a.height)
                        .unwrap_or(MATRIX_RAIN_H_DEFAULT);
                    self.resizing_matrix_rain = Some((ev.row, cur_h));
                    return;
                }
                self.selected_text = None;
                self.selected_text_bounds = None;
                self.selected_text_range = None;
                self.text_selection = Some(TextSelection {
                    anchor: ScreenPoint {
                        col: ev.column,
                        row: ev.row,
                    },
                    head: ScreenPoint {
                        col: ev.column,
                        row: ev.row,
                    },
                    dragged: false,
                    bounds: self.selection_bounds_at(ev.column, ev.row),
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((anchor_col, anchor_w)) = self.resizing_list {
                    // Apply the column delta to the width that was
                    // current at drag start. Works for both grab
                    // points: list's right border (col = w−1) and
                    // view/pin's left border (col = w) — the delta
                    // is what matters, not the absolute column.
                    let delta = ev.column as i32 - anchor_col as i32;
                    let want = (anchor_w as i32 + delta)
                        .max(LIST_PANEL_W_MIN as i32)
                        .min(u16::MAX as i32) as u16;
                    self.list_panel_w = want;
                } else if let Some((anchor_row, anchor_h)) = self.resizing_pin_strip {
                    // Dragging the divider DOWN (row grows) shrinks
                    // the pin strip; dragging UP grows it. Negate
                    // the row delta to match cursor direction.
                    let delta = anchor_row as i32 - ev.row as i32;
                    let want = (anchor_h as i32 + delta)
                        .max(PIN_STRIP_H_MIN as i32)
                        .min(PIN_STRIP_H_MAX as i32) as u16;
                    self.pin_strip_h = Some(want);
                } else if let Some((anchor_row, anchor_h)) = self.resizing_orchestrator_panel {
                    // Dragging the top border UP grows the panel; dragging it
                    // DOWN shrinks it. The render path still clamps to the
                    // available terminal height.
                    let delta = anchor_row as i32 - ev.row as i32;
                    let want = (anchor_h as i32 + delta)
                        .max(MINIBUFFER_PANEL_H_MIN as i32)
                        .min(MINIBUFFER_PANEL_H_MAX as i32) as u16;
                    self.orchestrator_panel_h = Some(want);
                } else if let Some((anchor_row, anchor_h)) = self.resizing_matrix_rain {
                    let delta = anchor_row as i32 - ev.row as i32;
                    let raw = (anchor_h as i32 + delta).max(MATRIX_RAIN_H_MIN as i32) as u16;
                    let available = self.matrix_rain_available_height().unwrap_or(raw);
                    self.matrix_rain_h =
                        Some(crate::ui::matrix_rain_panel_height(Some(raw), available));
                } else if let Some(sel) = self.text_selection.as_mut() {
                    sel.head = ScreenPoint {
                        col: ev.column,
                        row: ev.row,
                    };
                    sel.dragged =
                        sel.dragged || sel.anchor.col != ev.column || sel.anchor.row != ev.row;
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let was_resizing = self.resizing_list.is_some()
                    || self.resizing_pin_strip.is_some()
                    || self.resizing_orchestrator_panel.is_some()
                    || self.resizing_matrix_rain.is_some();
                self.resizing_list = None;
                self.resizing_pin_strip = None;
                self.resizing_orchestrator_panel = None;
                self.resizing_matrix_rain = None;
                if was_resizing {
                    self.text_selection = None;
                    return;
                }
                if let Some(mut sel) = self.text_selection.clone() {
                    sel.head = ScreenPoint {
                        col: ev.column,
                        row: ev.row,
                    };
                    if sel.dragged {
                        let text = self.selected_frame_text(&sel);
                        match copy_to_clipboard(&text) {
                            Ok(()) => {
                                let n = text.chars().count();
                                self.selected_text = (!text.is_empty()).then_some(text);
                                self.selected_text_bounds = sel.bounds;
                                self.selected_text_range = self.selected_frame_range(&sel);
                                self.text_selection = None;
                                self.set_status(format!("copied {n} chars"));
                            }
                            Err(e) => self.set_status(format!("copy failed: {e}")),
                        }
                        return;
                    }
                }
                self.text_selection = None;
                self.handle_left_click(ev.column, ev.row).await;
            }
            _ => {}
        }
    }

    fn selected_frame_text(&self, sel: &TextSelection) -> String {
        let Some(range) = self.selected_frame_range(sel) else {
            return String::new();
        };
        let bounds = sel.bounds;
        let bound_left = bounds.map(|b| b.left()).unwrap_or(0);
        let bound_right = bounds.map(|b| b.right().saturating_sub(1));
        let mut lines = Vec::new();
        for row in range.start.row..=range.end.row {
            let Some(line) = self.frame_text.get(row as usize) else {
                continue;
            };
            let start_col = if row == range.start.row {
                range.start.col
            } else {
                bound_left
            };
            let end_col = if row == range.end.row {
                range.end.col
            } else {
                bound_right.unwrap_or_else(|| line.chars().count().saturating_sub(1) as u16)
            };
            lines.push(slice_line(line, start_col, end_col).trim_end().to_string());
        }
        lines.join("\n").trim_end().to_string()
    }

    fn selected_frame_range(&self, sel: &TextSelection) -> Option<TextSelectionRange> {
        let (start, end) = normalized_points(sel.anchor, sel.head);
        let bounds = sel.bounds;
        let row_start = bounds.map(|b| b.top()).unwrap_or(0).max(start.row);
        let row_end = bounds
            .map(|b| b.bottom().saturating_sub(1))
            .unwrap_or(u16::MAX)
            .min(end.row);
        if row_start > row_end {
            return None;
        }
        let bound_left = bounds.map(|b| b.left()).unwrap_or(0);
        let bound_right = bounds.map(|b| b.right().saturating_sub(1));
        let start_col = if row_start == start.row {
            start.col
        } else {
            bound_left
        }
        .max(bound_left);
        let end_col = if row_end == end.row {
            end.col
        } else {
            bound_right.unwrap_or(u16::MAX)
        }
        .min(bound_right.unwrap_or(u16::MAX));
        Some(TextSelectionRange {
            start: ScreenPoint {
                col: start_col,
                row: row_start,
            },
            end: ScreenPoint {
                col: end_col,
                row: row_end,
            },
        })
    }

    fn selection_bounds_at(&self, col: u16, row: u16) -> Option<ratatui::layout::Rect> {
        let pinned_count = self
            .list_items()
            .into_iter()
            .filter(|it| matches!(it, ListItem::Session { summary, .. } if summary.pinned))
            .count();
        let is_orchestrator_panel = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::Orchestrator)
        );
        selection_bounds_for_layout(&self.layout, pinned_count, is_orchestrator_panel, col, row)
    }

    /// True if `(col, row)` sits on the main view's bottom border
    /// row — the divider directly above the pin strip. The view's
    /// bottom border is at `pin_strip.y − 1` (one row above the
    /// strip's top border / title row). Only meaningful when there
    /// IS a pin strip and we're in the normal split layout.
    fn is_on_pin_strip_divider(&self, col: u16, row: u16) -> bool {
        if !matches!(self.zoom, ZoomMode::None) {
            return false;
        }
        let Some(strip) = self.layout.pin_strip_area else {
            return false;
        };
        let view_bottom = match strip.y.checked_sub(1) {
            Some(r) => r,
            None => return false,
        };
        row == view_bottom && col >= strip.x && col < strip.x + strip.width
    }

    /// True if `(col, row)` sits on the orchestrator/god panel's top border.
    /// That border is the visible horizontal title line when god is focused
    /// and is used as a vertical resize handle.
    fn is_on_orchestrator_panel_divider(&self, col: u16, row: u16) -> bool {
        if !self.is_orchestrator_panel_open() {
            return false;
        }
        let Some(area) = self.layout.minibuffer_area else {
            return false;
        };
        area.height > 1 && row == area.y && col >= area.x && col < area.x + area.width
    }

    fn is_on_matrix_rain_title_bar(&self, col: u16, row: u16) -> bool {
        if self.matrix_rain_hidden {
            return false;
        }
        let Some(rain) = self.layout.matrix_rain_area else {
            return false;
        };
        if row != rain.y || col < rain.x || col >= rain.x + rain.width {
            return false;
        }
        if let Some((xs, xe, y)) = crate::ui::matrix_rain_close_button_range(rain) {
            if row == y && col >= xs && col < xe {
                return false;
            }
        }
        true
    }

    fn matrix_rain_available_height(&self) -> Option<u16> {
        let list = self.layout.list_area?;
        let inner_h = list.height.saturating_sub(2);
        // The matrix panel is sticky and may shrink the visible item
        // window, but it's clamped so the list always keeps at least
        // SESSION_LIST_H_MIN rows when both are shown.
        Some(inner_h.saturating_sub(SESSION_LIST_H_MIN))
    }

    /// True if `(col, row)` sits on the list ↔ right-pane divider.
    /// The grab zone covers three cells side-by-side:
    ///   * `list.x + list.width − 1` — list's right border
    ///   * `view_area.x` — main session view's left border
    ///   * `pin_strip.x` — first pin tile's left border (when any
    ///     sessions are pinned)
    /// The two "left border" cells are at the same column as each
    /// other (view and pin strip stack vertically), but at row-
    /// disjoint y ranges, so each contributes to one half of the
    /// vertical span. Returns false in zoomed layouts (no borders
    /// to grab there).
    fn is_on_list_divider(&self, col: u16, row: u16) -> bool {
        if !matches!(self.zoom, ZoomMode::None) {
            return false;
        }
        let Some(list) = self.layout.list_area else {
            return false;
        };
        if list.width == 0 {
            return false;
        }
        let list_right_x = list.x + list.width - 1;
        // List's right border — the original grab handle.
        if col == list_right_x && row >= list.y && row < list.y + list.height {
            return true;
        }
        // Main view's left border (immediately right of list's
        // right border).
        if let Some(view) = self.layout.view_area {
            if col == view.x && row >= view.y && row < view.y + view.height {
                return true;
            }
        }
        // First pin tile's left border. The strip's x is the same
        // column as view.x; we just need the strip's y range.
        if let Some(strip) = self.layout.pin_strip_area {
            if col == strip.x && row >= strip.y && row < strip.y + strip.height {
                return true;
            }
        }
        false
    }

    /// Hit-test a left-click against the last frame's pane geometry.
    /// - Inside the **minibuffer**: position the cursor within the
    ///   typed input when one is open, or open the command palette
    ///   when none is.
    /// - Inside the **list pane**: select the row (or toggle group on
    ///   a header click) and focus the list.
    /// - Inside the **view pane**: focus the view.
    /// - Inside the **pin strip**: select the matching pinned session.
    async fn handle_left_click(&mut self, col: u16, row: u16) {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(modal) = self.layout.modal_area {
            if !contains(modal, col, row) {
                self.dismiss_modal();
                return;
            }
            // The current modals are informational/read-only. Clicks
            // inside them are consumed so they don't focus or activate
            // controls in panes underneath the modal.
            return;
        }
        if let Some(hit) = self.url_hit_at(col, row) {
            match open_url(&hit.url) {
                Ok(()) => self.set_status(format!("opened {}", hit.url)),
                Err(e) => self.set_status(format!("open URL failed: {e}")),
            }
            return;
        }
        if let Some(mb_area) = self.layout.minibuffer_area {
            if contains(mb_area, col, row) {
                // First check the inline-hint zones ("C-x z unzoom" /
                // "? help" / etc.). They sit on the same row as the
                // minibuffer area when no prompt is open and dispatch
                // their bound action directly instead of opening the
                // palette.
                for hint in &self.layout.minibuffer_hints {
                    if row == hint.y && col >= hint.x_start && col < hint.x_end {
                        let action = hint.action;
                        self.run_action(action).await;
                        return;
                    }
                }
                // Orchestrator panel: click on a tool block toggles
                // its expand state. The orchestrator's render area
                // is the minibuffer rect minus the 1-row top border.
                if matches!(
                    self.minibuffer.as_ref().map(|m| &m.intent),
                    Some(MinibufferIntent::Orchestrator)
                ) {
                    if let Some(orch_id) = self.orchestrator_id.clone() {
                        let inner = ratatui::layout::Rect {
                            x: mb_area.x,
                            y: mb_area.y + 1,
                            width: mb_area.width,
                            height: mb_area.height.saturating_sub(1),
                        };
                        if self.try_toggle_block_at(&orch_id, inner, col, row).await {
                            return;
                        }
                    }
                }
                self.click_minibuffer(mb_area, col).await;
                return;
            }
        }
        if let Some(strip) = self.layout.pin_strip_area {
            if contains(strip, col, row) {
                self.click_pin_strip(strip, col, row).await;
                return;
            }
        }
        if let Some(list) = self.layout.list_area {
            if contains(list, col, row) {
                self.click_list(list, col, row).await;
                return;
            }
        }
        if let Some(view) = self.layout.view_area {
            if contains(view, col, row) {
                // Uncollapse handle: when the list is collapsed,
                // the view's left border column acts as the
                // "show list" button. Tested before other view
                // click handlers so a click on column `view.x`
                // never falls through to a content click.
                if crate::ui::is_on_view_uncollapse_handle(self, col, row) {
                    self.list_collapsed = false;
                    self.focus = PaneFocus::List;
                    return;
                }
                // Top-row close button: ` x ` 3-cell range at the
                // right edge of the top border. Click → delete
                // confirmation prompt for the selected session.
                let (close_x_start, close_x_end, close_y) =
                    crate::ui::view_close_button_range(view);
                if self.selected_session().is_some()
                    && row == close_y
                    && col >= close_x_start
                    && col < close_x_end
                {
                    self.run_action(crate::keymap::KeyAction::OpenDeleteConfirm)
                        .await;
                    return;
                }
                // Inner area: same Rect minus the 1-cell border on
                // each side. The render path stores hit ranges
                // relative to the inner area's top — translate
                // before lookup.
                let inner = ratatui::layout::Rect {
                    x: view.x + 1,
                    y: view.y + 1,
                    width: view.width.saturating_sub(2),
                    height: view.height.saturating_sub(2),
                };
                if let Some(id) = self.selected_id() {
                    if self.try_toggle_block_at(&id, inner, col, row).await {
                        return;
                    }
                }
                self.collapse_orchestrator_panel_on_focus_change();
                self.focus = PaneFocus::View;
                return;
            }
        }
    }

    fn dismiss_modal(&mut self) {
        if self.tasks_popup.take().is_some() {
            return;
        }
        if self.remote_control_popup.take().is_some() {
            if matches!(
                self.minibuffer.as_ref().map(|m| &m.intent),
                Some(MinibufferIntent::Orchestrator)
            ) {
                self.minibuffer = None;
            }
            return;
        }
        self.help_visible = false;
    }

    pub fn hovered_url(&self) -> Option<UrlHit> {
        let (col, row) = self.mouse_pos?;
        self.url_hit_at(col, row)
    }

    fn url_hit_at(&self, col: u16, row: u16) -> Option<UrlHit> {
        let bounds = self.url_click_bounds(col, row)?;
        url_hit_in_frame(&self.frame_text, col, row, bounds)
    }

    fn url_click_bounds(&self, col: u16, row: u16) -> Option<ratatui::layout::Rect> {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(view) = self.layout.view_area {
            let inner = ratatui::layout::Rect {
                x: view.x.saturating_add(1),
                y: view.y.saturating_add(1),
                width: view.width.saturating_sub(2),
                height: view.height.saturating_sub(2),
            };
            if contains(inner, col, row) {
                return Some(inner);
            }
        }
        if matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::Orchestrator)
        ) {
            if let Some(area) = self.layout.minibuffer_area {
                let inner = ratatui::layout::Rect {
                    x: area.x,
                    y: area.y.saturating_add(1),
                    width: area.width,
                    height: area.height.saturating_sub(1),
                };
                if contains(inner, col, row) {
                    return Some(inner);
                }
            }
        }
        None
    }

    /// Collapse the orchestrator panel (close the
    /// `MinibufferIntent::Orchestrator` minibuffer) if it's
    /// currently open. Called from every code path that moves
    /// focus to a different pane — clicking list / view / pin
    /// strip, the `SwitchFocus` and `FocusView` actions, the
    /// session-create completion handler. No-op when the panel
    /// isn't open or a different intent (palette, send-input,
    /// rename, etc.) is active.
    fn collapse_orchestrator_panel_on_focus_change(&mut self) {
        if matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::Orchestrator)
        ) {
            self.minibuffer = None;
        }
    }

    /// Hit-test (col, row) against the most recent `block_hits` for
    /// the given session, relative to `inner`. Returns true if the
    /// click was consumed:
    ///
    /// - `[bg]` button row + col range → `client.tool_action(call_id, "background")`.
    /// - `[kill]` button row + col range → `client.tool_action(call_id, "kill")`.
    /// - Else inside a block's row range → toggle expand/collapse.
    /// - Else → false (caller falls through to default focus behavior).
    async fn try_toggle_block_at(
        &mut self,
        session_id: &str,
        inner: ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> bool {
        if col < inner.x
            || col >= inner.x + inner.width
            || row < inner.y
            || row >= inner.y + inner.height
        {
            return false;
        }
        let rel_col = col - inner.x;
        let rel_row = row - inner.y;
        let hits = match self.block_hits.get(session_id) {
            Some(h) => h.clone(),
            None => return false,
        };
        for hit in hits {
            // Button row check. Button cols come from synth_block's
            // fixed-column layout (buttons FIRST in the status row),
            // so the recorded ranges are exact regardless of
            // elapsed-counter width or Unicode glyph width. A click
            // landing on the status row that DOESN'T hit a button
            // is consumed (returns true) so it can't silently fall
            // through to the toggle path.
            if rel_row == hit.header_row {
                if let Some((bs, be)) = hit.bg_button {
                    if rel_col >= bs && rel_col < be {
                        let session_id_owned = session_id.to_string();
                        let call_id_owned = hit.call_id.clone();
                        let short: String = hit.call_id.chars().take(10).collect();
                        match self
                            .client
                            .tool_action(&session_id_owned, call_id_owned, "background")
                            .await
                        {
                            Ok(()) => self.set_status(format!("→ background {short}")),
                            Err(e) => self.set_status(format!("background failed: {e}")),
                        }
                        return true;
                    }
                }
                if let Some((ks, ke)) = hit.kill_button {
                    if rel_col >= ks && rel_col < ke {
                        let session_id_owned = session_id.to_string();
                        let call_id_owned = hit.call_id.clone();
                        let short: String = hit.call_id.chars().take(10).collect();
                        match self
                            .client
                            .tool_action(&session_id_owned, call_id_owned, "kill")
                            .await
                        {
                            Ok(()) => self.set_status(format!("→ kill {short}")),
                            Err(e) => self.set_status(format!("kill failed: {e}")),
                        }
                        return true;
                    }
                }
                if hit.bg_button.is_some() || hit.kill_button.is_some() {
                    return true;
                }
            }
            // Toggle-on-row-range path (footer click for expand /
            // collapse). Only fires on COMPLETED blocks where the
            // footer text exists; the status row was handled above.
            if rel_row >= hit.row_start && rel_row < hit.row_end {
                if let Some(history) = self.histories.get_mut(session_id) {
                    if history.toggle_block(&hit.call_id) {
                        return true;
                    }
                }
            }
        }
        false
    }

    async fn click_minibuffer(&mut self, mb_area: ratatui::layout::Rect, col: u16) {
        if let Some(mb) = self.minibuffer.as_mut() {
            if matches!(mb.intent, MinibufferIntent::ApproveTool { .. }) {
                return;
            }
            // Harness picker: clicking an available name submits it
            // as if the user typed and pressed Enter. Unavailable
            // names are visually disabled (strikethrough); clicks
            // on them drop a status note rather than submitting —
            // the hover tooltip explains why.
            if matches!(mb.intent, MinibufferIntent::NewSessionHarness) {
                let hits = self.layout.minibuffer_harness_hits.clone();
                for hit in hits {
                    if hit.y == mb_area.y && col >= hit.x_start && col < hit.x_end {
                        if !hit.available {
                            self.set_status(format!("{}: adapter binary not installed", hit.name));
                            return;
                        }
                        let intent = mb.intent.clone();
                        self.minibuffer = None;
                        self.run_minibuffer_submit(intent, hit.name).await;
                        return;
                    }
                }
            }
            let prompt_w = unicode_width::UnicodeWidthStr::width(mb.prompt.as_str()) as u16;
            let input_start = mb_area.x + prompt_w;
            if col < input_start {
                mb.cursor = 0;
            } else {
                let offset_cells = (col - input_start) as usize;
                let max = mb.input.chars().count();
                mb.cursor = offset_cells.min(max);
            }
        } else {
            self.run_action(KeyAction::OpenCommandPalette).await;
        }
    }

    async fn click_list(&mut self, list: ratatui::layout::Rect, col: u16, row: u16) {
        // A click anywhere inside the list pane focuses it, even on the
        // border or empty space past the last item — matching the
        // intuitive "click the pane to focus it" UX.
        self.collapse_orchestrator_panel_on_focus_change();
        // Collapsed list pane: any click in the pane (border or
        // body) just re-expands. Don't try to interpret as a row /
        // button click — the geometry is meaningless at 3 cells.
        if self.list_collapsed && self.focus != PaneFocus::List {
            self.list_collapsed = false;
            self.focus = PaneFocus::List;
            return;
        }
        self.focus = PaneFocus::List;
        // Title bar buttons: `+` (left, new session) and `−`
        // (right, collapse). Both live on the top border row.
        if row == list.y {
            if let Some((xs, xe, y)) = crate::ui::list_plus_button_range(list) {
                if row == y && col >= xs && col < xe {
                    self.run_action(crate::keymap::KeyAction::OpenNewSession)
                        .await;
                    return;
                }
            }
            if let Some((xs, xe, y)) = crate::ui::list_collapse_button_range(list) {
                if row == y && col >= xs && col < xe {
                    self.list_collapsed = true;
                    // Drop focus so the collapse takes effect this
                    // frame (effective_collapsed = list_collapsed
                    // && focus != List).
                    self.focus = PaneFocus::View;
                    return;
                }
            }
        }
        if !self.matrix_rain_hidden {
            if let Some(rain) = self.layout.matrix_rain_area {
                if let Some((xs, xe, y)) = crate::ui::matrix_rain_close_button_range(rain) {
                    if row == y && col >= xs && col < xe {
                        self.matrix_rain_hidden = true;
                        self.set_status("matrix rain hidden — M-x rain to show".into());
                        return;
                    }
                }
            }
        }
        // Top + bottom border are 1 row each; rows outside the inner
        // content area only handle the focus change above.
        if row <= list.y || row + 1 >= list.y + list.height {
            return;
        }
        // Clicks inside the (sticky) matrix-rain panel at the bottom
        // of the list pane focus the list but do NOT count as a row
        // click — without this guard, clicks past the last visible
        // item would map to phantom indices when items overflow.
        let items_area = self
            .layout
            .list_items_area
            .unwrap_or(ratatui::layout::Rect {
                x: list.x,
                y: list.y.saturating_add(1),
                width: list.width,
                height: list.height.saturating_sub(2),
            });
        if row < items_area.y || row >= items_area.y + items_area.height {
            return;
        }
        let visible_row = (row - items_area.y) as usize;
        let idx = visible_row + self.layout.list_scroll_offset;
        let items = self.list_items();
        if idx >= items.len() {
            return;
        }
        // The 4-cell gutter to the left of the session name —
        //   [diamond][ ][status-circle][ ]
        // — toggles the pin instead of selecting the row. Wider than
        // the bare diamond so it's easy to click. Must stay in lockstep
        // with `hovered_diamond` in ui.rs.
        if let ListItem::Session { summary, indented } = &items[idx] {
            let indent = if *indented { 2 } else { 0 };
            let zone_start = list.x + 1 + indent;
            let zone_end = zone_start + 4;
            if col >= zone_start && col < zone_end {
                let id = summary.id.clone();
                let next = !summary.pinned;
                if let Err(e) = self.client.set_pinned(&id, next).await {
                    self.set_status(format!("set_pinned failed: {e}"));
                }
                return;
            }
        }
        match &items[idx] {
            ListItem::Session { summary, .. } => {
                self.select_session(summary.id.clone());
            }
            ListItem::GroupHeader { group, .. } => {
                let id = group.id.clone();
                let next = !group.collapsed;
                if self
                    .selection
                    .group_id()
                    .map(|s| s != id.as_str())
                    .unwrap_or(true)
                {
                    self.select_group(id.clone());
                }
                if let Err(e) = self.client.set_group_collapsed(&id, next).await {
                    self.set_status(format!("collapse failed: {e}"));
                }
            }
        }
    }

    async fn click_pin_strip(&mut self, strip: ratatui::layout::Rect, col: u16, row: u16) {
        let pinned_ids: Vec<String> = self
            .list_items()
            .into_iter()
            .filter_map(|it| match it {
                ListItem::Session { summary, .. } if summary.pinned => Some(summary.id),
                _ => None,
            })
            .collect();
        if pinned_ids.is_empty() {
            return;
        }
        let tiles = crate::ui::pin_tile_layout(strip, pinned_ids.len());
        for (tile, id) in tiles.iter().zip(pinned_ids.iter()) {
            if !(col >= tile.x
                && col < tile.x + tile.width
                && row >= tile.y
                && row < tile.y + tile.height)
            {
                continue;
            }
            // Diamond zone: 4 cells on the top border, starting
            // after the corner — covers `[ ][⬩][ ][status]` in the
            // title ` ⬩ <status> <label> <harness> `. Same gesture
            // as clicking the list-view diamond. Must stay in
            // lockstep with `pin_tile_diamond_zone` in ui.rs.
            let diamond_zone_start = tile.x + 1;
            let diamond_zone_end = tile.x + 5;
            if row == tile.y && col >= diamond_zone_start && col < diamond_zone_end {
                if let Err(e) = self.client.set_pinned(id, false).await {
                    self.set_status(format!("unpin failed: {e}"));
                }
                return;
            }
            // Body click: select + drop focus into the view.
            self.select_session(id.clone());
            self.collapse_orchestrator_panel_on_focus_change();
            self.focus = PaneFocus::View;
            return;
        }
    }

    /// Adjust the focused session's scrollback offset. Positive `delta` =
    /// scroll up (older); negative = scroll down (newer). No-op unless the
    /// view is on a PTY-backed session in terminal mode. vt100 clamps the
    /// offset to its actual buffer size internally on `set_scrollback`.
    fn adjust_scrollback(&mut self, delta: i32) {
        if self.is_orchestrator_panel_open() {
            self.orchestrator_scrollback = adjusted_scrollback(self.orchestrator_scrollback, delta);
            return;
        }
        if self.view != ViewMode::Terminal || !self.in_pty_session() {
            return;
        }
        self.view_scrollback = adjusted_scrollback(self.view_scrollback, delta);
    }

    fn adjust_mouse_list_scroll(&mut self, col: u16, row: u16, delta: i32) -> bool {
        let Some(area) = self.layout.list_items_area else {
            return false;
        };
        if col < area.x || col >= area.x + area.width || row < area.y || row >= area.y + area.height
        {
            return false;
        }
        self.adjust_list_scroll(delta);
        true
    }

    fn adjust_list_scroll(&mut self, delta: i32) {
        let visible_h = self
            .layout
            .list_items_area
            .map(|area| area.height as usize)
            .unwrap_or(0);
        self.list_scroll_offset = adjusted_list_scroll_offset(
            self.list_scroll_offset,
            delta,
            self.list_items().len(),
            visible_h,
        );
    }

    fn adjust_mouse_scrollback(&mut self, col: u16, row: u16, delta: i32) {
        if self.is_orchestrator_panel_open() {
            if let Some(area) = self.layout.minibuffer_area {
                if col >= area.x
                    && col < area.x + area.width
                    && row >= area.y
                    && row < area.y + area.height
                {
                    self.orchestrator_scrollback =
                        adjusted_scrollback(self.orchestrator_scrollback, delta);
                    return;
                }
            }
        }
        if self.view == ViewMode::Terminal && self.in_pty_session() {
            self.view_scrollback = adjusted_scrollback(self.view_scrollback, delta);
        }
    }

    fn is_orchestrator_panel_open(&self) -> bool {
        matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::Orchestrator)
        )
    }

    fn can_scroll_pty_history(&self) -> bool {
        self.is_orchestrator_panel_open()
            || (self.view == ViewMode::Terminal && self.in_pty_session())
    }

    /// Tell every relevant PTY child about the new pane geometry. The actual
    /// parser-side `set_size` happens during render (so within a single
    /// frame the parser's screen size matches the area we draw into);
    /// this method only sends the SIGWINCH-equivalent down to the adapter
    /// children.
    pub async fn notify_pane_size(&mut self, cols: u16, rows: u16) {
        let targets: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| {
                s.has_pty
                    && !s.state.is_terminal()
                    && (s.pinned || Some(s.id.as_str()) == self.selected_id().as_deref())
            })
            .map(|s| s.id.clone())
            .collect();
        for id in targets {
            let _ = self.client.pty_resize(&id, cols, rows).await;
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        self.text_selection = None;
        self.selected_text = None;
        self.selected_text_bounds = None;
        self.selected_text_range = None;
        // /tasks modal: Esc closes it; everything else falls through
        // (the popup itself is read-only at the keyboard layer in
        // v1 — mouse-only row interactions).
        if !self.connected && matches!(key.code, KeyCode::Char('q')) {
            self.should_quit = true;
            return;
        }
        if self.tasks_popup.is_some() {
            if matches!(key.code, KeyCode::Esc) {
                self.tasks_popup = None;
                return;
            }
        }
        // /remote-control modal: Esc closes the popup *and* the
        // orchestrator panel it was launched from, so a single Esc
        // returns the user to whichever session they had focused
        // before typing the slash. Without the orchestrator-close
        // step, the panel keeps routing every subsequent keystroke
        // to god's PTY — the user reported "couldn't type prompt
        // from tui after enabling remote control" because of this.
        //
        // Non-Esc keys are *eaten* while the popup is visible — the
        // popup body is informational only (URL + QR), and falling
        // through to the underlying handler would silently route
        // typing into god / a session under the modal.
        if self.remote_control_popup.is_some() {
            if matches!(key.code, KeyCode::Esc) {
                self.remote_control_popup = None;
                if matches!(
                    self.minibuffer.as_ref().map(|m| &m.intent),
                    Some(MinibufferIntent::Orchestrator)
                ) {
                    self.minibuffer = None;
                }
            }
            return;
        }
        // Minibuffer captures all input when open — with one exception:
        // the orchestrator intent is just a focus marker for a
        // PTY-backed panel, so keys go to the orchestrator session's
        // PTY (with the standard `C-x` chord escape) rather than into
        // the minibuffer's text input.
        if let Some(mb) = &self.minibuffer {
            if matches!(mb.intent, MinibufferIntent::Orchestrator) {
                self.handle_orchestrator_key(key).await;
                return;
            }
            self.handle_minibuffer_key(key).await;
            return;
        }
        if self.help_visible {
            // Any key closes help.
            self.help_visible = false;
            return;
        }

        if self.should_autofocus_view_from_list(key) {
            self.collapse_orchestrator_panel_on_focus_change();
            self.focus = PaneFocus::View;
        }

        // When the PTY is capturing keystrokes (View focus + terminal mode +
        // session has a PTY), keys go straight to the child *unless* the user
        // is starting or continuing a `C-x` chord — those drive the keymap.
        if self.is_pty_captured() {
            let is_ctrl_x = matches!(key.code, KeyCode::Char('x'))
                && key.modifiers.contains(KeyModifiers::CONTROL);
            // Escape hatch: `C-x C-x` sends a literal C-x byte through to the
            // PTY (so vim completion, bash's `C-x C-e`, etc. still work).
            if !self.chord_state.is_empty() && is_ctrl_x {
                self.chord_state = ChordState::default();
                self.chord_label.clear();
                if let Some(id) = self.selected_id() {
                    self.queue_pty_input(id, vec![0x18], "pty_input");
                }
                return;
            }
            if self.chord_state.is_empty() && !is_ctrl_x {
                // Typing snaps the view back to live: it's confusing to
                // type "into the past" while reading scrollback.
                self.view_scrollback = 0;
                if let Some(bytes) = encode_key_to_bytes(key) {
                    if let Some(id) = self.selected_id() {
                        self.queue_pty_input(id, bytes, "pty_input");
                    }
                }
                return;
            }
            // fall through to chord dispatch below
        }

        let res = self.chord_state.handle(key, &self.keymap);
        self.chord_label = self.chord_state.label();
        match res {
            KeymapResult::Action(a) => self.run_action(a).await,
            KeymapResult::Pending(label) => self.chord_label = label,
            KeymapResult::Unhandled => {
                self.chord_label.clear();
            }
        }
    }

    fn in_pty_session(&self) -> bool {
        self.selected_session().map(|s| s.has_pty).unwrap_or(false)
    }

    fn should_autofocus_view_from_list(&self, key: KeyEvent) -> bool {
        should_autofocus_view_from_list(self.focus, self.zoom, self.chord_state.is_empty(), key)
    }

    /// True when keystrokes should be forwarded to the session's PTY by
    /// default (view focused, terminal mode, session has a *live* PTY).
    /// Once the session reaches a terminal state the PTY is gone, so the
    /// view turns read-only and keys fall back to the regular keymap.
    fn is_pty_captured(&self) -> bool {
        let live = self
            .selected_session()
            .map(|s| !s.state.is_terminal())
            .unwrap_or(false);
        self.focus == PaneFocus::View
            && self.view == ViewMode::Terminal
            && self.in_pty_session()
            && live
    }

    async fn run_action(&mut self, action: KeyAction) {
        use KeyAction::*;
        match action {
            Quit => self.should_quit = true,
            NextSession => self.step_selection(1).await,
            PrevSession => self.step_selection(-1).await,
            Refresh => {
                self.refresh_sessions().await;
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
            }
            OpenSendInput => {
                if let Some(id) = self.selected_id() {
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!("Send to {}: ", short_id(&id)),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::SendInput { session_id: id },
                        error: None,
                    });
                } else {
                    self.set_status("no session selected".to_string());
                }
            }
            OpenNewSession => {
                if self.harnesses.is_empty() {
                    self.harnesses = self.client.harnesses().await.unwrap_or_default();
                }
                let mut names: Vec<&str> = self
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.as_str())
                    .collect();
                // `group` is a synthetic option that creates a group instead
                // of a session — surfaced in the same wizard for discovery.
                names.push("group");
                let hint = names.join("|");
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("New [{hint}] (Tab completes): "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::NewSessionHarness,
                    error: None,
                });
            }
            OpenRename => match self.selection.clone() {
                Selection::Session(id) => {
                    let Some(s) = self.sessions.iter().find(|s| s.id == id) else {
                        return;
                    };
                    let current = s.title.clone().unwrap_or_default();
                    let cursor = current.chars().count();
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!("Rename {} to: ", short_id(&id)),
                        input: current,
                        cursor,
                        intent: MinibufferIntent::Rename { session_id: id },
                        error: None,
                    });
                }
                Selection::Group(id) => {
                    let Some(g) = self.groups.iter().find(|g| g.id == id) else {
                        return;
                    };
                    let current = g.name.clone();
                    let cursor = current.chars().count();
                    self.minibuffer = Some(Minibuffer {
                        prompt: "Rename group to: ".to_string(),
                        input: current,
                        cursor,
                        intent: MinibufferIntent::GroupRename { group_id: id },
                        error: None,
                    });
                }
                Selection::None => self.set_status("nothing selected".into()),
            },
            OpenDeleteConfirm => match self.selection.clone() {
                Selection::Session(id) => {
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!(
                            "Delete {} (kill if running, drop transcript + worktree)? (y/N): ",
                            short_id(&id)
                        ),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::DeleteConfirm { session_id: id },
                        error: None,
                    });
                }
                Selection::Group(id) => {
                    let name = self
                        .groups
                        .iter()
                        .find(|g| g.id == id)
                        .map(|g| g.name.clone())
                        .unwrap_or_default();
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!(
                            "Delete group '{}'? (y = orphan members / type 'all' to delete sessions too / N = cancel): ",
                            name
                        ),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::GroupDeleteConfirm { group_id: id },
                        error: None,
                    });
                }
                Selection::None => {}
            },
            OpenDiff => {
                if let Some(id) = self.selected_id() {
                    match self.client.diff(&id).await {
                        Ok(r) => {
                            if r.patch.is_empty() {
                                self.set_status("(no diff)".to_string());
                                self.last_diff = None;
                            } else {
                                self.last_diff = Some(r.patch);
                            }
                        }
                        Err(e) => self.set_status(format!("diff failed: {e}")),
                    }
                }
            }
            Interrupt => {
                if let Some(id) = self.selected_id() {
                    match self.client.interrupt(&id).await {
                        Ok(()) => self.set_status("interrupt sent".to_string()),
                        Err(e) => self.set_status(format!("interrupt failed: {e}")),
                    }
                }
            }
            OpenCommandPalette => {
                self.open_minibuffer_for_command();
            }
            FocusView => {
                // Enter on a *terminated* session opens a restart
                // confirmation instead of drilling in. Typing into
                // the prompt of a Done/Errored session is a no-op
                // anyway (adapter exited; PTY writes go nowhere), so
                // surface a path back to a live adapter while we're
                // here. Live sessions keep the original drill-in.
                //
                // This applies from both panes: when a terminated
                // session is already focused in the view, the PTY is
                // no longer capturing keys, so Enter should offer the
                // same restart path as it does from the list.
                if let Some(s) = self.selected_session() {
                    if s.state.is_terminal() {
                        let session_id = s.id.clone();
                        let short = short_id(&session_id).to_string();
                        self.minibuffer = Some(Minibuffer {
                            prompt: format!("Restart session {short}? (y/N): "),
                            input: String::new(),
                            cursor: 0,
                            intent: MinibufferIntent::RestartConfirm { session_id },
                            error: None,
                        });
                        return;
                    }
                }
                // Enter from the list drills into the session view.
                // In zoomed-list mode, flip the zoom to the view side
                // so the pane the user is "entering" actually fills
                // the screen (mirrors SwitchFocus's zoom-aware path).
                match self.zoom {
                    ZoomMode::List => {
                        self.zoom = ZoomMode::View;
                    }
                    ZoomMode::View | ZoomMode::None => {}
                }
                self.collapse_orchestrator_panel_on_focus_change();
                self.focus = PaneFocus::View;
            }
            SwitchFocus => {
                // In a zoomed layout `C-x o` swaps which pane is
                // zoomed (and focused). In normal layout it just
                // swaps focus.
                self.collapse_orchestrator_panel_on_focus_change();
                match self.zoom {
                    ZoomMode::List => {
                        self.zoom = ZoomMode::View;
                        self.focus = PaneFocus::View;
                    }
                    ZoomMode::View => {
                        self.zoom = ZoomMode::List;
                        self.focus = PaneFocus::List;
                    }
                    ZoomMode::None => {
                        self.focus = match self.focus {
                            PaneFocus::List => PaneFocus::View,
                            PaneFocus::View => PaneFocus::List,
                        };
                    }
                }
                let label = match self.focus {
                    PaneFocus::List => "focus: list",
                    PaneFocus::View => "focus: view",
                };
                self.set_status(label.into());
            }
            ToggleZoom => {
                // Zoom the currently-focused pane; if anything is
                // already zoomed, unzoom back to the split layout.
                self.zoom = match self.zoom {
                    ZoomMode::None => match self.focus {
                        PaneFocus::List => ZoomMode::List,
                        PaneFocus::View => ZoomMode::View,
                    },
                    _ => ZoomMode::None,
                };
                // Keep focus in sync with whatever's visible.
                self.focus = match self.zoom {
                    ZoomMode::List => PaneFocus::List,
                    ZoomMode::View => PaneFocus::View,
                    ZoomMode::None => self.focus,
                };
                // The parser is re-sized in render and `pty_resize` propagates
                // SIGWINCH to the child. We intentionally do NOT send Ctrl-L
                // here — that would clear the screen in bash, wiping the
                // user's scrollback. Existing output stays put; new output
                // continues at the cursor's current row.
                self.set_status(
                    if self.zoom != ZoomMode::None {
                        "zoomed — C-x z to unzoom"
                    } else {
                        "zoom off"
                    }
                    .into(),
                );
            }
            ToggleView => {
                let has_pty = self.in_pty_session();
                self.view = match (self.view, has_pty) {
                    (ViewMode::Transcript, true) => {
                        // First time switching → bootstrap from replay snapshot.
                        if let Some(id) = self.selected_id() {
                            self.bootstrap_terminal(&id).await;
                        }
                        ViewMode::Terminal
                    }
                    _ => ViewMode::Transcript,
                };
            }
            MoveSelectedUp => self.move_selected(true).await,
            MoveSelectedDown => self.move_selected(false).await,
            TogglePin => {
                self.toggle_pin_on_selection().await;
            }
            ExpandGroup => {
                if let Some(g) = self.selected_group() {
                    let id = g.id.clone();
                    if let Err(e) = self.client.set_group_collapsed(&id, false).await {
                        self.set_status(format!("expand failed: {e}"));
                    }
                }
            }
            CollapseGroup => {
                if let Some(g) = self.selected_group() {
                    let id = g.id.clone();
                    if let Err(e) = self.client.set_group_collapsed(&id, true).await {
                        self.set_status(format!("collapse failed: {e}"));
                    }
                }
            }
            ScrollUp => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(1);
                } else if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                }
            }
            ScrollDown => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(-1);
                } else if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(1);
                }
            }
            ScrollPageUp => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(10);
                } else if self.transcript_scroll == u16::MAX {
                    self.transcript_scroll = 0;
                } else {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(10);
                }
            }
            ScrollPageDown => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(-10);
                } else if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(10);
                }
            }
            ScrollTop => {
                if self.can_scroll_pty_history() {
                    if self.is_orchestrator_panel_open() {
                        self.orchestrator_scrollback = SCROLLBACK_MAX;
                    } else {
                        self.view_scrollback = SCROLLBACK_MAX;
                    }
                } else {
                    self.transcript_scroll = 0;
                }
            }
            ScrollBottom => {
                if self.can_scroll_pty_history() {
                    if self.is_orchestrator_panel_open() {
                        self.orchestrator_scrollback = 0;
                    } else {
                        self.view_scrollback = 0;
                    }
                } else {
                    self.transcript_scroll = u16::MAX;
                }
            }
            ToggleHelp => {
                self.help_visible = !self.help_visible;
            }
            ToggleAutomode => {
                self.toggle_automode().await;
            }
            ToggleMouseCapture => {
                self.toggle_mouse_capture();
            }
        }
    }

    fn toggle_mouse_capture(&mut self) {
        self.mouse_capture_enabled = !self.mouse_capture_enabled;
        self.mouse_pos = None;
        let result = if self.mouse_capture_enabled {
            execute!(std::io::stdout(), EnableMouseCapture)
        } else {
            execute!(std::io::stdout(), DisableMouseCapture)
        };
        match result {
            Ok(()) if self.mouse_capture_enabled => {
                self.set_status("mouse capture on".into());
            }
            Ok(()) => {
                self.set_status("mouse capture off — drag to select text".into());
            }
            Err(e) => {
                self.mouse_capture_enabled = !self.mouse_capture_enabled;
                self.set_status(format!("mouse toggle failed: {e}"));
            }
        }
    }

    /// Key handler for the orchestrator panel: same shape as the main
    /// view's PTY mode (C-x chord escape, `C-x C-x` to forward a
    /// literal C-x byte). `Esc` closes the panel; the next `C-x x`
    /// reopens it. All non-chord keys are encoded to PTY bytes and
    /// forwarded to the orchestrator session.
    async fn handle_orchestrator_key(&mut self, key: KeyEvent) {
        let Some(orch_id) = self.orchestrator_id.clone() else {
            // Orchestrator went away — fall back to palette mode.
            self.minibuffer = None;
            return;
        };
        let is_ctrl_x =
            matches!(key.code, KeyCode::Char('x')) && key.modifiers.contains(KeyModifiers::CONTROL);
        // Escape hatch: `C-x C-x` sends a literal C-x byte through to the
        // PTY (matching the main-view PTY behavior).
        if !self.chord_state.is_empty() && is_ctrl_x {
            self.chord_state = ChordState::default();
            self.chord_label.clear();
            self.queue_pty_input(orch_id, vec![0x18], "orchestrator pty_input");
            return;
        }
        // Start of a chord or continuation: dispatch through the keymap.
        if !self.chord_state.is_empty() || is_ctrl_x {
            let res = self.chord_state.handle(key, &self.keymap);
            self.chord_label = self.chord_state.label();
            match res {
                KeymapResult::Action(a) => self.run_action(a).await,
                KeymapResult::Pending(label) => self.chord_label = label,
                KeymapResult::Unhandled => self.chord_label.clear(),
            }
            return;
        }
        // Esc closes the panel without sending anything to the PTY.
        if matches!(key.code, KeyCode::Esc) {
            self.minibuffer = None;
            return;
        }
        // Everything else goes to the orchestrator's PTY.
        if let Some(bytes) = encode_key_to_bytes(key) {
            // Typing into god snaps back to live output, matching the main
            // PTY pane's behavior.
            self.orchestrator_scrollback = 0;
            self.queue_pty_input(orch_id, bytes, "orchestrator pty_input");
        }
    }

    async fn handle_minibuffer_key(&mut self, key: KeyEvent) {
        // Snapshot the data we'll need without holding a borrow on
        // self.minibuffer across the (possibly &self) lookups.
        let is_new_harness = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::NewSessionHarness)
        );
        let available_harnesses: Vec<String> = if is_new_harness {
            let mut v: Vec<String> = self
                .harnesses
                .iter()
                .filter(|h| h.available)
                .map(|h| h.name.clone())
                .collect();
            v.push("group".to_string());
            v
        } else {
            Vec::new()
        };

        // Restart confirmation: single-key dispatch (`y` confirms,
        // anything else cancels) so the user can press one key and
        // move on, matching the way they invoked the prompt with a
        // single Enter on the Done session.
        let restart_intent = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::RestartConfirm { .. })
        );
        if restart_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // Pull the session_id out so we can drop the minibuffer
            // borrow before we await the client call.
            let session_id = match self.minibuffer.as_ref().map(|m| &m.intent) {
                Some(MinibufferIntent::RestartConfirm { session_id }) => session_id.clone(),
                _ => return,
            };
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.minibuffer = None;
                    match self.client.restart(&session_id).await {
                        Ok(()) => {
                            self.editor_states.remove(&session_id);
                            self.agent_statuses.remove(&session_id);
                            self.set_status(format!("restarted {}", short_id(&session_id)));
                        }
                        Err(e) => self.set_status(format!("restart failed: {e}")),
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.minibuffer = None;
                    self.set_status("restart cancelled".to_string());
                }
                KeyCode::Char('g') if ctrl => {
                    self.minibuffer = None;
                    self.set_status("restart cancelled".to_string());
                }
                _ => {
                    // Ignore other keys so a stray keystroke doesn't
                    // accidentally cancel the prompt mid-thought.
                }
            }
            return;
        }

        // Approval prompt has single-key shortcuts; bypass the normal
        // editing path so the user can hit y/n/a without typing + Enter.
        let approve_intent = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::ApproveTool { .. })
        );
        if approve_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some("approve"),
                KeyCode::Char('n') | KeyCode::Esc => Some("deny"),
                KeyCode::Char('a') => Some("automode"),
                KeyCode::Char('g') if ctrl => Some("deny"),
                _ => None,
            };
            if let Some(d) = decision {
                if let Some(MinibufferIntent::ApproveTool {
                    session_id,
                    call_id,
                    ..
                }) = self.minibuffer.as_ref().map(|m| m.intent.clone())
                {
                    self.minibuffer = None;
                    match self.client.tool_decision(&session_id, call_id, d).await {
                        Ok(()) => {
                            self.matrix_rain
                                .observe_tool_decision(d, self.matrix_rain_intensity);
                            self.set_status(format!("tool {d}"));
                        }
                        Err(e) => self.set_status(format!("tool_decision failed: {e}")),
                    }
                }
            }
            return;
        }

        let Some(mb) = self.minibuffer.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Esc => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Char('g') if ctrl => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Tab => {
                if is_new_harness {
                    apply_harness_completion(mb, &available_harnesses);
                }
                return;
            }
            KeyCode::Enter => {
                if is_new_harness {
                    let trimmed = mb.input.trim().to_string();
                    if trimmed.is_empty() {
                        mb.error = Some("pick a harness".to_string());
                        return;
                    }
                    if !available_harnesses.iter().any(|h| h == &trimmed) {
                        mb.error = Some(format!("unknown: {trimmed} (Tab to complete)"));
                        return;
                    }
                }
                let intent = mb.intent.clone();
                let input = std::mem::take(&mut mb.input);
                self.minibuffer = None;
                self.run_minibuffer_submit(intent, input).await;
                return;
            }
            KeyCode::Backspace => {
                delete_back_char(mb);
            }
            KeyCode::Delete => {
                delete_forward_char(mb);
            }
            KeyCode::Left if alt => mb.cursor = word_back(&mb.input, mb.cursor),
            KeyCode::Right if alt => mb.cursor = word_forward(&mb.input, mb.cursor),
            KeyCode::Left => mb.cursor = mb.cursor.saturating_sub(1),
            KeyCode::Right => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Home => mb.cursor = 0,
            KeyCode::End => mb.cursor = mb.input.chars().count(),

            // Emacs editing chords on Ctrl.
            KeyCode::Char('a') if ctrl => mb.cursor = 0,
            KeyCode::Char('e') if ctrl => mb.cursor = mb.input.chars().count(),
            KeyCode::Char('b') if ctrl => mb.cursor = mb.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Char('d') if ctrl => delete_forward_char(mb),
            KeyCode::Char('h') if ctrl => delete_back_char(mb),
            KeyCode::Char('k') if ctrl => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.truncate(pos);
                mb.error = None;
            }
            KeyCode::Char('u') if ctrl => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.replace_range(..pos, "");
                mb.cursor = 0;
                mb.error = None;
            }
            KeyCode::Char('w') if ctrl => kill_word_back(mb),

            // Emacs editing chords on Meta.
            KeyCode::Char('b') if alt => mb.cursor = word_back(&mb.input, mb.cursor),
            KeyCode::Char('f') if alt => mb.cursor = word_forward(&mb.input, mb.cursor),
            KeyCode::Char('d') if alt => kill_word_forward(mb),

            // Plain printable insertion. Ignore anything with Ctrl/Alt that
            // wasn't handled above so stray modifier combos don't pollute
            // the input.
            KeyCode::Char(c) if !ctrl && !alt => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.insert(pos, c);
                mb.cursor += 1;
                mb.error = None;
            }
            _ => {}
        }
    }

    async fn run_minibuffer_submit(&mut self, intent: MinibufferIntent, input: String) {
        match intent {
            MinibufferIntent::SendInput { session_id } => {
                if input.is_empty() {
                    return;
                }
                match self.client.send_input(&session_id, input).await {
                    Ok(()) => self.set_status("input sent".to_string()),
                    Err(e) => self.set_status(format!("send failed: {e}")),
                }
            }
            MinibufferIntent::NewSessionHarness => {
                let harness = input.trim().to_string();
                if harness.is_empty() {
                    return;
                }
                // 'group' is a synthetic option in the harness picker that
                // redirects to the group-create flow.
                if harness == "group" {
                    self.minibuffer = Some(Minibuffer {
                        prompt: "Group name: ".to_string(),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::NewGroupName,
                        error: None,
                    });
                    return;
                }
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                // Inherit the group context from the current selection
                // so creating a session "while inside" a group keeps
                // the new session in that same group.
                let group_id = match &self.selection {
                    Selection::Group(gid) => Some(gid.clone()),
                    Selection::Session(sid) => self
                        .sessions
                        .iter()
                        .find(|s| s.id == *sid)
                        .and_then(|s| s.group_id.clone()),
                    Selection::None => None,
                };
                let params = agentd_protocol::CreateSessionParams {
                    harness: harness.clone(),
                    cwd,
                    prompt: None,
                    model: None,
                    title: None,
                    mode: None,
                    pty_size: Some(agentd_protocol::PtySize {
                        cols: self.terminal_pane_size.0.max(20),
                        rows: self.terminal_pane_size.1.max(5),
                    }),
                    worktree: false,
                    env: HashMap::new(),
                    args: Vec::new(),
                    kind: agentd_protocol::SessionKind::User,
                    group_id,
                };
                match self.client.create(params).await {
                    Ok(id) => {
                        self.set_status(format!("created {}", short_id(&id)));
                        self.refresh_sessions().await;
                        // Pre-insert an empty PTY parser so the subsequent
                        // `refresh_selected_transcript → bootstrap_terminal`
                        // short-circuits (parser already present). Our live
                        // subscription will deliver every byte the adapter
                        // emits; without this short-circuit, pty_replay
                        // would race the subscription and the banner ends
                        // up rendered twice (once from the ring, once from
                        // the live broadcast that was already in flight).
                        if !self.histories.contains_key(&id) {
                            self.histories
                                .insert(id.clone(), crate::pty_render::ItemHistory::new());
                        }
                        self.select_session(id);
                        self.focus = PaneFocus::View;
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
                }
            }
            MinibufferIntent::GroupDeleteConfirm { group_id } => {
                let choice = parse_group_delete_choice(&input);
                let delete_members = match choice {
                    GroupDeleteChoice::Cancel => {
                        self.set_status("group delete cancelled".to_string());
                        return;
                    }
                    GroupDeleteChoice::OrphanMembers => false,
                    GroupDeleteChoice::DeleteMembers => true,
                };
                match self.client.delete_group(&group_id, delete_members).await {
                    Ok(()) => {
                        let msg = if delete_members {
                            "group + all sessions deleted"
                        } else {
                            "group deleted (members orphaned)"
                        };
                        self.set_status(msg.into());
                    }
                    Err(e) => self.set_status(format!("group delete failed: {e}")),
                }
            }
            MinibufferIntent::GroupRename { group_id } => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    self.set_status("group rename cancelled (empty)".into());
                    return;
                }
                match self.client.rename_group(&group_id, &trimmed).await {
                    Ok(()) => {
                        if let Some(g) = self.groups.iter_mut().find(|g| g.id == group_id) {
                            g.name = trimmed.clone();
                        }
                        self.set_status(format!("renamed group → {trimmed}"));
                    }
                    Err(e) => self.set_status(format!("group rename failed: {e}")),
                }
            }
            MinibufferIntent::NewGroupName => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    self.set_status("group name empty".into());
                    return;
                }
                match self.client.create_group(&trimmed).await {
                    Ok(id) => {
                        self.set_status(format!("created group '{trimmed}'"));
                        self.refresh_sessions().await; // also refreshes groups
                        self.select_group(id);
                    }
                    Err(e) => self.set_status(format!("group create failed: {e}")),
                }
            }
            MinibufferIntent::Rename { session_id } => {
                let trimmed = input.trim().to_string();
                let new_title = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                match self.client.set_title(&session_id, new_title.clone()).await {
                    Ok(()) => {
                        // Optimistically reflect locally.
                        if let Some(i) = self.sessions.iter().position(|s| s.id == session_id) {
                            self.sessions[i].title = new_title.clone();
                        }
                        self.set_status(match &new_title {
                            Some(t) => format!("renamed → {t}"),
                            None => "title cleared".into(),
                        });
                    }
                    Err(e) => self.set_status(format!("rename failed: {e}")),
                }
            }
            MinibufferIntent::DeleteConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("delete cancelled".to_string());
                    return;
                }
                match self.client.delete(&session_id).await {
                    Ok(()) => self.set_status(format!("deleted {}", short_id(&session_id))),
                    Err(e) => self.set_status(format!("delete failed: {e}")),
                }
            }
            MinibufferIntent::RestartConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("restart cancelled".to_string());
                    return;
                }
                match self.client.restart(&session_id).await {
                    Ok(()) => {
                        // After restart, the new adapter will emit
                        // EditorState on first input — but the user
                        // expects the prompt to be ready right away.
                        // Drop any cached editor state from the dead
                        // adapter so the next render reserves the
                        // editor pane preemptively (the
                        // bootstrap-replay path I landed earlier
                        // will repopulate it from the resumed
                        // adapter's transcript).
                        self.editor_states.remove(&session_id);
                        self.agent_statuses.remove(&session_id);
                        self.set_status(format!("restarted {}", short_id(&session_id)));
                    }
                    Err(e) => self.set_status(format!("restart failed: {e}")),
                }
            }
            MinibufferIntent::CommandPalette => {
                let cmd = input.trim();
                self.run_palette_command(cmd).await;
            }
            MinibufferIntent::Orchestrator => {
                // Unreachable in PTY-orchestrator mode — the
                // orchestrator panel's keys are handled in
                // handle_orchestrator_key and never reach the regular
                // submit path. Kept as a defensive fallback.
                let _ = input;
            }
            MinibufferIntent::ApproveTool {
                session_id,
                call_id,
                ..
            } => {
                // Reached only if the special-cased key handler in
                // handle_minibuffer_key fell through (defensive — should
                // not happen in practice). Treat any submit as approve.
                if let Err(e) = self
                    .client
                    .tool_decision(&session_id, call_id, "approve")
                    .await
                {
                    self.set_status(format!("tool_decision failed: {e}"));
                } else {
                    self.matrix_rain
                        .observe_tool_decision("approve", self.matrix_rain_intensity);
                }
            }
        }
    }

    async fn run_palette_command(&mut self, cmd: &str) {
        // Palette text is the same shape as a slash command without
        // the leading `/`; share the dispatch.
        self.run_slash_command(cmd).await;
    }

    /// Execute a slash-style command (`zoom`, `new`, `quit`, ...) with
    /// no LLM involvement. Used both by the orchestrator panel (when
    /// input starts with `/`) and by the static palette (fallback when
    /// no orchestrator is present).
    pub async fn run_slash_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        // Split into verb + remaining args. Commands that don't
        // take args ignore the tail silently (slight behavior
        // change from the previous strict-match style, but more
        // forgiving and lets verbs like `remote-control <pw>` reuse
        // the same dispatcher).
        let (verb, arg) = cmd
            .split_once(char::is_whitespace)
            .map(|(v, a)| (v, a.trim()))
            .unwrap_or((cmd, ""));
        match verb {
            "" => {}
            "quit" | "exit" => self.should_quit = true,
            "refresh" => {
                self.refresh_sessions().await;
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
            }
            "new" | "new-session" => self.run_action(KeyAction::OpenNewSession).await,
            "send" | "send-input" => self.run_action(KeyAction::OpenSendInput).await,
            "delete" | "kill" | "rm" => self.run_action(KeyAction::OpenDeleteConfirm).await,
            "rename" => self.run_action(KeyAction::OpenRename).await,
            "zoom" | "fullscreen" => self.run_action(KeyAction::ToggleZoom).await,
            "rain" | "matrix" | "matrix-rain" => {
                self.matrix_rain_hidden = !self.matrix_rain_hidden;
                self.set_status(format!(
                    "matrix rain {}",
                    if self.matrix_rain_hidden {
                        "hidden"
                    } else {
                        "shown"
                    }
                ));
            }
            "border" => {
                self.hide_pane_side_borders = !self.hide_pane_side_borders;
                self.set_status(format!(
                    "pane side borders {}",
                    if self.hide_pane_side_borders {
                        "hidden"
                    } else {
                        "shown"
                    }
                ));
            }
            "diff" => self.run_action(KeyAction::OpenDiff).await,
            "interrupt" => self.run_action(KeyAction::Interrupt).await,
            "mouse" | "select" | "selection" => {
                self.run_action(KeyAction::ToggleMouseCapture).await
            }
            "help" | "?" => self.help_visible = true,
            "tasks" => {
                self.open_tasks_popup().await;
            }
            "remote-control" | "remote" => {
                // Subcommand dispatch: `stop` and `debug` are
                // reserved keywords; anything else is treated as
                // a literal password override (so a user who
                // wants the password `stop` has to pick a
                // different word — fine for v1).
                //
                //   /remote-control                  → start (auto pw)
                //   /remote-control stop             → stop
                //   /remote-control debug            → local-only
                //   /remote-control debug myword     → local-only + pw
                //   /remote-control <anything else>  → start + pw=<that>
                let (sub, rest) = arg
                    .split_once(char::is_whitespace)
                    .map(|(s, r)| (s, r.trim()))
                    .unwrap_or((arg, ""));
                match sub {
                    "stop" => self.stop_remote_control().await,
                    "debug" => {
                        let pw = (!rest.is_empty()).then(|| rest.to_string());
                        self.open_remote_control_popup(true, pw).await;
                    }
                    "" => self.open_remote_control_popup(false, None).await,
                    _ => {
                        // Everything (including any trailing
                        // whitespace-separated tokens) becomes
                        // the password — supports passwords with
                        // spaces like `/remote-control my secret`.
                        let pw = arg.to_string();
                        self.open_remote_control_popup(false, Some(pw)).await;
                    }
                }
            }
            "harnesses" => {
                self.harnesses = self.client.harnesses().await.unwrap_or_default();
                let names: Vec<String> = self
                    .harnesses
                    .iter()
                    .map(|h| {
                        let mark = if h.available { "ok" } else { "missing" };
                        format!("{} ({})", h.name, mark)
                    })
                    .collect();
                self.set_status(format!("harnesses: {}", names.join(", ")));
            }
            "agentd" => {
                // Subcommand dispatch:
                //
                //   /agentd restart   → daemon.restart (exec self)
                //
                // Other subcommands are reserved for future use
                // (e.g. `/agentd info` to print build version). The
                // daemon.restart RPC will close the IPC connection
                // as the new process replaces the old; the TUI
                // observes that as a "daemon disconnected" status
                // and the user must re-run `agent` to reconnect
                // (auto-reconnect is follow-up work, see issue #90).
                let sub = arg.trim();
                match sub {
                    "restart" => {
                        match self.client.daemon_restart().await {
                            Ok(r) => self.set_status(format!(
                                "agentd: restart requested (exe={}, pid={}) — reconnect when ready",
                                r.exe, r.pid
                            )),
                            // BrokenPipe / connection closed is the
                            // expected outcome — the daemon execs
                            // before fully writing the reply on
                            // some platforms. Treat it as success.
                            Err(e) => {
                                let msg = e.to_string().to_lowercase();
                                if msg.contains("broken pipe")
                                    || msg.contains("connection reset")
                                    || msg.contains("eof")
                                    || msg.contains("closed")
                                {
                                    self.set_status(
                                        "agentd: restart in flight (socket closed) — reconnect when ready".to_string(),
                                    );
                                } else {
                                    self.set_status(format!("agentd restart failed: {e}"));
                                }
                            }
                        }
                    }
                    "" => self.set_status("agentd: subcommand required (e.g. `restart`)".into()),
                    other => self.set_status(format!(
                        "agentd: unknown subcommand '{other}'; try `restart`"
                    )),
                }
            }
            other => self.set_status(format!("unknown command: {other}")),
        }
    }

    /// Snapshot the selected session's task registry and open the
    /// `/tasks` modal popup. The popup is read-only on its data
    /// (no live updates while open); the user closes with Esc and
    /// re-opens to refresh. Click handlers in the popup itself can
    /// issue `tool_action(kill)` to terminate running tasks.
    pub async fn open_tasks_popup(&mut self) {
        let Some(id) = self.selected_id().or_else(|| self.orchestrator_id.clone()) else {
            self.set_status("no session selected".into());
            return;
        };
        match self.client.list_tasks(&id).await {
            Ok(tasks) => {
                self.tasks_popup = Some(TasksPopup {
                    session_id: id,
                    tasks,
                });
            }
            Err(e) => self.set_status(format!("list_tasks failed: {e}")),
        }
    }

    /// Call `remote.start` on the daemon and surface the resulting
    /// URL + QR in the modal.
    ///
    /// `local_only=false` is the `/remote-control` slash: the
    /// daemon waits for cloudflared and the result is always the
    /// public `wss://…trycloudflare.com` URL — or, on timeout, a
    /// JSON-RPC error with an actionable diagnostic that the popup
    /// shows in an error state. No more "warming up" trap where
    /// rerunning loops on the same hint.
    ///
    /// `local_only=true` is the `/remote-control-debug` slash:
    /// returns the local `ws://127.0.0.1` URL immediately, never
    /// touches cloudflared. Useful for desktop-browser smoke tests
    /// and CI.
    pub async fn open_remote_control_popup(&mut self, local_only: bool, password: Option<String>) {
        if let Some(task) = self.remote_control_task.take() {
            task.abort();
        }
        if local_only {
            match self.client.remote_start(local_only, password).await {
                Ok(r) => self.apply_remote_control_result(local_only, r, false),
                Err(e) => {
                    self.remote_control_popup = Some(RemoteControlPopup::Err {
                        local_only,
                        message: e.to_string(),
                    });
                }
            }
            return;
        }

        match self
            .client
            .remote_start_with_wait(false, password.clone(), false)
            .await
        {
            Ok(r) => self.apply_remote_control_result(false, r, true),
            Err(e) => {
                self.remote_control_popup = Some(RemoteControlPopup::Err {
                    local_only: false,
                    message: e.to_string(),
                });
                return;
            }
        }

        let client = self.client.clone();
        self.remote_control_task = Some(tokio::spawn(async move {
            let result = client.remote_start_with_wait(false, password, true).await;
            (false, result)
        }));
    }

    fn apply_remote_control_result(
        &mut self,
        local_only: bool,
        r: agentd_protocol::RemoteStartResult,
        starting: bool,
    ) {
        let ok = RemoteControlOk {
            url: r.url,
            qr: r.qr,
            tunnel_ready: r.tunnel_ready,
            password: r.password,
            hint: r.hint,
            local_only,
        };
        self.remote_control_popup = Some(if starting {
            RemoteControlPopup::Starting(ok)
        } else {
            RemoteControlPopup::Ok(ok)
        });
    }

    async fn poll_remote_control_task(&mut self) {
        let Some(task) = self.remote_control_task.as_mut() else {
            return;
        };
        let Some(joined) = task.now_or_never() else {
            return;
        };
        self.remote_control_task = None;
        match joined {
            Ok((local_only, Ok(r))) => self.apply_remote_control_result(local_only, r, false),
            Ok((local_only, Err(e))) => {
                self.remote_control_popup = Some(RemoteControlPopup::Err {
                    local_only,
                    message: e.to_string(),
                });
            }
            Err(e) if e.is_cancelled() => {}
            Err(e) => {
                self.remote_control_popup = Some(RemoteControlPopup::Err {
                    local_only: false,
                    message: format!("remote-control task failed: {e}"),
                });
            }
        }
    }

    /// Tear down the remote WS listener + cloudflared tunnel on the
    /// daemon side. Surfaces `was_running` so the user gets
    /// distinct status messages for "we stopped it" vs "nothing
    /// was running to stop". Also auto-dismisses any open
    /// `/remote-control` popup since the URL it shows is now dead.
    pub async fn stop_remote_control(&mut self) {
        if let Some(task) = self.remote_control_task.take() {
            task.abort();
        }
        match self.client.remote_stop().await {
            Ok(r) if r.was_running => {
                self.remote_control_popup = None;
                self.set_status("remote stopped; QR + URL invalidated".into());
            }
            Ok(_) => {
                self.set_status("remote wasn't running".into());
            }
            Err(e) => self.set_status(format!("remote-control stop failed: {e}")),
        }
    }

    /// Open the right minibuffer mode for the user's main "command"
    /// keybind (`M-x` / `C-x x` / click on the prompt). Prefers the
    /// orchestrator panel when an orchestrator session is available;
    /// falls back to the static command palette.
    pub fn open_minibuffer_for_command(&mut self) {
        if self.orchestrator_id.is_some() {
            self.orchestrator_scrollback = 0;
            self.minibuffer = Some(Minibuffer {
                prompt: "> ".to_string(),
                input: String::new(),
                cursor: 0,
                intent: MinibufferIntent::Orchestrator,
                error: None,
            });
        } else {
            self.minibuffer = Some(Minibuffer {
                prompt: "M-x ".to_string(),
                input: String::new(),
                cursor: 0,
                intent: MinibufferIntent::CommandPalette,
                error: None,
            });
        }
    }
}

/// Best-effort one-line summary of a tool call's args JSON for the
/// PTY tool-block header. Prefers a single salient field
/// (`command` for shell, `path` for read_file, `query` for search-
/// style tools, `glob` for globs); otherwise falls back to
/// truncated JSON. Capped at 80 chars so the header stays single-
/// line in the typical pane width.
/// Replay a session's transcript into the local-state snapshots
/// `bootstrap_terminal` rebuilds on subscribe / pin / TUI restart.
///
/// The daemon stores every adapter event in the transcript ring,
/// but its `event.subscribe` notification stream only fires from
/// the moment of subscribe forward. Anything that happened before
/// — a tool block's arguments, a tool result's output, the
/// current editor buffer, the agent's running status — is missed
/// unless we re-derive it from the persisted transcript.
///
/// Pure function over the events slice plus mutable references to
/// the local-state cells so it stays trivially unit-testable.
pub fn apply_transcript_to_local_state(
    events: &[TimestampedEvent],
    history: &mut crate::pty_render::ItemHistory,
    editor_state: &mut Option<EditorState>,
    agent_status: &mut Option<agentd_protocol::AgentStatus>,
) {
    for ev in events {
        match &ev.event {
            // TaskStart is the PRIMARY block-creation event for
            // current zarvis sessions — it carries the explicit
            // `call_id` and the live `on_notification` handler
            // forwards it to `feed_task_start`. Without forwarding
            // it here too, a fresh TUI re-attaching to an existing
            // session sees no `ToolBlock` items in the replayed
            // history (the OSC 7700 backstop only fires for legacy
            // `pty.log` files; current zarvis doesn't write the
            // fences), `has_blocks` is false, and the user can no
            // longer see synthesized tool blocks at all — including
            // when scrolling. See
            // `zarvis_tool_block_visible_after_bootstrap_via_task_start`.
            SessionEvent::TaskStart {
                call_id,
                tool,
                args_summary,
            } => {
                if tool != agentd_protocol::TUI_DISPATCH_TOOL {
                    history.feed_task_start(call_id.clone(), tool.clone(), args_summary.clone());
                }
            }
            SessionEvent::Pty { .. } => {
                if let Some(bytes) = ev.event.pty_bytes() {
                    history.feed_pty(&bytes);
                }
            }
            SessionEvent::ToolUse { tool, args } => {
                // The TUI-dispatch tool (`tui`) is a slash-command
                // short-circuit, not a real tool block — skip it
                // just like the live notification handler does.
                if tool != agentd_protocol::TUI_DISPATCH_TOOL {
                    history.feed_tool_use(tool.clone(), summarize_tool_args(args));
                }
            }
            SessionEvent::ToolResult { tool, ok, output } => {
                history.feed_tool_result(tool, *ok, output.clone());
            }
            // Each new EditorState supersedes the prior one — the
            // adapter emits one on every buffer / cursor / queue /
            // completions change, so the last event in the
            // transcript is the live state at subscribe time. Without
            // this, the TUI shows no editor pane after reconnect
            // until the user types, and the bottom rows of the chat
            // overflow into where the prompt should sit.
            SessionEvent::EditorState {
                queued,
                buf,
                cursor,
                completions,
            } => {
                *editor_state = Some(EditorState {
                    queued: queued.clone(),
                    buf: buf.clone(),
                    cursor: *cursor,
                    completions: completions.clone(),
                });
            }
            // Mirror the live notification handler: `active=true`
            // sets the running status; `active=false` clears it and
            // appends the dim completion line into the local PTY
            // history. The adapter only emits this as a structured
            // event, so a fresh TUI must synthesize the same history
            // row during transcript replay or the completed-turn
            // message disappears after reconnect/restart.
            SessionEvent::AgentStatus(status) => {
                if status.active {
                    *agent_status = Some(status.clone());
                } else {
                    *agent_status = None;
                    if let Some(bytes) = agent_status_history_line(status) {
                        history.feed_pty(&bytes);
                    }
                }
            }
            _ => {}
        }
    }
}

pub fn summarize_tool_args(args: &serde_json::Value) -> String {
    if let Some(obj) = args.as_object() {
        for key in ["command", "path", "query", "glob", "pattern", "cwd"] {
            if let Some(v) = obj.get(key) {
                if let Some(s) = v.as_str() {
                    return s.chars().take(80).collect();
                }
            }
        }
    }
    let s = args.to_string();
    s.chars().take(80).collect()
}

pub fn short_id(id: &str) -> &str {
    let n = id.len().min(10);
    &id[..n]
}

fn normalized_points(a: ScreenPoint, b: ScreenPoint) -> (ScreenPoint, ScreenPoint) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

fn slice_line(line: &str, start_col: u16, end_col: u16) -> String {
    if end_col < start_col {
        return String::new();
    }
    line.chars()
        .skip(start_col as usize)
        .take((end_col - start_col + 1) as usize)
        .collect()
}

fn url_hit_in_frame(
    frame_text: &[String],
    col: u16,
    row: u16,
    bounds: ratatui::layout::Rect,
) -> Option<UrlHit> {
    let (text, positions) = wrapped_text_with_positions(frame_text, bounds);
    let idx = positions
        .iter()
        .position(|p| p.col == col && p.row == row)?;
    let (start, end, url) = url_range_at_col(&text, idx)?;
    let ranges = url_line_ranges(&positions[start..end]);
    Some(UrlHit { url, ranges })
}

fn wrapped_text_with_positions(
    frame_text: &[String],
    bounds: ratatui::layout::Rect,
) -> (String, Vec<ScreenPoint>) {
    let mut text = String::new();
    let mut positions = Vec::new();
    for row in bounds.top()..bounds.bottom() {
        let Some(line) = frame_text.get(row as usize) else {
            continue;
        };
        for col in bounds.left()..bounds.right() {
            let ch = line.chars().nth(col as usize).unwrap_or(' ');
            text.push(ch);
            positions.push(ScreenPoint { col, row });
        }
    }
    (text, positions)
}

fn url_line_ranges(positions: &[ScreenPoint]) -> Vec<UrlLineHit> {
    let mut ranges = Vec::new();
    let Some(first) = positions.first().copied() else {
        return ranges;
    };
    let mut row = first.row;
    let mut start_col = first.col;
    let mut last_col = first.col;
    for p in positions.iter().copied().skip(1) {
        if p.row == row && p.col == last_col.saturating_add(1) {
            last_col = p.col;
            continue;
        }
        ranges.push(UrlLineHit {
            row,
            start_col,
            end_col: last_col.saturating_add(1),
        });
        row = p.row;
        start_col = p.col;
        last_col = p.col;
    }
    ranges.push(UrlLineHit {
        row,
        start_col,
        end_col: last_col.saturating_add(1),
    });
    ranges
}

fn url_range_at_col(line: &str, col: usize) -> Option<(usize, usize, String)> {
    for (start, end) in url_ranges(line) {
        if col >= start && col < end {
            return Some((
                start,
                end,
                line.chars().skip(start).take(end - start).collect(),
            ));
        }
    }
    None
}

fn url_ranges(line: &str) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let mut ranges = Vec::new();
    let mut i = 0usize;
    while i + 2 < chars.len() {
        if chars[i] == ':' && chars[i + 1] == '/' && chars[i + 2] == '/' {
            let Some(start) = scheme_start(&chars, i) else {
                i += 3;
                continue;
            };
            let mut end = i + 3;
            while end < chars.len() && is_url_body_char(chars[end]) {
                end += 1;
            }
            while end > start && is_trailing_url_punct(chars[end - 1]) {
                end -= 1;
            }
            if end > i + 3 {
                ranges.push((start, end));
            }
            i = end.max(i + 3);
        } else {
            i += 1;
        }
    }
    ranges
}

fn scheme_start(chars: &[char], colon: usize) -> Option<usize> {
    if colon == 0 {
        return None;
    }
    let mut start = colon;
    while start > 0 && is_scheme_char(chars[start - 1]) {
        start -= 1;
    }
    if start == colon || !chars[start].is_ascii_alphabetic() {
        return None;
    }
    Some(start)
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

fn is_url_body_char(c: char) -> bool {
    !c.is_whitespace() && !matches!(c, '"' | '\'' | '`' | '<' | '>')
}

fn is_trailing_url_punct(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
}

fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn opener for {url}"))?;
    Ok(())
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    if copy_with_pbcopy(text).is_ok() {
        return Ok(());
    }
    copy_with_osc52(text)
}

fn copy_with_pbcopy(text: &str) -> Result<()> {
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(text.as_bytes())?;
    }
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        anyhow::bail!("pbcopy exited with {status}")
    }
}

fn copy_with_osc52(text: &str) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let mut stdout = std::io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}

fn byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn test_layout() -> LayoutSnapshot {
        LayoutSnapshot {
            list_area: Some(Rect::new(0, 0, 20, 10)),
            view_area: Some(Rect::new(20, 0, 80, 20)),
            pin_strip_area: Some(Rect::new(20, 20, 80, 8)),
            matrix_rain_area: None,
            minibuffer_area: Some(Rect::new(0, 29, 100, 4)),
            list_row_count: 0,
            list_items_area: None,
            list_scroll_offset: 0,
            minibuffer_hints: Vec::new(),
            minibuffer_harness_hits: Vec::new(),
            modal_area: None,
        }
    }

    fn test_app(client: Arc<Client>, sessions: Vec<SessionSummary>) -> App {
        let now = Instant::now();
        let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());
        App {
            client,
            sessions,
            groups: Vec::new(),
            selection: Selection::Session("s1".into()),
            focus: PaneFocus::View,
            transcript: Vec::new(),
            transcript_session: None,
            transcript_scroll: 0,
            minibuffer: None,
            harnesses: Vec::new(),
            theme: crate::theme::Theme::default(),
            help_visible: false,
            profile: Profile::Emacs,
            keymap: keymap::default_for(Profile::Emacs),
            chord_state: ChordState::default(),
            chord_label: String::new(),
            status: None,
            last_diff: None,
            should_quit: false,
            connected: true,
            remote_clients: 0,
            view: ViewMode::Terminal,
            histories: HashMap::new(),
            block_hits: HashMap::new(),
            orchestrator_desired_size: None,
            terminal_pane_size: (80, 24),
            zoom: ZoomMode::None,
            list_scroll_offset: 0,
            view_scrollback: 0,
            orchestrator_scrollback: 0,
            orchestrator_panel_h: None,
            resizing_orchestrator_panel: None,
            pty_activity: HashMap::new(),
            start_instant: now,
            layout: LayoutSnapshot::default(),
            mouse_pos: None,
            mouse_capture_enabled: true,
            orchestrator_id: None,
            list_panel_w: LIST_PANEL_W_DEFAULT,
            resizing_list: None,
            pin_strip_h: None,
            resizing_pin_strip: None,
            matrix_rain_h: None,
            resizing_matrix_rain: None,
            list_collapsed: false,
            tasks_popup: None,
            remote_control_popup: None,
            remote_control_task: None,
            editor_states: HashMap::new(),
            agent_statuses: HashMap::new(),
            session_transition: None,
            pin_transitions: HashMap::new(),
            matrix_rain: crate::matrix_rain::MatrixRain::default(),
            matrix_rain_intensity: 0.0,
            matrix_rain_intensity_updated_at: now,
            matrix_rain_foreground_epoch: now,
            matrix_rain_active_drops: HashMap::new(),
            matrix_rain_hidden: false,
            hide_pane_side_borders: false,
            frame_text: Vec::new(),
            text_selection: None,
            selected_text: None,
            selected_text_bounds: None,
            selected_text_range: None,
            pty_input_tx,
            pty_input_errors,
        }
    }

    #[test]
    fn selection_bounds_use_list_inner_area() {
        let bounds = selection_bounds_for_layout(&test_layout(), 0, false, 1, 1);

        assert_eq!(bounds, Some(Rect::new(1, 1, 18, 8)));
        assert_eq!(
            selection_bounds_for_layout(&test_layout(), 0, false, 0, 1),
            None
        );
    }

    fn summary_with_kind(kind: agentd_protocol::SessionKind) -> SessionSummary {
        SessionSummary {
            id: "s1".into(),
            harness: "shell".into(),
            cwd: "/tmp".into(),
            title: None,
            state: agentd_protocol::SessionState::Running,
            created_at: chrono::Utc::now(),
            last_event_at: None,
            cost_usd: None,
            model: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 0,
            has_pty: false,
            mode: None,
            pinned: false,
            position: 0,
            group_id: None,
            last_pty_at_ms: None,
            automode: false,
            kind,
        }
    }

    #[tokio::test]
    async fn disconnected_q_quits_even_when_pty_would_capture_keys() {
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });

        let client = Client::connect(&sock).await.expect("client connects");
        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.has_pty = true;
        let mut app = test_app(client, vec![summary]);
        app.connected = false;

        app.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .await;

        assert!(app.should_quit);
        server.abort();
    }

    // Regression: switching to a session showed "(no PTY history yet)"
    // even though history existed, until a keystroke recreated the entry
    // from a live PTY event. Root cause: once the transcript was
    // hydrated, the trigger never re-fired even if the history entry was
    // later dropped (e.g. a `SessionEvent::Reset`, or a reconnecting
    // adapter making the first fetch skip history). Hydration must
    // self-heal whenever the Terminal view has no history.
    #[tokio::test]
    async fn terminal_view_rehydrates_after_history_dropped() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.has_pty = true;
        let mut app = test_app(client, vec![summary]);
        app.view = ViewMode::Terminal;

        // Simulate a completed hydration: transcript loaded + history present.
        app.transcript_session = Some("s1".into());
        app.histories
            .insert("s1".into(), crate::pty_render::ItemHistory::new());
        assert!(
            !app.selected_needs_hydration(),
            "fully hydrated terminal session must not re-fetch"
        );

        // A Reset event drops the history while the session stays selected
        // (transcript_session stays == "s1", so the old transcript-only
        // trigger would never re-fire).
        let reset = agentd_protocol::Notification {
            jsonrpc: "2.0".into(),
            method: agentd_protocol::ipc_notif::EVENT.into(),
            params: Some(
                serde_json::to_value(agentd_protocol::EventNotificationPayload {
                    session_id: "s1".into(),
                    at: chrono::Utc::now(),
                    event: agentd_protocol::SessionEvent::Reset,
                    seq: 1,
                })
                .unwrap(),
            ),
        };
        app.on_notification(reset).await;

        assert!(
            !app.histories.contains_key("s1"),
            "Reset removes the history entry"
        );
        assert!(
            app.selected_needs_hydration(),
            "dropped history on a selected Terminal session must re-trigger hydration"
        );
        assert!(
            app.selected_hydration_request().unwrap().needs_history,
            "the re-hydration request must actually fetch the PTY snapshot"
        );

        // In Transcript view a missing history must NOT spin up fetches.
        app.view = ViewMode::Transcript;
        assert!(
            !app.selected_needs_hydration(),
            "transcript view should not re-fetch PTY history"
        );

        server.abort();
    }

    #[test]
    fn only_user_sessions_are_visible_list_items() {
        assert!(is_list_visible_session(&summary_with_kind(
            agentd_protocol::SessionKind::User
        )));
        assert!(!is_list_visible_session(&summary_with_kind(
            agentd_protocol::SessionKind::Orchestrator
        )));
        assert!(!is_list_visible_session(&summary_with_kind(
            agentd_protocol::SessionKind::Subagent
        )));
    }

    #[tokio::test]
    async fn pty_typing_does_not_wait_for_input_rpc_response() {
        use agentd_client::Client;
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use std::sync::Arc;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;
        use tokio::sync::{mpsc, Notify};

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let release_input = Arc::new(Notify::new());
        let (input_seen_tx, mut input_seen_rx) = mpsc::unbounded_channel();
        let release_for_server = release_input.clone();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let Ok(n) = reader.read_line(&mut line).await else {
                    break;
                };
                if n == 0 {
                    break;
                }
                let req: Value = serde_json::from_str(&line).expect("json request");
                let id = req.get("id").cloned().unwrap_or(Value::Null);
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                if method == ipc_method::SESSION_PTY_INPUT {
                    let _ = input_seen_tx.send(());
                    release_for_server.notified().await;
                }
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": Value::Null,
                });
                if writer
                    .write_all((resp.to_string() + "\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let client = Client::connect(&sock).await.expect("client connects");
        let mut app = test_app(
            client,
            vec![summary_with_kind(agentd_protocol::SessionKind::User)],
        );
        app.sessions[0].has_pty = true;

        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            app.on_term_event(CtEvent::Key(KeyEvent::new(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
            ))),
        )
        .await
        .expect("typing should queue PTY input without waiting for daemon response");

        tokio::time::timeout(std::time::Duration::from_secs(1), input_seen_rx.recv())
            .await
            .expect("mock daemon should receive queued PTY input")
            .expect("pty input seen");
        release_input.notify_waiters();
        server.abort();
    }

    // Typing into a zarvis prompt grows the editor pane, shrinking the
    // chat area. The chat parser must stay at the full pane height so
    // editor growth never resizes (and O(history)-rebuilds) it — that
    // rebuild-per-keystroke was the typing lag. Structural, timing-free.
    #[tokio::test]
    async fn zarvis_editor_growth_does_not_resize_chat_parser() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.harness = "zarvis".into();
        summary.has_pty = true;
        let mut app = test_app(client, vec![summary]);
        app.view = ViewMode::Terminal;
        app.focus = PaneFocus::View;

        let mut h = crate::pty_render::ItemHistory::new();
        for i in 0..40u32 {
            h.feed_pty(format!("\x1b[33mchat line {i}\x1b[0m\r\n").as_bytes());
            let call = format!("c{i}");
            h.feed_tool_use("shell".into(), format!("cmd {i}"));
            h.feed_pty(
                format!("\x1b]7700;open;call={call}\x07o\x1b]7700;close;call={call}\x07")
                    .as_bytes(),
            );
            h.feed_tool_result(&call, true, "ok".into());
        }
        app.histories.insert("s1".into(), h);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        let render_with_buf = |app: &mut App, terminal: &mut ratatui::Terminal<_>, buf: &str| {
            app.editor_states.insert(
                "s1".into(),
                EditorState {
                    queued: Vec::new(),
                    buf: buf.to_string(),
                    cursor: buf.len(),
                    completions: Vec::new(),
                },
            );
            terminal.draw(|f| crate::ui::render(f, app)).expect("draw");
            app.histories.get("s1").unwrap().cached_dims()
        };

        // Short prompt: editor pane is 1-2 rows.
        let small = render_with_buf(&mut app, &mut terminal, "hi");
        // Long multi-line prompt: editor pane grows several rows, so the
        // chat area shrinks. The parser dims must be unchanged.
        let big = render_with_buf(
            &mut app,
            &mut terminal,
            "line one\nline two\nline three\nline four\nline five\nline six",
        );

        assert!(small.is_some() && small == big,
            "editor growth resized the chat parser: {small:?} -> {big:?}");

        // And the shrunk chat must still show the MOST RECENT content
        // (the bottom slice), not stale/older lines.
        let buf = terminal.backend().buffer();
        let mut screen = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                screen.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            screen.push('\n');
        }
        assert!(
            screen.contains("cmd 39"),
            "shrunk chat lost the most-recent content; got:\n{screen}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn background_hydration_does_not_block_primary_client_rpc() {
        use agentd_client::Client;
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use std::sync::Arc;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;
        use tokio::sync::{mpsc, Notify};

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("agentd.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let release_transcript = Arc::new(Notify::new());
        let (transcript_seen_tx, mut transcript_seen_rx) = mpsc::unbounded_channel();
        let release_for_server = release_transcript.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let release = release_for_server.clone();
                let transcript_seen_tx = transcript_seen_tx.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut reader = BufReader::new(reader);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        let Ok(n) = reader.read_line(&mut line).await else {
                            break;
                        };
                        if n == 0 {
                            break;
                        }
                        let Ok(req) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let result = match method {
                            ipc_method::PING => {
                                serde_json::json!({"pong": true, "version": "test"})
                            }
                            ipc_method::SESSION_TRANSCRIPT => {
                                let _ = transcript_seen_tx.send(());
                                release.notified().await;
                                let events: Vec<Value> = (0..2_000)
                                    .map(|i| {
                                        serde_json::json!({
                                            "seq": i + 1,
                                            "at": "2026-05-21T00:00:00Z",
                                            "event": {
                                                "type": "pty",
                                                "data": base64::Engine::encode(
                                                    &base64::engine::general_purpose::STANDARD,
                                                    format!("line {i}\r\n")
                                                )
                                            }
                                        })
                                    })
                                    .collect();
                                serde_json::json!({"events": events, "total": 2_000})
                            }
                            ipc_method::SESSION_PTY_REPLAY => {
                                serde_json::json!({"data": "", "size": {"cols": 80, "rows": 24}})
                            }
                            _ => Value::Null,
                        };
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": result,
                        });
                        if writer
                            .write_all((resp.to_string() + "\n").as_bytes())
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
            }
        });

        let client = Client::connect(&sock)
            .await
            .expect("primary client connects");
        let hydration = tokio::spawn(load_session_hydration(SessionHydrationRequest {
            socket: sock.clone(),
            session_id: "s-big".to_string(),
            needs_history: true,
            terminal_pane_size: (80, 24),
        }));

        tokio::time::timeout(std::time::Duration::from_secs(1), transcript_seen_rx.recv())
            .await
            .expect("hydration transcript request should reach mock daemon")
            .expect("transcript request marker");

        let ping = tokio::time::timeout(std::time::Duration::from_millis(100), client.ping())
            .await
            .expect("primary client RPC should not wait for hydration transcript")
            .expect("ping should succeed");
        assert!(ping.pong);

        release_transcript.notify_waiters();
        let loaded = tokio::time::timeout(std::time::Duration::from_secs(2), hydration)
            .await
            .expect("hydration should finish")
            .expect("hydration task should join")
            .expect("hydration should succeed");
        assert_eq!(loaded.session_id, "s-big");
        assert_eq!(loaded.transcript.len(), 2_000);

        server.abort();
    }

    /// REGRESSION: a TUI re-attaching to an existing zarvis session
    /// shows the tool blocks again. Current zarvis interactive
    /// adapters never write OSC 7700 fences to the PTY (the helpers
    /// `tool_block_open` / `tool_block_close` exist but no call
    /// site remains); tool blocks are communicated entirely via
    /// `SessionEvent::TaskStart` (carrying `call_id`) followed by
    /// `ToolUse` and `ToolResult`. `apply_transcript_to_local_state`
    /// must forward `TaskStart` to `feed_task_start` or no
    /// `ToolBlock` items exist after bootstrap and the user sees
    /// raw chat with no synthesized blocks at any scroll position.
    #[test]
    fn task_start_in_transcript_creates_tool_block() {
        use agentd_protocol::{AgentStatus, SessionEvent, TimestampedEvent};
        use chrono::Utc;
        fn ev(seq: u64, event: SessionEvent) -> TimestampedEvent {
            TimestampedEvent {
                seq,
                at: Utc::now(),
                event,
            }
        }
        let events = vec![
            ev(
                1,
                SessionEvent::TaskStart {
                    call_id: "t1".into(),
                    tool: "shell".into(),
                    args_summary: "ls -la".into(),
                },
            ),
            ev(
                2,
                SessionEvent::ToolResult {
                    tool: "t1".into(),
                    ok: true,
                    output: "out".into(),
                },
            ),
        ];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor: Option<EditorState> = None;
        let mut status: Option<AgentStatus> = None;
        apply_transcript_to_local_state(&events, &mut history, &mut editor, &mut status);

        // The render must include the synthesized header for the
        // block. Before the fix, no `ToolBlock` items existed and
        // the renderer fell through to `replay_cached` — no header.
        let screen_rows = 24u16;
        let screen_cols = 80u16;
        let out = history.replay(screen_cols, screen_rows, 0);
        let text: String = (0..screen_rows)
            .flat_map(|r| {
                let mut row = String::new();
                for c in 0..screen_cols {
                    if let Some(cell) = out.screen.cell(r, c) {
                        row.push_str(&cell.contents());
                    }
                }
                row.push('\n');
                row.chars().collect::<Vec<_>>()
            })
            .collect();
        assert!(
            text.contains("→ shell"),
            "TaskStart must be forwarded to ItemHistory::feed_task_start. \
             Without it, fresh-TUI bootstrap of an existing zarvis session \
             rebuilds history with no tool blocks. Got render:\n{text}",
        );
    }

    #[test]
    fn transcript_replay_preserves_answer_after_tool_order() {
        use agentd_protocol::{SessionEvent, TimestampedEvent};
        use chrono::Utc;

        fn ev(seq: u64, event: SessionEvent) -> TimestampedEvent {
            TimestampedEvent {
                seq,
                at: Utc::now(),
                event,
            }
        }

        let events = vec![
            ev(1, SessionEvent::pty(b"before tool\r\n")),
            ev(
                2,
                SessionEvent::TaskStart {
                    call_id: "t1".into(),
                    tool: "shell".into(),
                    args_summary: "echo hi".into(),
                },
            ),
            ev(
                3,
                SessionEvent::ToolResult {
                    tool: "t1".into(),
                    ok: true,
                    output: "hi".into(),
                },
            ),
            ev(4, SessionEvent::pty(b"after tool answer\r\n")),
        ];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor: Option<EditorState> = None;
        let mut status = None;
        apply_transcript_to_local_state(&events, &mut history, &mut editor, &mut status);

        let screen_rows = 24u16;
        let screen_cols = 80u16;
        let out = history.replay(screen_cols, screen_rows, 0);
        let text: String = (0..screen_rows)
            .flat_map(|r| {
                let mut row = String::new();
                for c in 0..screen_cols {
                    if let Some(cell) = out.screen.cell(r, c) {
                        row.push_str(&cell.contents());
                    }
                }
                row.push('\n');
                row.chars().collect::<Vec<_>>()
            })
            .collect();

        let tool_idx = text
            .find("→ shell")
            .expect("transcript replay should synthesize the tool block");
        let answer_idx = text
            .find("after tool answer")
            .expect("transcript replay should restore assistant PTY bytes after the tool");
        assert!(
            answer_idx > tool_idx,
            "assistant answer emitted after a tool call must replay after the tool block. got:\n{text}"
        );
    }

    #[test]
    fn transcript_replay_restores_completed_turn_status_line() {
        use agentd_protocol::{AgentStatus, SessionEvent, TimestampedEvent};
        use chrono::Utc;

        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
            .saturating_sub(2_000);
        let events = vec![TimestampedEvent {
            seq: 1,
            at: Utc::now(),
            event: SessionEvent::AgentStatus(AgentStatus {
                active: false,
                started_at_ms,
                status: "Finished".into(),
            }),
        }];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor: Option<EditorState> = None;
        let mut status: Option<AgentStatus> = Some(AgentStatus {
            active: true,
            started_at_ms,
            status: "Working".into(),
        });
        apply_transcript_to_local_state(&events, &mut history, &mut editor, &mut status);

        assert!(
            status.is_none(),
            "inactive AgentStatus should clear any live running status on bootstrap"
        );

        let screen_rows = 24u16;
        let screen_cols = 80u16;
        let out = history.replay(screen_cols, screen_rows, 0);
        let text: String = (0..screen_rows)
            .flat_map(|r| {
                let mut row = String::new();
                for c in 0..screen_cols {
                    if let Some(cell) = out.screen.cell(r, c) {
                        row.push_str(&cell.contents());
                    }
                }
                row.push('\n');
                row.chars().collect::<Vec<_>>()
            })
            .collect();
        assert!(
            text.contains("* Finished"),
            "bootstrap transcript replay must restore the completed-turn history line. got:\n{text}"
        );
    }

    #[test]
    fn selection_bounds_use_view_inner_area() {
        let bounds = selection_bounds_for_layout(&test_layout(), 0, false, 21, 1);

        assert_eq!(bounds, Some(Rect::new(21, 1, 78, 18)));
        assert_eq!(
            selection_bounds_for_layout(&test_layout(), 0, false, 20, 1),
            None
        );
    }

    #[test]
    fn selection_bounds_use_pinned_tile_inner_area() {
        let bounds = selection_bounds_for_layout(&test_layout(), 2, false, 21, 21);

        assert_eq!(bounds, Some(Rect::new(21, 21, 38, 6)));
        assert_eq!(
            selection_bounds_for_layout(&test_layout(), 2, false, 20, 21),
            None
        );
    }

    #[test]
    fn selection_bounds_use_minibuffer_line_for_god_area() {
        let bounds = selection_bounds_for_layout(&test_layout(), 0, false, 0, 29);

        assert_eq!(bounds, Some(Rect::new(0, 29, 100, 4)));
    }

    #[test]
    fn selection_bounds_exclude_orchestrator_panel_top_border() {
        assert_eq!(
            selection_bounds_for_layout(&test_layout(), 0, true, 0, 29),
            None
        );
        assert_eq!(
            selection_bounds_for_layout(&test_layout(), 0, true, 0, 30),
            Some(Rect::new(0, 30, 100, 3))
        );
    }

    /// Symptom-level repro for the zarvis-prompt-overlap bug.
    ///
    /// User report (against `tui reconnect`, harness=zarvis): after
    /// the TUI reconnects or otherwise rebootstraps a session, the
    /// last turn's output extends all the way to the bottom row,
    /// overwriting the position where the `❯ ` prompt should sit.
    /// Typing anything (which triggers a fresh `EditorState` event
    /// from the adapter) shrinks the chat area by ~3 rows and the
    /// prompt finally appears.
    ///
    /// Root cause: `bootstrap_terminal` replays the transcript but
    /// only feeds `ToolUse` / `ToolResult` back into the local
    /// state. `EditorState` events are dropped, so `editor_states`
    /// stays empty and `render_terminal` doesn't reserve the
    /// bottom editor pane. Replaying the latest `EditorState` (and
    /// `AgentStatus`) from the transcript fixes it.
    #[test]
    fn apply_transcript_replays_latest_editor_state_for_bootstrap() {
        use agentd_protocol::SessionEvent;
        use chrono::TimeZone;

        fn ev(seq: u64, e: SessionEvent) -> TimestampedEvent {
            TimestampedEvent {
                seq,
                at: chrono::Utc.timestamp_opt(0, 0).unwrap(),
                event: e,
            }
        }

        let events = vec![
            ev(
                1,
                SessionEvent::EditorState {
                    queued: Vec::new(),
                    buf: "stale".into(),
                    cursor: 5,
                    completions: Vec::new(),
                },
            ),
            ev(
                2,
                SessionEvent::ToolUse {
                    tool: "shell".into(),
                    args: serde_json::json!({"command": "ls"}),
                },
            ),
            // The most recent EditorState — this is what the TUI
            // must surface on reconnect so the prompt is visible
            // before the user touches the keyboard.
            ev(
                3,
                SessionEvent::EditorState {
                    queued: vec!["queued msg".into()],
                    buf: "latest".into(),
                    cursor: 6,
                    completions: vec!["/help".into()],
                },
            ),
        ];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor_state: Option<EditorState> = None;
        let mut agent_status: Option<agentd_protocol::AgentStatus> = None;
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor_state,
            &mut agent_status,
        );

        let state = editor_state
            .expect("bootstrap must replay the most recent EditorState so the prompt is visible");
        assert_eq!(state.buf, "latest");
        assert_eq!(state.cursor, 6);
        assert_eq!(state.queued, vec!["queued msg".to_string()]);
        assert_eq!(state.completions, vec!["/help".to_string()]);
    }

    #[test]
    fn adjusted_scrollback_clamps_to_live_and_max() {
        assert_eq!(adjusted_scrollback(0, -10), 0);
        assert_eq!(adjusted_scrollback(5, -3), 2);
        assert_eq!(adjusted_scrollback(5, 3), 8);
        assert_eq!(adjusted_scrollback(SCROLLBACK_MAX - 1, 10), SCROLLBACK_MAX);
    }

    #[test]
    fn adjusted_list_scroll_offset_clamps_to_visible_range() {
        assert_eq!(adjusted_list_scroll_offset(0, 3, 10, 4), 3);
        assert_eq!(adjusted_list_scroll_offset(3, -1, 10, 4), 2);
        assert_eq!(adjusted_list_scroll_offset(0, -99, 10, 4), 0);
        assert_eq!(adjusted_list_scroll_offset(0, 99, 10, 4), 6);
        assert_eq!(adjusted_list_scroll_offset(9, 0, 10, 4), 6);
        assert_eq!(adjusted_list_scroll_offset(2, 1, 3, 4), 0);
    }
}

fn adjusted_scrollback(current: usize, delta: i32) -> usize {
    let next = current as i32 + delta;
    next.max(0).min(SCROLLBACK_MAX as i32) as usize
}

fn adjusted_list_scroll_offset(
    current: usize,
    delta: i32,
    item_count: usize,
    visible_rows: usize,
) -> usize {
    let max_scroll = item_count.saturating_sub(visible_rows);
    adjusted_scrollback(current, delta).min(max_scroll)
}

fn delete_back_char(mb: &mut Minibuffer) {
    if mb.cursor > 0 {
        let prev = mb.cursor - 1;
        let pos = byte_pos(&mb.input, prev);
        mb.input.remove(pos);
        mb.cursor = prev;
        mb.error = None;
    }
}

fn delete_forward_char(mb: &mut Minibuffer) {
    if mb.cursor < mb.input.chars().count() {
        let pos = byte_pos(&mb.input, mb.cursor);
        mb.input.remove(pos);
        mb.error = None;
    }
}

fn word_back(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut c = cursor.min(chars.len());
    while c > 0 && !chars[c - 1].is_alphanumeric() {
        c -= 1;
    }
    while c > 0 && chars[c - 1].is_alphanumeric() {
        c -= 1;
    }
    c
}

fn word_forward(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut c = cursor.min(chars.len());
    while c < chars.len() && !chars[c].is_alphanumeric() {
        c += 1;
    }
    while c < chars.len() && chars[c].is_alphanumeric() {
        c += 1;
    }
    c
}

fn kill_word_back(mb: &mut Minibuffer) {
    let start = word_back(&mb.input, mb.cursor);
    let start_b = byte_pos(&mb.input, start);
    let end_b = byte_pos(&mb.input, mb.cursor);
    mb.input.drain(start_b..end_b);
    mb.cursor = start;
    mb.error = None;
}

fn kill_word_forward(mb: &mut Minibuffer) {
    let end = word_forward(&mb.input, mb.cursor);
    let start_b = byte_pos(&mb.input, mb.cursor);
    let end_b = byte_pos(&mb.input, end);
    mb.input.drain(start_b..end_b);
    mb.error = None;
}

/// Bash-style Tab completion for the harness-picker minibuffer. Completes
/// to the longest common prefix of all matches; sets an inline hint listing
/// the candidates when the result is ambiguous.
fn apply_harness_completion(mb: &mut Minibuffer, options: &[String]) {
    let current = mb.input.clone();
    let matches: Vec<&String> = options.iter().filter(|o| o.starts_with(&current)).collect();
    if matches.is_empty() {
        mb.error = if options.is_empty() {
            Some("(no harnesses available)".to_string())
        } else {
            Some(format!("no match for {current}"))
        };
        return;
    }
    if matches.len() == 1 {
        mb.input = matches[0].clone();
        mb.cursor = mb.input.chars().count();
        mb.error = None;
        return;
    }
    let prefix = longest_common_prefix(&matches);
    if prefix.len() > mb.input.len() {
        mb.input = prefix;
        mb.cursor = mb.input.chars().count();
    }
    let listed: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
    mb.error = Some(format!("matches: {}", listed.join(", ")));
}

fn longest_common_prefix(strs: &[&String]) -> String {
    let mut out = String::new();
    let Some(first) = strs.first() else {
        return out;
    };
    'outer: for (i, c) in first.chars().enumerate() {
        for s in &strs[1..] {
            if s.chars().nth(i) != Some(c) {
                break 'outer;
            }
        }
        out.push(c);
    }
    out
}

/// Translate a crossterm `KeyEvent` into the raw byte sequence a PTY would
/// receive from a real terminal. Returns `None` for keys we don't have a
/// canonical encoding for (e.g. function keys we don't ship a mapping for).
fn encode_key_to_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let lower = c.to_ascii_lowercase();
                let byte = if lower.is_ascii_alphabetic() {
                    (lower as u8) - b'a' + 1
                } else {
                    match c {
                        ' ' | '@' => 0x00,
                        '[' => 0x1b,
                        '\\' => 0x1c,
                        ']' => 0x1d,
                        '^' => 0x1e,
                        '_' | '?' => 0x1f,
                        _ => return None,
                    }
                };
                Some(vec![byte])
            } else if alt {
                let mut out = vec![0x1b];
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                Some(out)
            } else {
                let mut buf = [0u8; 4];
                Some(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            Some(vec![b'\n'])
        }
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(b"\x1b[Z".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::F(n) => {
            let s: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => return None,
            };
            Some(s.to_vec())
        }
        _ => None,
    }
}

fn should_autofocus_view_from_list(
    focus: PaneFocus,
    zoom: ZoomMode,
    chord_is_empty: bool,
    key: KeyEvent,
) -> bool {
    if focus != PaneFocus::List || !matches!(zoom, ZoomMode::None) {
        return false;
    }
    if !chord_is_empty {
        return false;
    }
    if key
        .modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return false;
    }
    matches!(key.code, KeyCode::Char(c) if c.is_ascii_alphabetic())
}

/// True when the just-handled input event should trigger the
/// drag-coalesce drain (which calls `now_or_never` on the input
/// stream, briefly poisoning crossterm's wake task). Only left-button
/// drags qualify; gating like this keeps typing — and every other
/// event — off the noop-waker path. See the comment at the drain
/// call-site in `run_loop` for the full failure mode.
fn should_drain_after(ev: &CtEvent) -> bool {
    matches!(
        ev,
        CtEvent::Mouse(m)
            if matches!(
                m.kind,
                MouseEventKind::Drag(crossterm::event::MouseButton::Left)
            )
    )
}

#[cfg(test)]
mod drain_gate_tests {
    use super::{
        should_autofocus_view_from_list, should_drain_after, url_range_at_col, url_ranges,
        PaneFocus, ZoomMode,
    };
    use crossterm::event::{
        Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
        MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;

    fn mouse(kind: MouseEventKind) -> CtEvent {
        CtEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::empty(),
        })
    }

    fn key(code: KeyCode) -> CtEvent {
        CtEvent::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        })
    }

    /// Regression for the typing-lag bug introduced by #24 and fixed
    /// here: keystrokes must NOT trigger the drag-coalesce drain. If
    /// this returns true for `Key('a')`, every keystroke calls
    /// `now_or_never` on the EventStream and poisons crossterm's wake
    /// task with a noop waker — subsequent keystrokes sit in the
    /// buffer until the next tick (~120 ms).
    #[test]
    fn typing_does_not_trigger_drain() {
        assert!(!should_drain_after(&key(KeyCode::Char('a'))));
        assert!(!should_drain_after(&key(KeyCode::Char('Z'))));
        assert!(!should_drain_after(&key(KeyCode::Enter)));
        assert!(!should_drain_after(&key(KeyCode::Esc)));
        assert!(!should_drain_after(&key(KeyCode::Backspace)));
    }

    fn autofocus_key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::empty(),
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    fn autofocus_key_with_modifiers(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn list_focus_plain_letters_autofocus_view_only_when_unzoomed() {
        assert!(should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key(KeyCode::Char('a')),
        ));
        assert!(should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key(KeyCode::Char('Z')),
        ));

        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::List,
            true,
            autofocus_key(KeyCode::Char('a')),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::View,
            true,
            autofocus_key(KeyCode::Char('a')),
        ));
    }

    #[test]
    fn list_focus_autofocus_ignores_shortcuts_chords_and_non_letters() {
        assert!(!should_autofocus_view_from_list(
            PaneFocus::View,
            ZoomMode::None,
            true,
            autofocus_key(KeyCode::Char('a')),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            false,
            autofocus_key(KeyCode::Char('a')),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key_with_modifiers(KeyCode::Char('a'), KeyModifiers::CONTROL),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key_with_modifiers(KeyCode::Char('a'), KeyModifiers::ALT),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key(KeyCode::Char('1')),
        ));
        assert!(!should_autofocus_view_from_list(
            PaneFocus::List,
            ZoomMode::None,
            true,
            autofocus_key(KeyCode::Enter),
        ));
    }

    #[test]
    fn left_drag_triggers_drain() {
        assert!(should_drain_after(&mouse(MouseEventKind::Drag(
            MouseButton::Left
        ))));
    }

    /// Other mouse events — including motion, scroll, clicks, and
    /// non-left drags — should not trigger the drain. They go through
    /// the normal one-event-per-render path.
    #[test]
    fn other_mouse_events_do_not_trigger_drain() {
        assert!(!should_drain_after(&mouse(MouseEventKind::Moved)));
        assert!(!should_drain_after(&mouse(MouseEventKind::ScrollUp)));
        assert!(!should_drain_after(&mouse(MouseEventKind::ScrollDown)));
        assert!(!should_drain_after(&mouse(MouseEventKind::Down(
            MouseButton::Left
        ))));
        assert!(!should_drain_after(&mouse(MouseEventKind::Up(
            MouseButton::Left
        ))));
        assert!(!should_drain_after(&mouse(MouseEventKind::Drag(
            MouseButton::Right
        ))));
        assert!(!should_drain_after(&mouse(MouseEventKind::Drag(
            MouseButton::Middle
        ))));
    }

    #[test]
    fn resize_and_paste_do_not_trigger_drain() {
        assert!(!should_drain_after(&CtEvent::Resize(120, 40)));
        assert!(!should_drain_after(&CtEvent::Paste(String::from("hi"))));
        assert!(!should_drain_after(&CtEvent::FocusGained));
        assert!(!should_drain_after(&CtEvent::FocusLost));
    }

    #[test]
    fn url_ranges_find_scheme_urls_and_trim_sentence_punctuation() {
        let line = "see https://example.com/path?q=1, then file:///tmp/a.txt.";
        let ranges = url_ranges(line);
        let urls: Vec<String> = ranges
            .into_iter()
            .map(|(s, e)| line.chars().skip(s).take(e - s).collect())
            .collect();
        assert_eq!(
            urls,
            vec!["https://example.com/path?q=1", "file:///tmp/a.txt"]
        );
    }

    #[test]
    fn url_hit_requires_cursor_inside_url() {
        let line = "open https://example.com/docs now";
        assert_eq!(
            url_range_at_col(line, 8).map(|(_, _, url)| url),
            Some("https://example.com/docs".to_string())
        );
        assert!(url_range_at_col(line, 0).is_none());
        assert!(url_range_at_col(line, line.chars().count() - 1).is_none());
    }

    #[test]
    fn url_ranges_reject_missing_scheme_and_empty_authority() {
        assert!(url_ranges("not a url: ://example.com").is_empty());
        assert!(url_ranges("not a url: https://").is_empty());
        assert!(url_ranges("example.com/path").is_empty());
    }

    #[test]
    fn url_hit_reconstructs_wrapped_url_across_rows() {
        let frame = vec![
            "    https://example.".to_string(),
            "com/docs?q=1      ".to_string(),
        ];
        let hit = super::url_hit_in_frame(&frame, 2, 1, Rect::new(0, 0, 20, 2))
            .expect("wrapped URL should be clickable from second row");
        assert_eq!(hit.url, "https://example.com/docs?q=1");
        assert_eq!(hit.ranges.len(), 2);
        assert_eq!(hit.ranges[0].row, 0);
        assert_eq!(hit.ranges[0].start_col, 4);
        assert_eq!(hit.ranges[0].end_col, 20);
        assert_eq!(hit.ranges[1].row, 1);
        assert_eq!(hit.ranges[1].start_col, 0);
        assert_eq!(hit.ranges[1].end_col, 12);
    }
}

/// Three-way choice in the group-delete confirmation minibuffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDeleteChoice {
    /// User pressed Enter on an empty line, or typed anything that
    /// isn't a recognized "yes" variant. Treat as cancel — the
    /// destructive default is always "no".
    Cancel,
    /// `y` / `yes` — drop the group, keep the sessions (their
    /// `group_id` clears to `None`).
    OrphanMembers,
    /// `all` — drop the group AND every member session. Requires
    /// typing the full word; a single-letter `a` is rejected so a
    /// stray keystroke can't trigger a cascade delete.
    DeleteMembers,
}

pub fn parse_group_delete_choice(input: &str) -> GroupDeleteChoice {
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => GroupDeleteChoice::OrphanMembers,
        // Intentionally NO single-letter alias here — the destructive
        // cascade should never be a typo away from "y".
        "all" => GroupDeleteChoice::DeleteMembers,
        _ => GroupDeleteChoice::Cancel,
    }
}

#[cfg(test)]
mod group_delete_prompt_tests {
    use super::{parse_group_delete_choice, GroupDeleteChoice};

    /// `y` / `yes` (any case, with whitespace) → orphan members
    /// (original pre-cascade behavior).
    #[test]
    fn yes_orphans_members() {
        for s in ["y", "Y", "yes", "YES", "  y  ", " Yes "] {
            assert_eq!(
                parse_group_delete_choice(s),
                GroupDeleteChoice::OrphanMembers,
                "input {s:?} should orphan",
            );
        }
    }

    /// Only the full word `all` (case-insensitive, whitespace ok)
    /// triggers cascade-delete. Requiring the full word means the
    /// destructive option is never one stray keystroke away from a
    /// confirm.
    #[test]
    fn all_deletes_members() {
        for s in ["all", "ALL", "  all  ", " All "] {
            assert_eq!(
                parse_group_delete_choice(s),
                GroupDeleteChoice::DeleteMembers,
                "input {s:?} should delete members",
            );
        }
    }

    /// Regression: a single `a` must NOT be a shortcut for cascade.
    /// Same for `al` or any prefix. The user has to type the full
    /// word.
    #[test]
    fn single_letter_a_does_not_delete_members() {
        for s in ["a", "A", "  a  ", "al", "AL"] {
            assert_eq!(
                parse_group_delete_choice(s),
                GroupDeleteChoice::Cancel,
                "input {s:?} must not delete members — full word required",
            );
        }
    }

    /// The destructive default is always cancel. Empty input,
    /// explicit `n`/`no`, garbage, or anything ambiguous routes to
    /// Cancel so a stray keystroke never wipes sessions.
    #[test]
    fn anything_else_cancels() {
        for s in ["", " ", "n", "N", "no", "NO", "maybe", "1", "yep", "delete"] {
            assert_eq!(
                parse_group_delete_choice(s),
                GroupDeleteChoice::Cancel,
                "input {s:?} should cancel",
            );
        }
    }
}
