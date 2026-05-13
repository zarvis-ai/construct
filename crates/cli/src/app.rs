//! TUI app state and event loop.

use crate::client::Client;
use crate::keymap::{self, ChordState, KeyAction, Keymap, KeymapResult, Profile};
use crate::ui;
use agentd_protocol::{
    EventNotificationPayload, HarnessInfo, SessionEvent, SessionSummary, StateNotificationPayload,
    TimestampedEvent,
};
use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
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

#[derive(Debug, Clone)]
pub enum MinibufferIntent {
    SendInput { session_id: String },
    NewSessionHarness,
    DeleteConfirm { session_id: String },
    CommandPalette,
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
    pub selected: usize,
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
}

pub async fn run(client: Arc<Client>) -> Result<()> {
    let profile = Profile::from_env();
    let keymap = keymap::default_for(profile);

    // Initial fetches.
    let sessions = client.list().await.unwrap_or_default();
    let harnesses = client.harnesses().await.unwrap_or_default();

    let mut app = App {
        client: client.clone(),
        sessions,
        selected: 0,
        focus: PaneFocus::View,
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

    // Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(&mut terminal, &mut app).await;

    // Teardown — best effort.
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
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
    let mut tick = tokio::time::interval(Duration::from_millis(500));

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
        self.sessions.get(self.selected)
    }

    pub fn selected_id(&self) -> Option<String> {
        self.selected_session().map(|s| s.id.clone())
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

    async fn bootstrap_terminal(&mut self, id: &str) {
        if self.terminals.contains_key(id) {
            return;
        }
        let (cols, rows) = self.terminal_pane_size;
        let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), 5_000);
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

    async fn refresh_sessions(&mut self) {
        match self.client.list().await {
            Ok(list) => {
                let prev_id = self.selected_id();
                self.sessions = list;
                if let Some(pid) = prev_id {
                    if let Some(i) = self.sessions.iter().position(|s| s.id == pid) {
                        self.selected = i;
                    } else if self.selected >= self.sessions.len() {
                        self.selected = self.sessions.len().saturating_sub(1);
                    }
                }
            }
            Err(e) => self.set_status(format!("list failed: {e}")),
        }
    }

    async fn on_notification(&mut self, n: agentd_protocol::Notification) {
        match n.method.as_str() {
            m if m == agentd_protocol::ipc_notif::EVENT => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<EventNotificationPayload>(p) {
                        // PTY events: feed into the per-session terminal emulator.
                        if let SessionEvent::Pty { .. } = &payload.event {
                            if let Some(bytes) = payload.event.pty_bytes() {
                                let parser = self
                                    .terminals
                                    .entry(payload.session_id.clone())
                                    .or_insert_with(|| {
                                        let (cols, rows) = self.terminal_pane_size;
                                        vt100::Parser::new(rows.max(1), cols.max(1), 5_000)
                                    });
                                parser.process(&bytes);
                            }
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
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.sessions[i] = payload.session;
                        } else {
                            self.sessions.push(payload.session);
                            self.sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
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
            _ => {}
        }
    }

    async fn on_session_deleted(&mut self, id: &str) {
        // Drop from list.
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions.remove(i);
            // Keep `selected` in range.
            if self.selected >= self.sessions.len() && !self.sessions.is_empty() {
                self.selected = self.sessions.len() - 1;
            } else if self.sessions.is_empty() {
                self.selected = 0;
            }
        }
        // Drop cached transcript / terminal state for this session.
        if self.transcript_session.as_deref() == Some(id) {
            self.transcript.clear();
            self.transcript_session = None;
        }
        self.terminals.remove(id);
        // Refresh the right pane for whatever's now selected.
        self.refresh_selected_transcript().await;
    }

    async fn on_term_event(&mut self, ev: CtEvent) {
        match ev {
            CtEvent::Key(k) => self.on_key(k).await,
            CtEvent::Resize(_, _) => {
                // The TUI re-derives the pane size on next render; we trigger
                // an explicit resize for the current PTY there.
            }
            _ => {}
        }
    }

    /// Called from `ui::render` after computing the terminal pane size. Sends
    /// `pty_resize` when the size changed for the currently-focused PTY
    /// session.
    pub async fn notify_pane_size(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.terminal_pane_size {
            return;
        }
        self.terminal_pane_size = (cols, rows);
        for parser in self.terminals.values_mut() {
            parser.set_size(rows.max(1), cols.max(1));
        }
        if let Some(id) = self.selected_id() {
            if self
                .selected_session()
                .map(|s| s.has_pty)
                .unwrap_or(false)
            {
                let _ = self.client.pty_resize(&id, cols, rows).await;
            }
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
    /// default (view focused, terminal mode, session has a PTY).
    fn is_pty_captured(&self) -> bool {
        self.focus == PaneFocus::View
            && self.view == ViewMode::Terminal
            && self.in_pty_session()
    }

    async fn run_action(&mut self, action: KeyAction) {
        use KeyAction::*;
        match action {
            Quit => self.should_quit = true,
            NextSession => {
                if !self.sessions.is_empty() {
                    self.selected = (self.selected + 1) % self.sessions.len();
                    self.refresh_selected_transcript().await;
                }
            }
            PrevSession => {
                if !self.sessions.is_empty() {
                    if self.selected == 0 {
                        self.selected = self.sessions.len() - 1;
                    } else {
                        self.selected -= 1;
                    }
                    self.refresh_selected_transcript().await;
                }
            }
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
                let hint = self
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.as_str())
                    .collect::<Vec<_>>()
                    .join("|");
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("Harness [{hint}] (Tab completes): "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::NewSessionHarness,
                    error: None,
                });
            }
            OpenDeleteConfirm => {
                if let Some(id) = self.selected_id() {
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
            }
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
                self.focus = match self.focus {
                    PaneFocus::List => PaneFocus::View,
                    PaneFocus::View => PaneFocus::List,
                };
                let label = match self.focus {
                    PaneFocus::List => "focus: list",
                    PaneFocus::View => "focus: view",
                };
                self.set_status(label.into());
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
            self.harnesses
                .iter()
                .filter(|h| h.available)
                .map(|h| h.name.clone())
                .collect()
        } else {
            Vec::new()
        };

        let Some(mb) = self.minibuffer.as_mut() else { return; };
        match key.code {
            KeyCode::Esc => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
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
                            "no such harness: {trimmed} (Tab to complete)"
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
                if mb.cursor > 0 {
                    let prev = mb.cursor - 1;
                    mb.input.remove(prev);
                    mb.cursor = prev;
                }
                mb.error = None;
            }
            KeyCode::Left => {
                mb.cursor = mb.cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Home => mb.cursor = 0,
            KeyCode::End => mb.cursor = mb.input.chars().count(),
            KeyCode::Char(c) => {
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
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.selected = i;
                            self.refresh_selected_transcript().await;
                            // Drop focus into the new session's pane so the
                            // user can start typing immediately.
                            self.focus = PaneFocus::View;
                        }
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
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
