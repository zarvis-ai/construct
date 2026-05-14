//! TUI app state and event loop.

use agentd_client::Client;
use crate::keymap::{self, ChordState, KeyAction, Keymap, KeymapResult, Profile};
use crate::ui;
use agentd_protocol::{
    EventNotificationPayload, GroupSummary, HarnessInfo, SessionEvent, SessionSummary,
    StateNotificationPayload, TimestampedEvent,
};
use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as CtEvent, EventStream, KeyCode, KeyEvent,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::collections::HashMap;
use std::io::Stdout;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which pane currently owns the keyboard. `View` covers both the transcript
/// and the terminal renderer — when the view shows a PTY-backed session and
/// View has focus, keystrokes are captured by the PTY (with `C-x` as the
/// escape prefix back to agentd commands).
/// Max scrollback rows kept by each [`vt100::Parser`]. Mouse-wheel can scroll
/// up to this many lines into history.
pub const SCROLLBACK_MAX: usize = 5_000;

/// A row in the rendered list view. Sessions and group headers share the
/// list; key dispatch and selection are typed.
#[derive(Debug, Clone)]
pub enum ListItem {
    Session { summary: SessionSummary, indented: bool },
    GroupHeader { group: GroupSummary, member_count: usize },
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
        if let Self::Session(id) = self { Some(id) } else { None }
    }
    pub fn group_id(&self) -> Option<&str> {
        if let Self::Group(id) = self { Some(id) } else { None }
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    SendInput { session_id: String },
    NewSessionHarness,
    /// Second stage of the new-session wizard when the user typed `group`:
    /// asks for the group's name.
    NewGroupName,
    DeleteConfirm { session_id: String },
    Rename { session_id: String },
    GroupDeleteConfirm { group_id: String },
    GroupRename { group_id: String },
    CommandPalette,
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
    pub help_visible: bool,
    pub profile: Profile,
    pub keymap: Keymap,
    pub chord_state: ChordState,
    pub chord_label: String,
    pub status: Option<(String, Instant)>,
    pub last_diff: Option<String>,
    pub should_quit: bool,
    pub connected: bool,
    // Terminal-pane state.
    pub view: ViewMode,
    pub terminals: HashMap<String, vt100::Parser>,
    pub terminal_pane_size: (u16, u16), // (cols, rows) of the right pane.
    /// Zoom: hide list / pin strip / modeline; the session view fills the
    /// screen except for the minibuffer line at the bottom. Toggled with
    /// `C-x z` (emacs) / `z` (vim), matching tmux's prefix-z.
    pub zoom: ZoomMode,
    /// Scrollback offset (in rows) applied to the *focused* session's PTY
    /// parser when rendering. 0 = live view. Increased by mouse-wheel up,
    /// decreased by mouse-wheel down. Reset to 0 on user keystroke into
    /// the PTY or on session change.
    pub view_scrollback: usize,
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
}

/// Last-frame geometry for hit-testing mouse clicks.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayoutSnapshot {
    pub list_area: Option<ratatui::layout::Rect>,
    pub view_area: Option<ratatui::layout::Rect>,
    pub pin_strip_area: Option<ratatui::layout::Rect>,
    pub minibuffer_area: Option<ratatui::layout::Rect>,
    /// Number of rows of the list pane currently in use (so a click
    /// past the last row is a no-op rather than selecting an
    /// out-of-range item). Mirrors `app.list_items().len()`.
    pub list_row_count: usize,
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
pub const SPINNER_FRAMES: [&str; 8] =
    ["✦", "✧", "✶", "✷", "✸", "✷", "✶", "✧"];

pub async fn run(client: Arc<Client>) -> Result<()> {
    let profile = Profile::from_env();
    let keymap = keymap::default_for(profile);

    // Initial fetches.
    let sessions = client.list().await.unwrap_or_default();
    let groups = client.list_groups().await.unwrap_or_default();
    let harnesses = client.harnesses().await.unwrap_or_default();
    let initial_sel = sessions
        .first()
        .map(|s| Selection::Session(s.id.clone()))
        .unwrap_or(Selection::None);

    let mut app = App {
        client: client.clone(),
        sessions,
        groups,
        selection: initial_sel,
        // Start focused on the list so navigation keys (Up/Down, C-n/C-p)
        // work immediately on first launch. User reaches the view with
        // `C-x o` or `Tab`.
        focus: PaneFocus::List,
        transcript: Vec::new(),
        transcript_session: None,
        transcript_scroll: 0,
        minibuffer: None,
        harnesses,
        help_visible: false,
        profile,
        keymap,
        chord_state: ChordState::default(),
        chord_label: String::new(),
        status: None,
        last_diff: None,
        should_quit: false,
        connected: true,
        view: ViewMode::Transcript,
        terminals: HashMap::new(),
        terminal_pane_size: (100, 30),
        zoom: ZoomMode::None,
        view_scrollback: 0,
        pty_activity: HashMap::new(),
        start_instant: Instant::now(),
        layout: LayoutSnapshot::default(),
    };
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

    let result = run_loop(&mut terminal, &mut app).await;

    // Teardown — best effort.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture);
    terminal.show_cursor().ok();
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut input_stream = EventStream::new();
    let mut notifications = app
        .client
        .take_notifications()
        .await
        .context("notifications channel already taken")?;
    // Tick at the spinner frame boundary so each frame gets one redraw.
    let mut tick = tokio::time::interval(Duration::from_millis(SPINNER_FRAME_MS as u64));

    let mut last_size_sent: (u16, u16) = (0, 0);
    while !app.should_quit {
        terminal.draw(|f| ui::render(f, app))?;
        // If the right pane changed size, push pty_resize for the current
        // PTY session (if any). Skip the very first frame (0,0).
        let cur = app.terminal_pane_size;
        if cur != last_size_sent && cur.0 > 0 && cur.1 > 0 {
            app.notify_pane_size(cur.0, cur.1).await;
            last_size_sent = cur;
        }
        tokio::select! {
            ev = input_stream.next() => {
                match ev {
                    Some(Ok(ev)) => app.on_term_event(ev).await,
                    Some(Err(e)) => {
                        app.set_status(format!("input error: {e}"));
                    }
                    None => break,
                }
            }
            notif = notifications.recv() => {
                match notif {
                    Some(n) => app.on_notification(n).await,
                    None => {
                        app.connected = false;
                        app.set_status("daemon disconnected".to_string());
                    }
                }
            }
            _ = tick.tick() => {
                // Clear expired status, redraw.
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
    pub fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
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

    /// Materialize the rendered list: ungrouped sessions (sorted by
    /// position) on top, then groups in position order with each group's
    /// members indented underneath (skipped entirely when the group is
    /// collapsed).
    pub fn list_items(&self) -> Vec<ListItem> {
        let mut out: Vec<ListItem> = Vec::new();

        let mut ungrouped: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| s.group_id.is_none())
            .collect();
        ungrouped.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        for s in ungrouped {
            out.push(ListItem::Session { summary: s.clone(), indented: false });
        }

        let mut groups: Vec<&GroupSummary> = self.groups.iter().collect();
        groups.sort_by_key(|g| g.position);
        for g in groups {
            let mut members: Vec<&SessionSummary> = self
                .sessions
                .iter()
                .filter(|s| s.group_id.as_deref() == Some(g.id.as_str()))
                .collect();
            members.sort_by_key(|s| s.position);
            out.push(ListItem::GroupHeader {
                group: g.clone(),
                member_count: members.len(),
            });
            if !g.collapsed {
                for s in members {
                    out.push(ListItem::Session { summary: s.clone(), indented: true });
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
    }

    /// Bootstrap a vt100 parser for every pinned PTY-backed session that
    /// doesn't have one yet. Called at startup and whenever a session is
    /// freshly pinned (so the pin strip never shows a blank tile for a
    /// session that has had output).
    pub async fn ensure_pinned_parsers(&mut self) {
        let ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|s| s.pinned && s.has_pty && !self.terminals.contains_key(&s.id))
            .map(|s| s.id.clone())
            .collect();
        for id in ids {
            self.bootstrap_terminal(&id).await;
        }
    }

    async fn bootstrap_terminal(&mut self, id: &str) {
        if self.terminals.contains_key(id) {
            return;
        }
        let (cols, rows) = self.terminal_pane_size;
        let mut parser =
            vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK_MAX);
        match self.client.pty_replay(id).await {
            Ok(snap) => {
                use base64::Engine;
                if let Ok(bytes) =
                    base64::engine::general_purpose::STANDARD.decode(&snap.data)
                {
                    parser.process(&bytes);
                }
            }
            Err(e) => {
                self.set_status(format!("pty_replay: {e}"));
            }
        }
        self.terminals.insert(id.to_string(), parser);
        // Tell the daemon what size we'd like.
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
                self.set_status(
                    if want {
                        format!("pinned {} member(s)", members.len())
                    } else {
                        format!("unpinned {} member(s)", members.len())
                    },
                );
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
        self.ensure_selection_valid();
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
        self.selection = match &items[next] {
            ListItem::Session { summary, .. } => Selection::Session(summary.id.clone()),
            ListItem::GroupHeader { group, .. } => Selection::Group(group.id.clone()),
        };
        self.refresh_selected_transcript().await;
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
                        // PTY events: feed into the per-session terminal emulator.
                        if let SessionEvent::Pty { .. } = &payload.event {
                            if let Some(bytes) = payload.event.pty_bytes() {
                                let parser = self
                                    .terminals
                                    .entry(payload.session_id.clone())
                                    .or_insert_with(|| {
                                        let (cols, rows) = self.terminal_pane_size;
                                        vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK_MAX)
                                    });
                                parser.process(&bytes);
                            }
                            // Mark the session as freshly active for the spinner.
                            self.pty_activity
                                .insert(payload.session_id.clone(), Instant::now());
                            return;
                        }
                        if Some(payload.session_id.as_str())
                            == self.transcript_session.as_deref()
                        {
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
                            self.sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
                        }
                        // Newly pinned PTY session: bootstrap so its tile
                        // populates immediately, even when the pin came from
                        // outside this TUI process.
                        if has_pty && now_pinned && !was_pinned {
                            self.bootstrap_terminal(&id).await;
                        }
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::DELETED => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::DeletedNotificationPayload,
                    >(p)
                    {
                        self.on_session_deleted(&payload.session_id).await;
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::GROUP_STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::GroupStateNotificationPayload,
                    >(p)
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
            Ok(()) => self.set_status(format!(
                "automode {}",
                if next { "ON" } else { "off" }
            )),
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
        self.terminals.remove(id);
        self.pty_activity.remove(id);
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
        let idx =
            (self.start_instant.elapsed().as_millis() / SPINNER_FRAME_MS) as usize
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
        use crossterm::event::MouseButton;
        const STEP: i32 = 3;
        match ev.kind {
            MouseEventKind::ScrollUp => self.adjust_scrollback(STEP),
            MouseEventKind::ScrollDown => self.adjust_scrollback(-STEP),
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_left_click(ev.column, ev.row).await;
            }
            _ => {}
        }
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
        if let Some(mb_area) = self.layout.minibuffer_area {
            if contains(mb_area, col, row) {
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
                self.click_list(list, row).await;
                return;
            }
        }
        if let Some(view) = self.layout.view_area {
            if contains(view, col, row) {
                self.focus = PaneFocus::View;
                return;
            }
        }
    }

    async fn click_minibuffer(&mut self, mb_area: ratatui::layout::Rect, col: u16) {
        if let Some(mb) = self.minibuffer.as_mut() {
            // Position the cursor at the clicked column inside the
            // input. Math: text starts at `area.x + prompt.width()`.
            // ApproveTool is a single-key intent — no cursor moves are
            // meaningful, so we skip those.
            if matches!(mb.intent, MinibufferIntent::ApproveTool { .. }) {
                return;
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
            // No minibuffer open — clicking the prompt area opens the
            // command palette, matching the `M-x` / `C-x x` chord.
            self.run_action(KeyAction::OpenCommandPalette).await;
        }
    }

    async fn click_list(&mut self, list: ratatui::layout::Rect, row: u16) {
        // A click anywhere inside the list pane focuses it, even on the
        // border or empty space past the last item — matching the
        // intuitive "click the pane to focus it" UX.
        self.focus = PaneFocus::List;
        // Top + bottom border are 1 row each; rows outside the inner
        // content area only handle the focus change above.
        if row <= list.y || row + 1 >= list.y + list.height {
            return;
        }
        let idx = (row - list.y - 1) as usize;
        let items = self.list_items();
        if idx >= items.len() {
            return;
        }
        match &items[idx] {
            ListItem::Session { summary, .. } => {
                self.selection = Selection::Session(summary.id.clone());
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
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
                    self.selection = Selection::Group(id.clone());
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
            if col >= tile.x
                && col < tile.x + tile.width
                && row >= tile.y
                && row < tile.y + tile.height
            {
                self.selection = Selection::Session(id.clone());
                self.transcript_session = None;
                self.refresh_selected_transcript().await;
                self.focus = PaneFocus::List;
                return;
            }
        }
    }

    /// Adjust the focused session's scrollback offset. Positive `delta` =
    /// scroll up (older); negative = scroll down (newer). No-op unless the
    /// view is on a PTY-backed session in terminal mode. vt100 clamps the
    /// offset to its actual buffer size internally on `set_scrollback`.
    fn adjust_scrollback(&mut self, delta: i32) {
        if self.view != ViewMode::Terminal || !self.in_pty_session() {
            return;
        }
        let cur = self.view_scrollback as i32;
        let next = (cur + delta).max(0).min(SCROLLBACK_MAX as i32);
        self.view_scrollback = next as usize;
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
        // Minibuffer captures all input when open.
        if self.minibuffer.is_some() {
            self.handle_minibuffer_key(key).await;
            return;
        }
        if self.help_visible {
            // Any key closes help.
            self.help_visible = false;
            return;
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
                    let _ = self.client.pty_input(&id, vec![0x18]).await;
                }
                return;
            }
            if self.chord_state.is_empty() && !is_ctrl_x {
                // Typing snaps the view back to live: it's confusing to
                // type "into the past" while reading scrollback.
                self.view_scrollback = 0;
                if let Some(bytes) = encode_key_to_bytes(key) {
                    if let Some(id) = self.selected_id() {
                        if let Err(e) = self.client.pty_input(&id, bytes).await {
                            self.set_status(format!("pty_input failed: {e}"));
                        }
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
                            "Delete group '{}' (members will be orphaned)? (y/N): ",
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
                self.minibuffer = Some(Minibuffer {
                    prompt: "M-x ".to_string(),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::CommandPalette,
                    error: None,
                });
            }
            SwitchFocus => {
                // In a zoomed layout `C-x o` swaps which pane is
                // zoomed (and focused). In normal layout it just
                // swaps focus.
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
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(1);
                }
            }
            ScrollDown => {
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(1);
                }
            }
            ScrollPageUp => {
                if self.transcript_scroll == u16::MAX {
                    self.transcript_scroll = 0;
                } else {
                    self.transcript_scroll = self.transcript_scroll.saturating_sub(10);
                }
            }
            ScrollPageDown => {
                if self.transcript_scroll != u16::MAX {
                    self.transcript_scroll = self.transcript_scroll.saturating_add(10);
                }
            }
            ScrollTop => {
                self.transcript_scroll = 0;
            }
            ScrollBottom => {
                self.transcript_scroll = u16::MAX;
            }
            ToggleHelp => {
                self.help_visible = !self.help_visible;
            }
            ToggleAutomode => {
                self.toggle_automode().await;
            }
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
                if let Some(MinibufferIntent::ApproveTool { session_id, call_id, .. }) =
                    self.minibuffer.as_ref().map(|m| m.intent.clone())
                {
                    self.minibuffer = None;
                    match self.client.tool_decision(&session_id, call_id, d).await {
                        Ok(()) => self.set_status(format!("tool {d}")),
                        Err(e) => self.set_status(format!("tool_decision failed: {e}")),
                    }
                }
            }
            return;
        }

        let Some(mb) = self.minibuffer.as_mut() else { return; };
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
                        mb.error = Some(format!(
                            "unknown: {trimmed} (Tab to complete)"
                        ));
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
                        if !self.terminals.contains_key(&id) {
                            let (cols, rows) = self.terminal_pane_size;
                            self.terminals.insert(
                                id.clone(),
                                vt100::Parser::new(
                                    rows.max(1),
                                    cols.max(1),
                                    SCROLLBACK_MAX,
                                ),
                            );
                        }
                        self.selection = Selection::Session(id);
                        self.refresh_selected_transcript().await;
                        self.focus = PaneFocus::View;
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
                }
            }
            MinibufferIntent::GroupDeleteConfirm { group_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("group delete cancelled".to_string());
                    return;
                }
                match self.client.delete_group(&group_id).await {
                    Ok(()) => self.set_status("group deleted".into()),
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
                        if let Some(g) =
                            self.groups.iter_mut().find(|g| g.id == group_id)
                        {
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
                        self.selection = Selection::Group(id);
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
                        if let Some(i) =
                            self.sessions.iter().position(|s| s.id == session_id)
                        {
                            self.sessions[i].title = new_title.clone();
                        }
                        self.set_status(
                            match &new_title {
                                Some(t) => format!("renamed → {t}"),
                                None => "title cleared".into(),
                            },
                        );
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
            MinibufferIntent::CommandPalette => {
                let cmd = input.trim();
                self.run_palette_command(cmd).await;
            }
            MinibufferIntent::ApproveTool { session_id, call_id, .. } => {
                // Reached only if the special-cased key handler in
                // handle_minibuffer_key fell through (defensive — should
                // not happen in practice). Treat any submit as approve.
                if let Err(e) = self.client.tool_decision(&session_id, call_id, "approve").await {
                    self.set_status(format!("tool_decision failed: {e}"));
                }
            }
        }
    }

    async fn run_palette_command(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        match cmd {
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
            "diff" => self.run_action(KeyAction::OpenDiff).await,
            "interrupt" => self.run_action(KeyAction::Interrupt).await,
            "help" | "?" => self.help_visible = true,
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
            other => self.set_status(format!("unknown command: {other}")),
        }
    }
}

pub fn short_id(id: &str) -> &str {
    let n = id.len().min(10);
    &id[..n]
}

fn byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
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
    let matches: Vec<&String> =
        options.iter().filter(|o| o.starts_with(&current)).collect();
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
    let Some(first) = strs.first() else { return out };
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
