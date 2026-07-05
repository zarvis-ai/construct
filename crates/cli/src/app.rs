//! TUI app state and event loop.

use crate::keymap::{self, ChordState, KeyAction, Keymap, KeymapResult, Profile};
use crate::ui;
use agentd_client::Client;
use agentd_protocol::{
    EventNotificationPayload, GroupSummary, HarnessInfo, MessageRole, Notification, Request,
    SessionEvent, SessionSummary, StateNotificationPayload, TimestampedEvent,
};
use anyhow::{Context, Result};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CtEvent, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::{FutureExt, StreamExt};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Stdout, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::mpsc;

mod dynamic_ui;
mod editor;
mod matrix_clicks;
mod minibuffer;
mod mouse;
mod program_popup;
mod session_picker;
mod session_title_menu;
pub use session_picker::{
    session_picker_scroll, SessionPickerDialog, SessionPickerPurpose, SessionPickerRow,
};

pub const TERMINAL_SCROLLBAR_TTL: Duration = Duration::from_millis(1200);
pub(crate) const DYNAMIC_UI_AUTOHIDE_SECS: u64 = 15;
/// How long a hover-revealed widget lingers after the cursor leaves its title
/// square (and the widget body). Short and responsive, just enough for the
/// pointer to travel from the square down onto the widget without it vanishing —
/// distinct from the 15s create/update auto-reveal above.
pub(crate) const DYNAMIC_UI_HOVER_GRACE_MS: u64 = 1000;

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
pub(crate) const PROGRAM_REVEAL_MS: u64 = 240;
pub(crate) const PROGRAM_CONTENT_PADDING_X: u16 = 1;
pub(crate) const PROGRAM_CONTENT_PADDING_Y: u16 = 1;
/// Hard cap on how long a program Run shimmer animates without an observed
/// output signal. A missed first-output transition must never strand the
/// animation; a timeout backstop is mandatory (spec 0042).
pub(crate) const PROGRAM_RUN_MAX_MS: u64 = 10 * 60 * 1000;
/// How long an identical Run (same session, scope, and executed body) is
/// suppressed after a successful dispatch (spec 0042 consequence): long
/// enough to absorb a double `C-x C-r` / double-click, which the TUI's
/// serialized event loop usually delivers as a second call milliseconds
/// *after* the first has already finished its save/execute round trip, but
/// short enough that a deliberate re-Run a moment later still goes through.
pub(crate) const PROGRAM_RUN_DEDUP_WINDOW_MS: u64 = 1500;
/// One-shot flourish shown when an authoritative program Run pending block
/// settles. Presentation-only and client-local.
pub(crate) const PROGRAM_SETTLE_FLASH_MS: u64 = 300;
/// Remote Program collaborator cursors are presence hints. Hide them when the
/// peer has not published activity recently.
pub(crate) const PROGRAM_COLLAB_CURSOR_TTL_MS: i64 = 60 * 1000;
/// Wrapped rows the program body scrolls per mouse-wheel notch.
pub(crate) const PROGRAM_WHEEL_SCROLL_ROWS: usize = 3;
const PROGRAM_UNDO_STACK_LIMIT: usize = 100;
const LARGE_TEXT_PASTE_CHARS: usize = 16 * 1024;

/// A row in the rendered list view. Sessions and group headers share the
/// list; key dispatch and selection are typed.
#[derive(Debug, Clone)]
pub enum ListItem {
    Session {
        summary: SessionSummary,
        indented: bool,
        has_children: bool,
        children_expanded: bool,
    },
    GroupHeader {
        group: GroupSummary,
        member_count: usize,
    },
    /// Expandable "▸ N archived" / "▾ N archived" row that ends a section
    /// (the ungrouped top-level run or a project) when that section has
    /// archived sessions. Clicking it reveals/hides that section's archived
    /// sessions; it is not itself a selectable target.
    ArchivedRow {
        section: ArchiveSection,
        count: usize,
        expanded: bool,
        indented: bool,
    },
}

/// A region of the session list whose archived sessions can be revealed
/// independently: the ungrouped top-level run, a specific project, or one
/// parent session's subagents.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ArchiveSection {
    Ungrouped,
    Group(String),
    Subagents(String),
}

fn is_user_list_session(s: &SessionSummary) -> bool {
    matches!(s.kind, agentd_protocol::SessionKind::User)
}

fn is_subagent_session(s: &SessionSummary) -> bool {
    matches!(s.kind, agentd_protocol::SessionKind::Subagent)
}

fn selection_is_valid_for_sessions(
    selection: &Selection,
    sessions: &[SessionSummary],
    groups: &[GroupSummary],
) -> bool {
    match selection {
        Selection::None => true,
        // A pane may hold any live session, not just ones with a list row: a
        // program clip can point the main view at a subagent. Keep the pane as
        // long as the session still exists; pruning only fires once it's gone.
        Selection::Session(id) => sessions.iter().any(|s| s.id == *id),
        Selection::Group(id) => groups.iter().any(|g| g.id == *id),
        Selection::ArchivedRow(section) => match section {
            ArchiveSection::Ungrouped => sessions
                .iter()
                .any(|s| is_user_list_session(s) && s.archived && s.group_id.is_none()),
            ArchiveSection::Group(id) => sessions.iter().any(|s| {
                is_user_list_session(s) && s.archived && s.group_id.as_deref() == Some(id.as_str())
            }),
            ArchiveSection::Subagents(parent_id) => sessions.iter().any(|s| {
                is_subagent_session(s)
                    && s.archived
                    && s.parent_session_id.as_deref() == Some(parent_id.as_str())
            }),
        },
    }
}

fn prune_window_tree(
    tree: MainWindowTree,
    sessions: &[SessionSummary],
    groups: &[GroupSummary],
    fallback: &Selection,
) -> MainWindowTree {
    match tree {
        MainWindowTree::Leaf { id, selection } => MainWindowTree::Leaf {
            id,
            selection: if selection_is_valid_for_sessions(&selection, sessions, groups) {
                selection
            } else {
                fallback.clone()
            },
        },
        MainWindowTree::Split {
            direction,
            ratio_percent,
            first,
            second,
        } => MainWindowTree::Split {
            direction,
            ratio_percent,
            first: Box::new(prune_window_tree(*first, sessions, groups, fallback)),
            second: Box::new(prune_window_tree(*second, sessions, groups, fallback)),
        },
    }
}

/// Whether any visible split pane's desired PTY size differs from what its
/// session's child was last told to use. The resize fire in the event loop is
/// otherwise gated on the *active* pane's size, so a passive pane that changed
/// size on its own (split created, divider dragged, sibling swapped) would
/// never be resized — its child keeps emitting at a stale width that the pane
/// then renders into a different-width grid, garbling until the next active
/// resize. `visible` is the deduped `(session_id, (cols, rows))` list from
/// `window_session_pane_sizes`; `last_sent` is the size we last pushed per id.
fn pane_sizes_diverged(
    visible: &[(String, (u16, u16))],
    last_sent: &HashMap<String, (u16, u16)>,
) -> bool {
    visible
        .iter()
        .any(|(id, size)| last_sent.get(id) != Some(size))
}

pub(crate) fn list_session_indent_cells(
    s: &SessionSummary,
    indented: bool,
    has_children: bool,
) -> u16 {
    if is_subagent_session(s) {
        4
    } else if indented && has_children {
        1
    } else if indented {
        2
    } else {
        0
    }
}

impl ListItem {
    pub fn matches(&self, sel: &Selection) -> bool {
        match (self, sel) {
            (ListItem::Session { summary, .. }, Selection::Session(id)) => summary.id == *id,
            (ListItem::GroupHeader { group, .. }, Selection::Group(id)) => group.id == *id,
            (ListItem::ArchivedRow { section, .. }, Selection::ArchivedRow(sel)) => section == sel,
            _ => false,
        }
    }
}

/// What's currently focused in the list pane.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Selection {
    #[default]
    None,
    Session(String),
    Group(String),
    /// A section's "N archived" disclosure row. Selectable like a group header
    /// so keyboard nav can land on it and left/right expand/collapse it.
    ArchivedRow(ArchiveSection),
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
    pub fn archive_section(&self) -> Option<&ArchiveSection> {
        if let Self::ArchivedRow(section) = self {
            Some(section)
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

/// A spatial direction for moving keyboard focus between split panes
/// (emacs `windmove`). Used by the `Shift+Arrow` bindings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowSplitDirection {
    Below,
    Right,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MainWindowTree {
    Leaf {
        id: u64,
        selection: Selection,
    },
    Split {
        direction: WindowSplitDirection,
        ratio_percent: u16,
        first: Box<MainWindowTree>,
        second: Box<MainWindowTree>,
    },
}

impl MainWindowTree {
    fn single(id: u64, selection: Selection) -> Self {
        Self::Leaf { id, selection }
    }

    fn max_id(&self) -> u64 {
        match self {
            Self::Leaf { id, .. } => *id,
            Self::Split { first, second, .. } => first.max_id().max(second.max_id()),
        }
    }

    fn first_leaf_id(&self) -> Option<u64> {
        match self {
            Self::Leaf { id, .. } => Some(*id),
            Self::Split { first, .. } => first.first_leaf_id(),
        }
    }

    fn find_selection(&self, target: u64) -> Option<&Selection> {
        match self {
            Self::Leaf { id, selection } if *id == target => Some(selection),
            Self::Leaf { .. } => None,
            Self::Split { first, second, .. } => first
                .find_selection(target)
                .or_else(|| second.find_selection(target)),
        }
    }

    fn set_selection(&mut self, target: u64, replacement: Selection) -> bool {
        match self {
            Self::Leaf { id, selection } if *id == target => {
                *selection = replacement;
                true
            }
            Self::Leaf { .. } => false,
            Self::Split { first, second, .. } => {
                first.set_selection(target, replacement.clone())
                    || second.set_selection(target, replacement)
            }
        }
    }

    fn find_window_with_session_except(&self, target_id: &str, except_id: u64) -> Option<u64> {
        match self {
            Self::Leaf {
                id,
                selection: Selection::Session(session_id),
            } if *id != except_id && session_id == target_id => Some(*id),
            Self::Leaf { .. } => None,
            Self::Split { first, second, .. } => first
                .find_window_with_session_except(target_id, except_id)
                .or_else(|| second.find_window_with_session_except(target_id, except_id)),
        }
    }

    fn replace_session_selection(&mut self, target_id: &str, replacement: &Selection) {
        match self {
            Self::Leaf { selection, .. } => {
                if selection.session_id() == Some(target_id) {
                    *selection = replacement.clone();
                }
            }
            Self::Split { first, second, .. } => {
                first.replace_session_selection(target_id, replacement);
                second.replace_session_selection(target_id, replacement);
            }
        }
    }

    /// Session IDs of every visible leaf pane (all split halves included).
    pub fn visible_session_ids(&self) -> Vec<&str> {
        let mut ids = Vec::new();
        self.collect_session_ids_into(&mut ids);
        ids
    }

    fn collect_session_ids_into<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::Leaf {
                selection: Selection::Session(id),
                ..
            } => out.push(id.as_str()),
            Self::Leaf { .. } => {}
            Self::Split { first, second, .. } => {
                first.collect_session_ids_into(out);
                second.collect_session_ids_into(out);
            }
        }
    }
}

/// What the right pane is currently showing for the selected session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// Structured-event chat renderer (default for headless / non-PTY sessions).
    Chat,
    /// Live PTY emulator (default for sessions whose adapter has supports_pty).
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatScrollKind {
    Hidden,
    AssistantMessage,
    Message,
    Reasoning,
    Tool,
    Metadata,
}

fn chat_scroll_kind(ev: &SessionEvent) -> ChatScrollKind {
    match ev {
        SessionEvent::Pty { .. }
        | SessionEvent::PtyResize { .. }
        | SessionEvent::EditorState { .. }
        | SessionEvent::ClientCommand { .. }
        | SessionEvent::ToolApprovalResolved { .. }
        | SessionEvent::ApprovalModeChanged { .. }
        | SessionEvent::OperatorLoopChanged { .. }
        | SessionEvent::ModelChanged { .. }
        | SessionEvent::AgentStatus(_) => ChatScrollKind::Hidden,
        SessionEvent::Message { role, text }
            if should_render_chat_message_for_scroll(*role, text) =>
        {
            if *role == MessageRole::Assistant {
                ChatScrollKind::AssistantMessage
            } else {
                ChatScrollKind::Message
            }
        }
        SessionEvent::Message { .. } => ChatScrollKind::Hidden,
        SessionEvent::Reasoning { .. } => ChatScrollKind::Reasoning,
        SessionEvent::ToolUse { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::ToolApprovalRequest { .. }
        | SessionEvent::TaskStart { .. }
        | SessionEvent::TaskBackgrounded { .. }
        | SessionEvent::TaskEnd { .. } => ChatScrollKind::Tool,
        SessionEvent::Status { .. }
        | SessionEvent::AwaitingInput { .. }
        | SessionEvent::Cost { .. }
        | SessionEvent::Diff { .. }
        | SessionEvent::Error { .. }
        | SessionEvent::Reset
        | SessionEvent::Done { .. }
        | SessionEvent::UiPanel(_)
        | SessionEvent::UiDelete { .. }
        | SessionEvent::BrowserPreview(_)
        | SessionEvent::ContextCompacted { .. } => ChatScrollKind::Metadata,
    }
}

fn chat_scroll_needs_gap(previous: ChatScrollKind, current: ChatScrollKind) -> bool {
    !matches!(
        (previous, current),
        (ChatScrollKind::Tool, ChatScrollKind::Tool)
            | (ChatScrollKind::Metadata, ChatScrollKind::Metadata)
            | (ChatScrollKind::Reasoning, ChatScrollKind::Reasoning)
            | (
                ChatScrollKind::AssistantMessage,
                ChatScrollKind::AssistantMessage
            )
    )
}

fn should_render_chat_message_for_scroll(role: MessageRole, text: &str) -> bool {
    let trimmed = text.trim_start();
    if role == MessageRole::Assistant && trimmed.starts_with("<permissions instructions>") {
        return false;
    }
    if role == MessageRole::User
        && trimmed.starts_with("# AGENTS.md instructions for ")
        && trimmed.contains("\n<INSTRUCTIONS>")
    {
        return false;
    }
    true
}

fn transcript_scroll_pos(value: usize) -> u16 {
    value.min((u16::MAX - 1) as usize) as u16
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
    /// Harness picker for forking the selected session into a new sibling
    /// (`OpenFork`). Shares the harness-picker UI/completion with
    /// `NewSessionHarness`; on submit, calls `client.fork_session`.
    ForkSessionHarness {
        source_session_id: String,
    },
    /// Second stage of the new-session wizard when the user typed `group`:
    /// asks for the group's name.
    NewGroupName,
    DeleteConfirm {
        session_id: String,
    },
    MenuArchiveConfirm {
        session_id: String,
    },
    MenuDeleteConfirm {
        session_id: String,
    },
    MenuUnarchiveConfirm {
        session_id: String,
    },
    /// Confirmation prompt for restarting a terminated (`Done` /
    /// `Errored`) session. Single-key dispatch: `y`/Enter respawns
    /// the adapter (with `CONSTRUCT_RESUME=1` so persistent harnesses
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
    /// Confirmation prompt for cascade-deleting every archived session a
    /// "N archived" disclosure row stands in for. `y`/`yes` deletes each
    /// archived session in the section (drops transcript + worktree, with
    /// the subagent cascade applied per session); anything else cancels.
    ArchivedDeleteConfirm {
        section: ArchiveSection,
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
    /// without inline approval UI. Single-key dispatch: `y`/Enter
    /// approve, `n`/Esc deny, `a` auto-review, `f` unsafe-auto.
    ApproveTool {
        session_id: String,
        call_id: String,
        tool: String,
        args_summary: String,
        risk: agentd_protocol::ToolRisk,
        allow_auto_review: bool,
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

/// A matrix-rain horizontal reveal word's clickable span on screen.
/// `col_start..=col_end` at `row` (absolute terminal coords).
#[derive(Debug, Clone)]
pub struct MatrixRevealHit {
    pub col_start: u16,
    pub col_end: u16,
    pub row: u16,
    pub text: String,
    pub session_id: String,
}

impl MatrixRevealHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.col_start && col <= self.col_end
    }
}

/// On-screen cell range of a session smart-clip (`@{session:id}`) rendered in
/// the program body, captured each frame so a hover/click can map a cell back to
/// its session id. A clip that word-wraps across rows contributes one hit per
/// row segment. `col_end` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramClipHit {
    pub col_start: u16,
    pub col_end: u16,
    pub row: u16,
    pub session_id: String,
}

impl ProgramClipHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.col_start && col < self.col_end
    }
}

/// A clickable template button drawn in the empty-program placeholder. The box
/// spans `row_start..=row_end` (top border, label, bottom border) over the
/// columns `col_start..col_end`; clicking anywhere inside fills the program with
/// that template's Markdown. Republished every frame the active program is empty.
#[derive(Debug, Clone)]
pub struct ProgramTemplateHit {
    pub col_start: u16,
    pub col_end: u16,
    pub row_start: u16,
    pub row_end: u16,
    /// Template id persisted on the program document once applied.
    pub template_id: String,
    /// The template's Markdown, dropped straight into the buffer on click.
    pub markdown: String,
}

impl ProgramTemplateHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row >= self.row_start && row <= self.row_end && col >= self.col_start && col < self.col_end
    }
}

#[derive(Debug, Clone)]
pub enum MatrixWidgetHitKind {
    Select { panel_id: String },
}

#[derive(Debug, Clone)]
pub struct MatrixWidgetHit {
    pub kind: MatrixWidgetHitKind,
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

impl MatrixWidgetHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
}

/// A widget panel shown transiently because the cursor is over its title
/// square (or, briefly after a create/update, auto-revealed). `until` is the
/// expiry; each hover frame pushes it out. Cleared when it lapses or the cursor
/// moves to a different square.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicUiHover {
    pub session_id: String,
    pub panel_id: String,
    pub until: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionTitleMenuAction {
    Rename,
    SplitHorizontal,
    SplitVertical,
    CloseSplit,
    Archive,
    Delete,
}

impl SessionTitleMenuAction {
    pub const ALL: [Self; 6] = [
        Self::Rename,
        Self::SplitHorizontal,
        Self::SplitVertical,
        Self::CloseSplit,
        Self::Archive,
        Self::Delete,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Rename => "rename",
            Self::SplitHorizontal => "split horizontal",
            Self::SplitVertical => "split vertical",
            Self::CloseSplit => "close split",
            Self::Archive => "archive",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionTitleMenu {
    pub session_id: String,
    pub area: ratatui::layout::Rect,
}

impl SessionTitleMenu {
    pub fn item_at(&self, col: u16, row: u16) -> Option<SessionTitleMenuAction> {
        if col <= self.area.x
            || col
                >= self
                    .area
                    .x
                    .saturating_add(self.area.width)
                    .saturating_sub(1)
            || row <= self.area.y
            || row
                >= self
                    .area
                    .y
                    .saturating_add(self.area.height)
                    .saturating_sub(1)
        {
            return None;
        }
        let idx = row.saturating_sub(self.area.y).saturating_sub(1) as usize;
        SessionTitleMenuAction::ALL.get(idx).copied()
    }

    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.area.x
            && col < self.area.x.saturating_add(self.area.width)
            && row >= self.area.y
            && row < self.area.y.saturating_add(self.area.height)
    }
}

/// Operator-rain analogue of [`DynamicUiHover`]: the rain panel shows a single
/// widget at a time, so only the panel id and expiry are needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixWidgetHover {
    pub panel_id: String,
    pub until: Instant,
}

pub struct App {
    pub client: Arc<Client>,
    /// Last `(session, view)` reported to the daemon via `set_view`, so we only
    /// re-send on change. Drives the AskUserQuestion chat-gate.
    last_reported_view: Option<(String, agentd_protocol::ClientView)>,
    pub sessions: Vec<SessionSummary>,
    pub groups: Vec<GroupSummary>,
    pub selection: Selection,
    pub focus: PaneFocus,
    pub main_windows: MainWindowTree,
    pub active_window_id: u64,
    pub next_window_id: u64,
    pub subagent_collapsed: HashSet<String>,
    pub transcript: Vec<TimestampedEvent>,
    pub transcript_session: Option<String>,
    pub transcript_scroll: u16,
    pub minibuffer: Option<Minibuffer>,
    pub harnesses: Vec<HarnessInfo>,
    /// Program templates offered as clickable buttons in the empty-program
    /// placeholder. Fetched at startup and on reconnect, and refreshed in the
    /// background every time the program pane opens so edits to template files
    /// (or newly dropped files) appear on the next open without a daemon restart.
    pub program_templates: Vec<agentd_protocol::ProgramTemplate>,
    /// Background channel for live-reloaded program templates. `open_program_popup`
    /// spawns a non-blocking fetch that delivers the latest list here; the event
    /// loop applies it on the next iteration (no flicker — the placeholder keeps
    /// the cached list until the fresh one lands).
    pub program_templates_tx: mpsc::UnboundedSender<Vec<agentd_protocol::ProgramTemplate>>,
    pub theme: crate::theme::Theme,
    pub help_visible: bool,
    pub profile: Profile,
    pub keymap: Keymap,
    pub chord_state: ChordState,
    pub chord_label: String,
    pub status: Option<(String, Instant)>,
    /// Persistent "update available" advisory, sourced from the upgrade cache.
    /// Unlike `status`, it is never auto-cleared on tick — it stays in the
    /// modeline until you upgrade (after which `cached_update_notice` returns
    /// None). Rendered right-aligned at the far edge of the modeline, so it
    /// coexists with a transient `status` message shown inline on the left.
    pub update_notice: Option<String>,
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
    /// gymnastics. Non-smith sessions degrade to a single
    /// `PtyChunk` and render identically to the old pipeline.
    pub histories: HashMap<String, crate::pty_render::ItemHistory>,
    /// Session ids whose `ItemHistory` has already been replayed once this
    /// frame, populated by `render_terminal_for_window` and cleared at the
    /// top of every frame. A session normally appears in exactly one split
    /// pane, but a stale window-selection reassignment (e.g. the neighbor a
    /// deleted/archived session's pane falls back to) can leave two panes
    /// showing the same session at two different widths. `ItemHistory`
    /// caches a single parser sized to the *last* width it was asked to
    /// replay at, so alternating between two widths for the same session
    /// within one frame rebuilds that parser from scratch on every single
    /// call — cheap for a nearly-empty session, catastrophic for one with
    /// substantial scrollback (same failure mode as the pin-strip/split-pane
    /// thrash `pin_tile_reuses_cached_size_to_avoid_split_thrash` guards
    /// against). The second (and later) pane showing an already-rendered
    /// session this frame reuses the first pane's cached size instead of its
    /// own, trading a slightly-off-size duplicate render for avoiding the
    /// rebuild.
    pub terminal_replayed_sessions_this_frame: HashSet<String>,
    /// Per-session cached block hit-test ranges (call_id, row range
    /// within the rendered pane). Refreshed by the render functions
    /// after each `replay`. Mouse clicks in the PTY pane consult
    /// this to toggle the right block.
    pub block_hits: HashMap<String, Vec<crate::pty_render::BlockHitRect>>,
    /// Screen rects of the matrix-rain horizontal reveal words rendered
    /// this frame, each tagged with the session that produced the word.
    /// Written by `render_matrix_rain`, consumed by mouse hover (tooltip)
    /// and click (switch to the session). Reset every frame.
    pub matrix_reveal_hits: Vec<MatrixRevealHit>,
    /// The orchestrator panel's most recent inner (cols, rows) as
    /// computed during render. Written by `ui::render`, consumed by
    /// `run_loop`'s debounce — once the value stays stable for
    /// `RESIZE_DEBOUNCE_MS`, a single `pty_resize` IPC fires.
    pub orchestrator_desired_size: Option<(u16, u16)>,
    pub terminal_pane_size: (u16, u16), // (cols, rows) of the right pane.
    /// Desired PTY size per split window, keyed by main-window id. Split panes
    /// can have different widths/heights, so adapters like claude need the
    /// focused split's actual inner area rather than the whole right pane.
    pub window_pane_sizes: HashMap<u64, (u16, u16)>,
    /// Zoom: hide list / pin strip / modeline; the session view fills the
    /// screen except for the minibuffer line at the bottom. Toggled with
    /// `C-x z` (emacs) / `z` (vim), matching tmux's prefix-z.
    pub zoom: ZoomMode,
    /// User-controlled scroll offset for the session list. 0 = first item at
    /// top. Mouse wheel over the list adjusts this; keyboard selection still
    /// lets ratatui pull the selected item back into view when needed.
    pub list_scroll_offset: usize,
    /// Scrollback offset (in rows) applied to the active/focused session's PTY
    /// parser when rendering zoomed or single-window views. Split windows keep
    /// their own offsets in `window_scrollback` so mouse-wheel scrolling one
    /// split does not move its siblings. 0 = live view.
    pub view_scrollback: usize,
    /// Per-split-window PTY scrollback offsets, keyed by main-window id.
    pub window_scrollback: HashMap<u64, usize>,
    /// Per-split-window view mode (transcript/chat vs live terminal), keyed by
    /// main-window id. Lets `C-x t` toggle only the focused split: each pane
    /// remembers its own mode across focus changes instead of sharing the
    /// single global `view`. Absent an entry, a window falls back to its
    /// session's natural mode (Terminal for PTY sessions, Chat otherwise).
    pub window_views: HashMap<u64, ViewMode>,
    /// Show the terminal scrollback overlay until this instant, keyed by
    /// main-window id. Refreshed by wheel/key scrollback input for the window
    /// being scrolled and hidden automatically after a short idle delay,
    /// similar to editor overlay scrollbars. Keyed per window so scrolling one
    /// split does not flash the scrollbar over its (at-bottom) siblings.
    pub terminal_scrollbar_visible_until: HashMap<u64, Instant>,
    /// Set by an event handler when the just-handled event produced
    /// no local display change that needs an immediate repaint — the
    /// canonical case being a keystroke forwarded straight to a PTY,
    /// whose visible effect arrives later as PTY output (which
    /// triggers its own redraw). The run loop honors this to skip a
    /// wasted `terminal.draw()` per repeated keystroke; the 120ms
    /// tick is a safety net so nothing can stay stale for long.
    /// Reset to false every loop iteration.
    pub skip_redraw_after_event: bool,
    /// Set by `on_notification` to report whether the just-handled
    /// notification changed something currently *visible* (a focused /
    /// split / pinned pane, the orchestrator panel, or any structural /
    /// status change shown in the list). The run loop reads it to avoid a
    /// full-frame `terminal.draw()` for the high-frequency background
    /// `Pty` chunks of off-screen sessions — those only warm history /
    /// feed the spinner + matrix rain, which animate on the 120ms tick
    /// anyway. Defaults to `true` (every notification kind except an
    /// off-screen `Pty` forces a redraw), so the gate can never *miss* a
    /// needed repaint. A heartbeat in the loop still forces a draw at
    /// least every tick under sustained background load.
    pub notification_dirtied_view: bool,
    /// Sessions whose selected/pinned terminal history is being rehydrated in
    /// the background. Renderers use this to show a loading placeholder while
    /// the TUI stays responsive instead of blocking startup on full transcript
    /// replay.
    pub hydrating_sessions: HashSet<String>,
    /// Scrollback offset for the daemon-owned orchestrator panel rendered in
    /// the minibuffer. Kept separate from `view_scrollback` so reading operator
    /// history does not leave the main session view scrolled when the panel
    /// closes.
    pub orchestrator_scrollback: usize,
    /// Active operator monolog typewritten over the matrix rain (`None` = rain).
    pub operator_monolog: Option<OperatorMonolog>,
    /// Accumulates the orchestrator's streaming assistant text across the
    /// current turn; consolidated into `operator_monolog` at turn end.
    pub operator_utterance: String,
    /// User-preferred height for the daemon-owned orchestrator panel rendered
    /// in the minibuffer. Clamped by terminal height at render time.
    pub orchestrator_panel_h: Option<u16>,
    /// `Some((anchor_row, anchor_height))` while the user drags the
    /// orchestrator panel's top border.
    pub resizing_orchestrator_panel: Option<(u16, u16)>,
    /// `Some((thumb_grab_offset, max_scrollback))` while dragging the terminal
    /// scrollbar thumb. `thumb_grab_offset` is the row delta from the thumb top
    /// to the cursor at mouse-down, so dragging preserves where the user grabbed.
    pub dragging_terminal_scrollbar: Option<(u16, usize)>,
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
    /// Session-view title hamburger dropdown.
    pub session_title_menu: Option<SessionTitleMenu>,
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
    /// in the sessions list. The orchestrator runs as a smith
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
    /// Split-window divider drag: parent split id, direction, drag-start
    /// coordinate, drag-start ratio, and parent split area.
    pub resizing_main_window: Option<(u64, WindowSplitDirection, u16, u16, ratatui::layout::Rect)>,
    pub resizing_program_popup: Option<()>,
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
    /// Reusable session-picker dialog (spec 0063): `None` = closed. Opened by
    /// `C-x b` (switch the active window's session) and by the program view's
    /// `@`→session path (insert a session clip). Captures all input while open.
    pub session_picker: Option<SessionPickerDialog>,
    /// Selected session's program, rendered in an in-TUI modal.
    pub program_popup: Option<ProgramPopup>,
    /// Open program popups for sessions that are not currently selected.
    /// Presence means the program should be restored when that session is
    /// focused again, including unsaved draft text and cursor state.
    pub program_popups: HashMap<String, ProgramPopup>,
    /// Remembered caret + scroll for each session's program, captured when the
    /// program view is hidden so reopening it lands on the same position. This
    /// is intentionally distinct from `program_popups`: that map drives split-
    /// window rendering, so a fully-hidden program must not live there (it would
    /// re-render in a split), yet its caret/scroll must still survive a
    /// hide→show cycle. Keyed by session id; consumed on the next open.
    pub program_view_memory: HashMap<String, ProgramViewMemory>,
    /// In-flight program Run animations, keyed by session id (spec 0042). An
    /// entry means a program Run is believed to still be executing for that
    /// session; it drives the shimmer over the executed Markdown.
    pub program_runs: HashMap<String, ProgramRun>,
    /// Run overlap/idempotency guard (spec 0042 consequence), keyed by
    /// `(session_id, is_selection, executed-body hash)`. A duplicate Run
    /// gesture — a double `C-x C-r`, a double-click on a Run button — for the
    /// exact same session/scope/body is coalesced into whichever dispatch is
    /// already in flight or just completed, rather than sending a second
    /// `program.execute`. Keying on the executed body (not just session)
    /// means a selection Run, a different selection, or a full re-Run whose
    /// body changed always dispatches — only a truly identical repeat is
    /// suppressed. See `execute_program_popup`.
    pub(crate) program_run_dispatch: HashMap<ProgramRunDispatchKey, ProgramRunDispatchState>,
    /// Recently-settled program block refs, keyed by session id then block ref.
    /// Renderers turn these into a short one-shot flourish and prune them after
    /// `PROGRAM_SETTLE_FLASH_MS`.
    pub program_settle_flourishes: HashMap<String, HashMap<String, Instant>>,
    /// Ephemeral Program collaboration cursors, keyed by daemon client id.
    pub program_collaborators: HashMap<String, agentd_protocol::ProgramCursor>,
    /// Local receipt clock for agent-cursor freshness (spec 0065 agent
    /// presence), keyed by daemon client id: the agent cursor's
    /// `updated_at_ms` last observed for that client, alongside the local
    /// `Instant` it first arrived at. The reveal/edge-indicator gates key off
    /// this instead of the daemon's `updated_at_ms` directly — broadcast
    /// transit plus the render tick can eat most of a short reveal window
    /// before the first paint, so the daemon stamp alone makes the reveal
    /// invisible. Only bumped when the daemon stamp itself advances, so a
    /// rebase (position change, unchanged `updated_at_ms`) does not renew it.
    pub program_agent_reveal_receipts: HashMap<String, (i64, Instant)>,
    pub own_program_client_id: Option<String>,
    pub program_clipboard: Option<String>,
    /// Live `/remote-control` modal — URL + QR for the active
    /// remote-WS deployment. `Some` while open, `None` otherwise.
    /// Dismissed with Esc the same way `tasks_popup` is.
    pub remote_control_popup: Option<RemoteControlPopup>,
    pub remote_control_task:
        Option<tokio::task::JoinHandle<(bool, Result<agentd_protocol::RemoteStartResult>)>>,
    /// Per-session input editor state, fed by `SessionEvent::EditorState`
    /// from the adapter (currently smith interactive). Drives the
    /// fixed bottom input pane.
    pub editor_states: HashMap<String, EditorState>,
    /// Per-session live agent status, fed by `SessionEvent::AgentStatus`
    /// and rendered above queued input while a turn is active.
    pub agent_statuses: HashMap<String, agentd_protocol::AgentStatus>,
    /// Pending tool approvals by session id. Orchestrator approvals stay inline
    /// in the Operator PTY, but this lets the Matrix title bar surface a clear
    /// attention marker while the prompt is waiting.
    pub pending_tool_approvals: HashMap<String, HashSet<String>>,
    /// Latest browser preview per session, fed by `SessionEvent::BrowserPreview`
    /// and rendered as a top-right overlay in the terminal view.
    pub browser_previews: HashMap<String, BrowserPreviewState>,
    /// Adapter/file-backed dynamic UI panels, keyed by session id then panel id.
    /// Actions route back as normal session input.
    pub ui_panels: HashMap<String, HashMap<String, agentd_protocol::UiPanel>>,
    /// Currently open widget selector dropdown session, if any.
    pub dynamic_ui_popover_open: Option<String>,
    /// Widgets explicitly selected by the user. Default state is hidden.
    pub dynamic_ui_selected: HashSet<(String, String)>,
    /// Widgets temporarily shown after create/update. Hover extends this deadline.
    pub dynamic_ui_temporary_until: HashMap<(String, String), Instant>,
    /// Widget previewed purely because the cursor is over its title square (or
    /// the widget body). At most one across the fleet — the pointer is over one
    /// square at a time. Kept apart from `dynamic_ui_temporary_until` so a
    /// preview switches instantly between squares and uses the short hover grace
    /// rather than the 15s auto-reveal.
    pub dynamic_ui_hover: Option<DynamicUiHover>,
    /// The one widget panel currently focused for keyboard handling.
    pub dynamic_ui_focused: Option<(String, String)>,
    /// Per-session scroll offset for the stacked dynamic UI widget panel.
    /// This scrolls the whole widget stack independently from terminal scrollback.
    pub dynamic_ui_scroll_offsets: HashMap<String, usize>,
    /// MRU cache of resized preview images shared by the terminal-view
    /// overlay and the matrix-rain wallpaper — avoids re-downscaling the
    /// screenshot every frame. See [`ImageResizeCache`].
    pub image_resize_cache: ImageResizeCache,
    /// Short visual transition when a window explicitly switches to a
    /// different session. Keyed per main-window id so split panes don't
    /// glitch together.
    pub session_transitions: HashMap<u64, SessionTransition>,
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
    /// Operator widget pinned open by a click on its title square. Persistent
    /// until clicked again (or the panel is deleted) — survives the cursor
    /// leaving the rain panel, unlike a hover preview.
    pub matrix_widget_pinned: Option<String>,
    /// Operator widget shown transiently on hover. Takes visual precedence over
    /// `matrix_widget_pinned` while live, then reverts to the pin when it lapses.
    pub matrix_widget_hover: Option<MatrixWidgetHover>,
    /// User-hidden Matrix-rain panel. Toggle with `/rain`; close with the
    /// panel's `x` button.
    pub matrix_rain_hidden: bool,
    /// Whether the ungrouped top-level section's archived sessions are
    /// revealed. Toggled by its "N archived" row. Ephemeral — archived
    /// sessions default to hidden on each launch.
    pub show_archived_ungrouped: bool,
    /// Project ids whose archived sessions are currently revealed, toggled by
    /// each project's "N archived" row. Ephemeral, like
    /// [`Self::show_archived_ungrouped`].
    pub show_archived_groups: HashSet<String>,
    /// Parent session ids whose archived subagents are currently revealed.
    /// Ephemeral, like the other archived disclosure state.
    pub show_archived_subagents: HashSet<String>,
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
    ui_panels: HashMap<String, agentd_protocol::UiPanel>,
    status_messages: Vec<String>,
    /// True when the pre-rendered screen was in alt-screen mode (e.g. grok,
    /// interactive codex). Used by `apply_hydration_state` to force a bump
    /// resize so the child gets SIGWINCH and repaints even when the terminal
    /// dimensions haven't changed since the last TUI session.
    history_is_alt_screen: bool,
}

struct SessionHydrationRequest {
    socket: std::path::PathBuf,
    session_id: String,
    needs_history: bool,
    terminal_pane_size: (u16, u16),
    /// Whether the session is headless (no PTY). Headless sessions carry
    /// their conversation as structured Message/Reasoning events, which
    /// replay folds into the items history; PTY-backed sessions don't.
    is_headless: bool,
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

/// The operator's latest finalized utterance, typewritten over the matrix
/// rain as an ambient "monolog" then auto-cleared. Lets the user see what the
/// operator said without opening the (collapsed) minibuffer panel.
#[derive(Debug, Clone)]
pub struct OperatorMonolog {
    pub text: String,
    pub started_at: Instant,
}

/// Consolidate the orchestrator's streaming assistant text into a user-facing
/// monolog line, or `None` if it's empty or the internal `noted` no-op token
/// (which the operator replies when nothing needs surfacing).
pub fn operator_monolog_text(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if lower == "noted" || lower == "noted." {
        return None;
    }
    Some(t.to_string())
}

/// MRU cache of resized preview images, keyed by `(source Arc ptr,
/// out_w, out_h)`. Lets the overlay and the matrix-rain wallpaper blit
/// the same image every frame without re-running the (very expensive)
/// downscale. Kept tiny (a few entries) — see `ui::resized_image`.
pub type ImageResizeCache = Vec<((usize, u32, u32), std::sync::Arc<image::RgbaImage>)>;

/// Largest dimension (px) we keep a decoded preview at. Browser
/// screenshots arrive at full page size (often >1280px); the overlay and
/// wallpaper render into tiny cell grids, so a one-time downscale to this
/// cap makes every subsequent resize cheap with no visible quality loss.
const PREVIEW_MAX_DIM: u32 = 400;

/// How long a browser preview stays up (before its top-to-bottom erase)
/// when not hovered. Hovering keeps it and resets this on un-hover.
const BROWSER_PREVIEW_TTL: Duration = Duration::from_secs(7);

/// Decode a base64-PNG browser-preview image to a shared RGBA buffer,
/// downscaled once to `PREVIEW_MAX_DIM`. `None` if the base64 or the
/// image fails to decode. Done once on insert so per-frame rendering
/// (overlay + matrix wallpaper) only does a small resize/blit.
pub fn decode_browser_preview_image(b64: &str) -> Option<std::sync::Arc<image::RgbaImage>> {
    use base64::Engine;
    let png = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let img = image::load_from_memory(&png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let img = if w.max(h) > PREVIEW_MAX_DIM {
        let scale = PREVIEW_MAX_DIM as f32 / w.max(h) as f32;
        let nw = ((w as f32 * scale).round() as u32).max(1);
        let nh = ((h as f32 * scale).round() as u32).max(1);
        // `thumbnail` is an averaging downscaler — faster than the
        // general resampler and fine for this one-time shrink.
        image::imageops::thumbnail(&img, nw, nh)
    } else {
        img
    };
    Some(std::sync::Arc::new(img))
}

#[derive(Debug, Clone)]
pub struct BrowserPreviewState {
    pub hide_after: Instant,
    pub hover_started: Option<Instant>,
    /// When this preview first arrived — drives the matrix-rain
    /// wallpaper's top-to-bottom "dial-up" reveal.
    pub revealed_at: Instant,
    /// PNG decoded to RGBA once on insert, shared (`Arc`) so both the
    /// terminal-view overlay and the matrix-rain wallpaper can blit it
    /// every frame without re-decoding. `None` if the bytes failed to
    /// decode.
    pub decoded: Option<std::sync::Arc<image::RgbaImage>>,
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
        let detail: agentd_protocol::SessionDetail = blocking_request(
            &req.socket,
            agentd_protocol::ipc_method::SESSION_GET,
            &agentd_protocol::SessionIdParams {
                session_id: req.session_id.clone(),
            },
        )?;
        let transcript = agentd_protocol::TranscriptResult {
            total: detail.events.len() as u64,
            events: detail.events,
        };

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
            let mut ui_panels: HashMap<String, agentd_protocol::UiPanel> = detail
                .ui_panels
                .iter()
                .cloned()
                .map(|panel| (panel.id.clone(), panel))
                .collect();
            if transcript
                .events
                .iter()
                .any(|ev| matches!(ev.event, SessionEvent::Pty { .. }))
            {
                // New daemons persist PTY events in the transcript as ordering
                // markers. Prefer rebuilding from those markers so transcript-only
                // items (smith tool blocks) are interleaved with the raw bytes in
                // chronological order. The pty_replay path above remains the
                // fallback for older sessions whose transcripts do not contain PTY.
                h.clear_items();
            }
            apply_transcript_to_local_state(
                &transcript.events,
                &mut h,
                &mut editor_state,
                &mut agent_status,
                &mut ui_panels,
                req.is_headless,
            );
            let (cols, rows) = req.terminal_pane_size;
            let pre_render = h.replay(cols.max(1), rows.max(1), 0);
            let is_alt_screen = pre_render.screen.alternate_screen();
            drop(pre_render);
            (
                Some(h),
                editor_state,
                agent_status,
                ui_panels,
                is_alt_screen,
            )
        } else {
            let mut ui_panels: HashMap<String, agentd_protocol::UiPanel> = detail
                .ui_panels
                .iter()
                .cloned()
                .map(|panel| (panel.id.clone(), panel))
                .collect();
            for ev in &transcript.events {
                match &ev.event {
                    SessionEvent::UiPanel(panel) => {
                        ui_panels.insert(panel.id.clone(), panel.clone());
                    }
                    SessionEvent::UiDelete { id } => {
                        ui_panels.remove(id);
                    }
                    _ => {}
                }
            }
            (None, None, None, ui_panels, false)
        };

        Ok(SessionHydration {
            session_id: req.session_id,
            transcript: transcript.events,
            history: history.0,
            editor_state: history.1,
            agent_status: history.2,
            ui_panels: history.3,
            status_messages,
            history_is_alt_screen: history.4,
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
        // Coalesce a burst of queued keystrokes for the same session
        // into one `pty_input`. A held key (delete / arrow across a
        // long line) queues repeats faster than each RPC round-trips,
        // so the previous one-RPC-per-key loop serialized the whole
        // backlog — the dominant repeated-key latency. Concatenating
        // all immediately-available same-session bytes into a single
        // write lets the child process the burst at once and emit one
        // settled frame instead of animating every intermediate
        // keystroke. Single keystrokes (nothing else queued) are
        // unaffected — the drain finds an empty channel and sends
        // exactly those bytes. See `coalesce_pty_input`.
        let mut carried: Option<PtyInputJob> = None;
        loop {
            let first = match carried.take() {
                Some(j) => j,
                None => match rx.recv().await {
                    Some(j) => j,
                    None => break,
                },
            };
            let (session_id, bytes, label, next) = coalesce_pty_input(first, &mut rx);
            carried = next;
            if let Err(e) = client.pty_input(&session_id, bytes).await {
                let _ = err_tx.send(format!("{label} failed: {e}"));
            }
        }
    });
    (tx, err_rx)
}

/// Drain all *immediately-available* jobs for the same session as
/// `first` and concatenate their bytes into one batch. Stops at
/// the first different-session job, which is returned as `carried`
/// so the caller can start a fresh batch for it (its own burst
/// still coalesces on the next call). Pure + synchronous so it can
/// be unit-tested without a daemon — the regression guard for the
/// "one RPC per keystroke" latency bug this replaced.
fn coalesce_pty_input(
    first: PtyInputJob,
    rx: &mut mpsc::UnboundedReceiver<PtyInputJob>,
) -> (String, Vec<u8>, &'static str, Option<PtyInputJob>) {
    let session_id = first.session_id;
    let label = first.label;
    let mut bytes = first.bytes;
    let mut carried = None;
    loop {
        match rx.try_recv() {
            Ok(next) if next.session_id == session_id => {
                bytes.extend_from_slice(&next.bytes);
            }
            Ok(next) => {
                carried = Some(next);
                break;
            }
            Err(_) => break,
        }
    }
    (session_id, bytes, label, carried)
}

/// State for the `/tasks` modal popup. v1 is read-only at the UI
/// layer (Esc closes; clicks outside close); re-typing `/tasks`
/// refreshes the snapshot.
#[derive(Debug, Clone)]
pub struct TasksPopup {
    pub session_id: String,
    pub tasks: Vec<agentd_protocol::TaskInfo>,
}

/// In-TUI program surface for the selected session. The renderer treats
/// Markdown as source and projects smart clips as chips/blocks.
#[derive(Debug, Clone)]
pub struct ProgramPopup {
    pub program: agentd_protocol::ProgramDocument,
    pub buffer: String,
    pub saved_markdown: String,
    pub blocks: Vec<agentd_protocol::ProgramBlockView>,
    pub undo_stack: Vec<ProgramUndoState>,
    pub cursor: usize,
    pub preferred_col: Option<usize>,
    pub selection: Option<ProgramSelection>,
    pub smart_clip: Option<ProgramSmartClipSearch>,
    pub search: Option<ProgramSearch>,
    pub revealed_at: Instant,
    pub hide_after: Instant,
    pub closing: bool,
    /// Vertical scroll offset measured in wrapped (visual) rows — the number of
    /// rows skipped off the top so the body can scroll when it overflows the
    /// viewport. Cursor moves follow the caret; the mouse wheel scrolls freely.
    pub scroll_offset: usize,
    /// Percent of the owning pane height covered by the roll-down Program
    /// surface. Defaults to roughly two thirds so the bottom third of the
    /// session terminal remains visible.
    pub cover_percent: u16,
    /// True while this rolled-down Program is slid aside because the terminal
    /// it exposes holds keyboard focus. Keys flow to the underlying session
    /// instead of editing Program Markdown. Per-popup (not on `App`) so that
    /// focusing a different split window leaves this popup's slide untouched:
    /// the popup stays slid in its own pane and is still slid when its window
    /// regains focus.
    pub terminal_focus: bool,
    /// Terminal-focus slide animation: the slide fraction this popup had when
    /// `terminal_focus` last flipped (0.0 anchored, 1.0 fully slid), and when
    /// the flip happened. The renderer eases from this fraction toward the
    /// focus target over `PROGRAM_REVEAL_MS`, so reversing focus mid-slide
    /// resumes from the popup's current position instead of snapping. Flip
    /// focus via [`ProgramPopup::set_terminal_focus`].
    pub slide_from: f32,
    pub slide_changed_at: Option<Instant>,
}

impl ProgramPopup {
    /// Flip keyboard focus between this rolled-down Program and the terminal
    /// it exposes. Every flip goes through here so the popup's terminal-focus
    /// slide animates instead of snapping: the current in-flight fraction is
    /// captured as the new starting point, so reversing focus mid-slide
    /// resumes from wherever the popup is rather than jumping to an endpoint.
    pub(crate) fn set_terminal_focus(&mut self, focused: bool) {
        if self.terminal_focus == focused {
            return;
        }
        let now = Instant::now();
        self.slide_from = self.slide_fraction(now);
        self.terminal_focus = focused;
        self.slide_changed_at = Some(now);
    }

    /// Current terminal-focus slide fraction: 0.0 = anchored at the pane's
    /// left edge, 1.0 = fully slid right. Eases linearly from `slide_from`
    /// toward the focus target over `PROGRAM_REVEAL_MS` (the same duration as
    /// the roll-down reveal).
    pub(crate) fn slide_fraction(&self, now: Instant) -> f32 {
        let target = if self.terminal_focus { 1.0 } else { 0.0 };
        let Some(changed_at) = self.slide_changed_at else {
            return target;
        };
        let progress = (now.saturating_duration_since(changed_at).as_secs_f32()
            / (PROGRAM_REVEAL_MS as f32 / 1000.0))
            .clamp(0.0, 1.0);
        self.slide_from + (target - self.slide_from) * progress
    }
}

/// Caret + scroll of a program view, remembered across a hide→show cycle. When
/// the program is hidden the active popup is dropped (it must not linger in
/// `program_popups`, which renders split windows); this snapshot lets a later
/// reopen restore the exact position the user left, even though the document is
/// re-fetched fresh from the daemon.
#[derive(Debug, Clone)]
pub struct ProgramViewMemory {
    /// Caret as a char offset into the buffer. Clamped to the (possibly changed)
    /// buffer length on restore.
    pub cursor: usize,
    /// Preferred column for vertical motion, carried so an up/down right after
    /// reopening behaves as it did before hiding.
    pub preferred_col: Option<usize>,
    /// Vertical scroll offset in wrapped rows. An out-of-range value is clamped
    /// by the renderer, so a shrunk document can't scroll past its end.
    pub scroll_offset: usize,
    pub cover_percent: u16,
}

pub(crate) const PROGRAM_COVER_PERCENT_DEFAULT: u16 = 67;
pub(crate) const PROGRAM_COVER_PERCENT_MIN: u16 = 30;
pub(crate) const PROGRAM_COVER_PERCENT_MAX: u16 = 100;

/// Key for the Run overlap/idempotency guard: the session a Run targets, the
/// scope (`true` = selection, `false` = whole program), and a hash of the
/// executed body. See `App::program_run_dispatch`.
pub(crate) type ProgramRunDispatchKey = (String, bool, u64);

/// State of one `ProgramRunDispatchKey` in `App::program_run_dispatch`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ProgramRunDispatchState {
    /// The save/execute round trip for this exact Run is still awaited. A
    /// second identical Run arriving while this is set is suppressed
    /// outright, regardless of how long ago the first began.
    InFlight,
    /// The Run was dispatched (its `program.execute` request was sent) at
    /// this instant. An identical repeat within `PROGRAM_RUN_DEDUP_WINDOW_MS`
    /// of this instant is suppressed; after that it dispatches again.
    Dispatched(Instant),
}

/// In-flight program Run animation state for one session (spec 0042). Present
/// while a program Run this client issued is believed to still be executing in
/// the owning session; drives the "shimmer" over the executed Markdown.
#[derive(Debug, Clone)]
pub struct ProgramRun {
    /// When Run was pressed. The shimmer wave is a function of elapsed time.
    pub started_at: Instant,
    /// Stable block refs (plus legacy fallback content ids) of blocks still
    /// pending, per spec 0053. The daemon owns the set and publishes it; clients
    /// map it back onto source lines through the daemon block projection when
    /// the buffer is clean and through content ids only for dirty fallback.
    pub pending: HashSet<String>,
    /// Per-block run-status tooltips keyed by stable block ref (spec 0057). Missing
    /// entries render the hardcoded fallback label on hover.
    pub pending_tooltips: HashMap<String, String>,
    /// Daemon-derived run-level fallback for pending blocks with no
    /// agent-authored tooltip.
    pub system_status: Option<String>,
    /// Absolute backstop: clear no later than this regardless of signals.
    pub deadline: Instant,
    /// Whether the first output has been observed.
    pub first_output_seen: bool,
    /// Compact run-pipeline stage derived by the daemon, or Pressed for this
    /// client's optimistic pre-response state.
    pub stage: agentd_protocol::ProgramRunStage,
    /// True once this run has been observed in a daemon payload. Local
    /// optimistic starts stay false until program.execute or program/state
    /// confirms them.
    pub daemon_confirmed: bool,
    /// Blocks settled from the run's initial pending set.
    pub settled_block_count: usize,
    /// Blocks in the run's initial pending set.
    pub total_block_count: usize,
}

impl ProgramRun {
    fn from_progress(mut progress: agentd_protocol::ProgramRunProgress) -> Option<Self> {
        progress.refresh_stage();
        let now = Instant::now();
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_millis() as i64;
        if progress.expires_at_ms <= now_ms
            || (progress.pending_block_refs.is_empty() && progress.pending_block_ids.is_empty())
        {
            return None;
        }
        let started_at = if progress.started_at_ms <= now_ms {
            now - Duration::from_millis((now_ms - progress.started_at_ms) as u64)
        } else {
            now
        };
        let deadline = now + Duration::from_millis((progress.expires_at_ms - now_ms) as u64);
        let mut pending: HashSet<String> = progress.pending_block_refs.into_iter().collect();
        pending.extend(progress.pending_block_ids);
        Some(Self {
            started_at,
            pending,
            pending_tooltips: progress.pending_block_tooltips,
            system_status: progress.system_status,
            deadline,
            first_output_seen: progress.first_output_seen,
            stage: progress.stage,
            daemon_confirmed: true,
            settled_block_count: progress.settled_block_count,
            total_block_count: progress.total_block_count,
        })
    }
}

/// A program "block": a maximal run of consecutive non-blank Markdown lines,
/// identified by its legacy content id for dirty-buffer fallback. The daemon
/// projection supplies authoritative stable refs for synced documents.
pub(crate) struct ProgramBlock {
    /// Source-line index range `[start_line, end_line)` into `markdown.lines()`.
    pub start_line: usize,
    pub end_line: usize,
    /// Legacy content-derived block id (spec 0053).
    pub id: String,
}

/// Split Markdown into blocks (runs of consecutive non-blank lines), via the
/// shared protocol parser so the TUI and the daemon agree on block boundaries
/// and ids. The renderer uses this to decide which source lines shimmer.
pub(crate) fn program_blocks(markdown: &str) -> Vec<ProgramBlock> {
    agentd_protocol::program_block_spans(markdown)
        .into_iter()
        .map(|span| ProgramBlock {
            start_line: span.start_line,
            end_line: span.end_line,
            id: span.id,
        })
        .collect()
}

/// Session ids referenced by `@{session:…}` smart clips anywhere in `markdown`,
/// in first-seen order and deduplicated. Used to keep the referenced worker
/// sessions' PTY history warm so the program hover preview (spec 0060) can paint
/// a live terminal tail the instant the pointer lands. `@{harness:…}` and other
/// clip kinds are ignored — only sessions have a terminal to preview.
pub(crate) fn program_referenced_session_ids(markdown: &str) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    let mut rest = markdown;
    // `@`, `{`, `}` are ASCII so byte-`find` lands on char boundaries.
    while let Some(open) = rest.find("@{") {
        let after = &rest[open + 2..];
        let Some(close) = after.find('}') else {
            break;
        };
        let body = &after[..close];
        // Body looks like `session:abc` or `session:abc clip_id=3`; the kind/id
        // pair is the first whitespace-delimited token (mirrors the clip target
        // parse used for rendering).
        let token = body.split_whitespace().next().unwrap_or(body);
        if let Some(("session", id)) = token.split_once(':') {
            if !id.is_empty() && !ids.iter().any(|existing| existing == id) {
                ids.push(id.to_string());
            }
        }
        rest = &after[close + 1..];
    }
    ids
}

/// Legacy content ids of the blocks contained in `body` — used for local
/// optimistic/selection shimmer before the daemon returns stable refs.
pub(crate) fn program_run_pending_ids(body: &str) -> HashSet<String> {
    agentd_protocol::program_block_spans(body)
        .into_iter()
        .map(|span| span.id)
        .collect()
}

fn program_run_progress_pending_ids(
    progress: &agentd_protocol::ProgramRunProgress,
) -> HashSet<String> {
    let mut pending: HashSet<String> = progress.pending_block_refs.iter().cloned().collect();
    pending.extend(progress.pending_block_ids.iter().cloned());
    pending
}

/// Next `program_agent_reveal_receipts` entry for an agent cursor carrying
/// `updated_at_ms`, given the entry currently on file (if any) and the local
/// time `now` this cursor was received at (spec 0065 agent presence).
///
/// Renews the receipt (bumps the local `Instant` to `now`) only when
/// `updated_at_ms` itself has advanced past what's on file — a genuine new
/// agent write. When the incoming stamp matches what's already recorded, the
/// cursor was rebased through someone else's edit without the daemon
/// advancing its own timestamp, so the existing receipt (and thus the
/// reveal's remaining freshness) carries over unchanged. Pure so the
/// bump-vs-hold decision is unit-testable without an `App` or live clock.
fn program_agent_reveal_receipt_update(
    existing: Option<&(i64, Instant)>,
    updated_at_ms: i64,
    now: Instant,
) -> (i64, Instant) {
    match existing {
        Some((seen_ms, receipt_at)) if *seen_ms == updated_at_ms => (*seen_ms, *receipt_at),
        _ => (updated_at_ms, now),
    }
}

/// Result of flushing a program popup's buffer to the daemon.
struct ProgramSaveOutcome {
    /// The document as it now lives on the daemon (our content, possibly
    /// merged with concurrent edits).
    program: agentd_protocol::ProgramDocument,
    /// Echoed per-block projection for stable shimmer refs.
    blocks: Vec<agentd_protocol::ProgramBlockView>,
    /// A 3-way merge ran because the document advanced underneath us.
    merged: bool,
    /// The merge could not reconcile overlapping edits, so the saved content
    /// carries conflict markers for the user (or agent) to resolve.
    conflicted: bool,
}

#[derive(Debug, Clone)]
pub struct ProgramSelection {
    pub anchor: usize,
    pub head: usize,
    pub dragged: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramUndoState {
    buffer: String,
    cursor: usize,
    preferred_col: Option<usize>,
    selection: Option<ProgramSelection>,
    smart_clip: Option<ProgramSmartClipSearch>,
    scroll_offset: usize,
}

#[derive(Debug, Clone)]
pub struct ProgramSmartClipSearch {
    pub trigger_start: usize,
    pub selected: usize,
    /// Which menu level is on screen. The picker opens at [`ProgramSmartClipView::Root`]
    /// (top-relevance section + category headers) and drills into a category's
    /// full submenu when one is activated.
    pub view: ProgramSmartClipView,
}

/// The level the `@` smart-clip picker is showing. `Root` is the two-part menu
/// (up-to-5 most-relevant clips, a separator, then expandable category rows).
/// `Submenu` is one category's full, list-view-ordered set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramSmartClipView {
    Root,
    Submenu(ProgramSmartClipGroup),
}

/// One rendered/​navigable line of the smart-clip picker. The picker is built as
/// a flat row list so the TUI and web UI share the same shape: selectable rows
/// ([`Self::Clip`], [`Self::Category`]) interleave with non-selectable
/// decoration ([`Self::Separator`], [`Self::Header`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProgramSmartClipRow {
    /// A selectable clip. `dimmed` is set inside a submenu for items that do not
    /// match the active type-ahead query — shown (so the full list stays
    /// visible) but de-emphasized so matches stand out.
    Clip {
        candidate: ProgramSmartClipCandidate,
        dimmed: bool,
    },
    /// Divider between the top relevance section and the category list.
    Separator,
    /// A root-view category header that expands into a submenu. `count` is the
    /// number of clips the submenu holds.
    Category {
        group: ProgramSmartClipGroup,
        count: usize,
    },
    /// A non-selectable project/group header inside the session submenu, mirroring
    /// the session-list view's grouping.
    Header(String),
}

impl ProgramSmartClipRow {
    pub fn is_selectable(&self) -> bool {
        matches!(self, Self::Clip { .. } | Self::Category { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ProgramSearch {
    pub anchor_cursor: usize,
    pub query: String,
    pub matches: Vec<(usize, usize)>,
    pub selected: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramSmartClipCandidate {
    pub group: ProgramSmartClipGroup,
    pub clip: String,
    pub label: String,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramSmartClipGroup {
    Session,
    Harness,
}

impl ProgramSmartClipGroup {
    pub fn label(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Harness => "harness",
        }
    }
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

#[derive(Debug, Clone)]
pub struct DynamicUiActionHit {
    pub session_id: String,
    pub panel_id: String,
    pub action: agentd_protocol::UiAction,
    pub row: u16,
    pub start_col: u16,
    /// Exclusive end column.
    pub end_col: u16,
}

#[derive(Debug, Clone)]
pub struct DynamicUiInlineHit {
    pub session_id: String,
    pub panel_id: String,
    pub area: ratatui::layout::Rect,
}

#[derive(Debug, Clone)]
pub struct DynamicUiWidgetHit {
    pub session_id: String,
    pub panel_id: String,
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

#[derive(Debug, Clone)]
pub struct DynamicUiPanelCloseHit {
    pub session_id: String,
    pub panel_id: String,
    pub row: u16,
    pub start_col: u16,
    pub end_col: u16,
}

impl DynamicUiActionHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
}

/// A click target inside a widget panel that opens an external URL when
/// the user clicks it. Populated for `[label](http://…)` / `[label](https://…)`
/// links by `render_inline_action_spans`; dispatched by
/// `handle_dynamic_ui_overlay_click` via [`open_url`]. Kept in a parallel
/// list rather than folded into [`DynamicUiActionHit`] so the action type
/// (which lives in `agentd_protocol::UiAction`) doesn't have to grow a
/// URL variant — widgets that mix action links and plain http links work
/// independently.
#[derive(Debug, Clone)]
pub struct DynamicUiUrlHit {
    pub session_id: String,
    pub panel_id: String,
    pub url: String,
    pub row: u16,
    pub start_col: u16,
    /// Exclusive end column.
    pub end_col: u16,
}

impl DynamicUiUrlHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
}

impl DynamicUiWidgetHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
}

impl DynamicUiPanelCloseHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalScrollbarHit {
    pub area: ratatui::layout::Rect,
    pub thumb: ratatui::layout::Rect,
    pub max_scrollback: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelineApprovalModeHit {
    pub row: u16,
    pub start_col: u16,
    /// Exclusive end column.
    pub end_col: u16,
}

impl ModelineApprovalModeHit {
    pub fn contains(&self, col: u16, row: u16) -> bool {
        row == self.row && col >= self.start_col && col < self.end_col
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowPaneHit {
    pub id: u64,
    pub area: ratatui::layout::Rect,
    pub inner_area: ratatui::layout::Rect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowDividerHit {
    pub parent: u64,
    pub direction: WindowSplitDirection,
    pub area: ratatui::layout::Rect,
    pub parent_area: ratatui::layout::Rect,
    pub ratio_percent: u16,
}

/// Last-frame geometry for hit-testing mouse clicks.
#[derive(Debug, Clone, Default)]
pub struct LayoutSnapshot {
    pub list_area: Option<ratatui::layout::Rect>,
    pub view_area: Option<ratatui::layout::Rect>,
    pub main_window_areas: Vec<WindowPaneHit>,
    pub main_window_dividers: Vec<WindowDividerHit>,
    pub pin_strip_area: Option<ratatui::layout::Rect>,
    pub matrix_rain_area: Option<ratatui::layout::Rect>,
    pub minibuffer_area: Option<ratatui::layout::Rect>,
    /// Last rendered chat areas by session id to conditionally clear only
    /// when geometry grows (editor shrinks), avoiding per-frame clears.
    pub last_chat_areas: std::collections::HashMap<String, ratatui::layout::Rect>,
    /// Clickable approval-mode badge in the modeline for the selected session.
    pub modeline_approval_mode_hit: Option<ModelineApprovalModeHit>,
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
    /// Clickable shortcut labels in the last frame (minibuffer hints,
    /// empty-state onboarding shortcuts). Empty when a minibuffer prompt
    /// (palette / send-input / etc.) is open for minibuffer hints, but may
    /// still contain main-view shortcut affordances.
    pub shortcut_hints: Vec<HintZone>,
    /// Clickable harness names in the new-session picker prompt
    /// (`MinibufferIntent::NewSessionHarness`). Click → submit the
    /// matching name as if the user typed it and hit Enter.
    pub minibuffer_harness_hits: Vec<HarnessHit>,
    /// Bounds of the topmost modal/dialog rendered in the last frame.
    /// Mouse clicks outside this rect dismiss the modal instead of
    /// falling through to panes underneath it.
    pub modal_area: Option<ratatui::layout::Rect>,
    /// Program title-bar Run button bounds: `(x_start, x_end, y)`.
    pub program_title_run_hit: Option<(u16, u16, u16)>,
    /// Program title-bar mode toggle bounds: `(x_start, x_end, y)`.
    pub program_title_toggle_hit: Option<(u16, u16, u16)>,
    /// Program title-bar close button bounds: `(x_start, x_end, y)`.
    pub program_title_close_hit: Option<(u16, u16, u16)>,
    /// Program selected-text context Run button bounds: `(x_start, x_end, y)`.
    pub program_selection_run_hit: Option<(u16, u16, u16)>,
    /// Inner content rect of the active program popup from the last frame.
    /// Cursor-move handlers and the mouse wheel read its width/height to keep
    /// the caret on-screen and to bound scrolling; `None` when no program is open.
    pub program_inner_area: Option<ratatui::layout::Rect>,
    /// Full pane rect the active Program is rolled down inside.
    pub program_base_area: Option<ratatui::layout::Rect>,
    /// Bottom border row of the active Program; dragging it resizes coverage.
    pub program_resize_hit: Option<ratatui::layout::Rect>,
    /// `@`-smart-clip anchor from the last frame: the editor cursor `Position`
    /// the inline picker hangs from, plus the program's inner `Rect`. The
    /// session-picker dialog reads this to render its `@`→session variant in
    /// place of the inline context menu instead of center-screen. `Some` only
    /// while a program's `@` smart-clip search is live.
    pub program_smart_clip_anchor: Option<(ratatui::layout::Position, ratatui::layout::Rect)>,
    /// Session smart-clip hitboxes in the active program body from the last
    /// frame. Drives hover-preview and click-to-focus on `@{session:id}` chips.
    pub program_clip_hits: Vec<ProgramClipHit>,
    /// Template-button hitboxes drawn in the empty-program placeholder. Clicking
    /// one fills the program with that template's Markdown. Empty unless the
    /// active program is showing the empty-state placeholder.
    pub program_template_hits: Vec<ProgramTemplateHit>,
    /// Bounds of the browser preview overlay rendered in the terminal view.
    pub browser_preview_area: Option<ratatui::layout::Rect>,
    /// Top-right close button bounds for the browser preview overlay: `(x_start, x_end, y)`.
    pub browser_preview_close: Option<(u16, u16, u16)>,
    /// Terminal scrollback overlay hit geometry for the selected session.
    pub terminal_scrollbar: Option<TerminalScrollbarHit>,
    /// Dynamic UI action hitboxes from the last frame.
    pub dynamic_ui_action_hits: Vec<DynamicUiActionHit>,
    /// External-URL hitboxes from the last frame for widget panels.
    /// Parallels `dynamic_ui_action_hits`; populated by markdown rendering
    /// and dispatched by `handle_dynamic_ui_overlay_click` via `open_url`.
    pub dynamic_ui_url_hits: Vec<DynamicUiUrlHit>,
    pub dynamic_ui_widget_hits: Vec<DynamicUiWidgetHit>,
    pub dynamic_ui_panel_close_hits: Vec<DynamicUiPanelCloseHit>,
    pub dynamic_ui_inline_hit: Option<DynamicUiInlineHit>,
    /// Matrix-rain title-bar play/pause loop-toggle button: `(x_start, x_end, y)`.
    pub matrix_operator_loop_hit: Option<(u16, u16, u16)>,
    /// Matrix-rain title-bar Operator label bounds: `(x_start, x_end, y)`.
    pub matrix_operator_title_hit: Option<(u16, u16, u16)>,
    /// Matrix-rain title-bar widget viewport affordances for the operator session.
    pub matrix_widget_hits: Vec<MatrixWidgetHit>,
    /// Dynamic UI title-bar affordance bounds: `(x_start, x_end, y, session_id)`.
    pub dynamic_ui_trigger: Option<(u16, u16, u16, String)>,
    pub dynamic_ui_triggers: Vec<(u16, u16, u16, String)>,
    /// Dynamic UI widget panel bounds from the last frame.
    pub dynamic_ui_popover_area: Option<ratatui::layout::Rect>,
    /// Dynamic UI dropdown bounds from the last frame.
    pub dynamic_ui_dropdown_area: Option<ratatui::layout::Rect>,
    /// Last rendered dynamic UI stack dimensions: `(session_id, content_rows, viewport_rows)`.
    pub dynamic_ui_scroll_metrics: Option<(String, usize, usize)>,
}

#[derive(Debug, Clone)]
pub struct HarnessHit {
    pub name: String,
    pub x_start: u16,
    /// Exclusive end column.
    pub x_end: u16,
    pub y: u16,
    /// `false` for harnesses that failed their real availability probe
    /// (spec 0068) — rendered dimmed + struck-through, click is a no-op +
    /// status line note, hover shows a tooltip explaining why.
    pub available: bool,
    /// Short human-readable reason from the daemon's probe (e.g. "`claude`
    /// CLI not found on daemon PATH"), shown on hover/click when
    /// `available` is false.
    pub detail: Option<String>,
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

    for hit in &layout.main_window_areas {
        if contains(hit.inner_area, col, row) {
            return Some(hit.inner_area);
        }
    }

    if layout.main_window_areas.is_empty() {
        if let Some(view) = layout.view_area {
            let view_inner = inner(view);
            if contains(view_inner, col, row) {
                return Some(view_inner);
            }
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
/// Maximum number of queued daemon notifications to fold into one event-loop
/// pass. Keeps terminal redraw fragments from repainting one chunk at a time.
const MAX_NOTIFICATION_DRAIN: usize = 256;
/// Time budget for the same notification-drain pass. Under many active
/// sessions, per-notification PTY feed work can be expensive enough that a
/// count-only drain skips visible animation frames before the loop paints again.
const NOTIFICATION_DRAIN_BUDGET: Duration = Duration::from_millis(8);

fn should_continue_notification_drain(
    drained: usize,
    drain_started: Instant,
    now: Instant,
) -> bool {
    drained < MAX_NOTIFICATION_DRAIN
        && now
            .checked_duration_since(drain_started)
            .unwrap_or(Duration::ZERO)
            < NOTIFICATION_DRAIN_BUDGET
}

#[allow(dead_code)]
pub async fn run(client: Arc<Client>) -> Result<()> {
    run_with_socket(client.socket_path().to_path_buf()).await
}

pub async fn run_with_socket(socket: std::path::PathBuf) -> Result<()> {
    run_with_socket_initial_selection(socket, None).await
}

pub async fn run_with_socket_selected(
    socket: std::path::PathBuf,
    session_id: String,
) -> Result<()> {
    run_with_socket_initial_selection(socket, Some(session_id)).await
}

async fn run_with_socket_initial_selection(
    socket: std::path::PathBuf,
    initial_session_id: Option<String>,
) -> Result<()> {
    let client = Client::connect(&socket).await?;
    let profile = Profile::from_env();
    let keymap = keymap::default_for(profile);

    // Initial fetches.
    let sessions = client.list().await.unwrap_or_default();
    let groups = client.list_projects().await.unwrap_or_default();
    let harnesses = client.harnesses().await.unwrap_or_default();
    let program_templates = client
        .program_templates()
        .await
        .map(|r| r.templates)
        .unwrap_or_default();
    // Theme config is parsed now; the final palette (light vs dark) is resolved
    // after raw mode is on, once we can query the terminal background (OSC 11).
    let theme_config = crate::theme::ThemeConfig::load();
    let theme_warning = theme_config.warning.clone();
    let theme = theme_config.resolve(None);
    let initial_orch_id = sessions
        .iter()
        .find(|s| s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal())
        .map(|s| s.id.clone());
    // Restore the previously-selected session if it still exists,
    // else fall back to the first non-orchestrator session.
    let persisted = crate::tui_state::load();
    let requested_initial_sel = initial_session_id.as_ref().and_then(|id| {
        sessions
            .iter()
            .find(|s| s.id == *id && is_user_list_session(s))
            .map(|s| Selection::Session(s.id.clone()))
    });
    let initial_zoom = if requested_initial_sel.is_some() {
        ZoomMode::None
    } else {
        persisted.zoom
    };
    let initial_focus = match initial_zoom {
        ZoomMode::List => PaneFocus::List,
        ZoomMode::View | ZoomMode::None => PaneFocus::View,
    };
    let initial_sel = requested_initial_sel
        .clone()
        .or_else(|| {
            persisted.last_selected_session_id.as_ref().and_then(|id| {
                sessions
                    .iter()
                    .find(|s| s.id == *id && is_user_list_session(s))
                    .map(|s| Selection::Session(s.id.clone()))
            })
        })
        .or_else(|| {
            sessions
                .iter()
                .find(|s| is_user_list_session(s))
                .map(|s| Selection::Session(s.id.clone()))
        })
        .unwrap_or(Selection::None);
    let mut initial_main_windows = persisted
        .main_windows
        .clone()
        .map(|tree| prune_window_tree(tree, &sessions, &groups, &initial_sel))
        .unwrap_or_else(|| MainWindowTree::single(1, initial_sel.clone()));
    let mut initial_active_window_id = persisted
        .active_window_id
        .filter(|id| initial_main_windows.find_selection(*id).is_some())
        .unwrap_or_else(|| initial_main_windows.first_leaf_id().unwrap_or(1));
    let initial_window_sel = if let Some(sel) = requested_initial_sel {
        if !initial_main_windows.set_selection(initial_active_window_id, sel.clone()) {
            initial_active_window_id = 1;
            initial_main_windows = MainWindowTree::single(initial_active_window_id, sel.clone());
        }
        sel
    } else {
        initial_main_windows
            .find_selection(initial_active_window_id)
            .cloned()
            .unwrap_or_else(|| initial_sel.clone())
    };

    let now = Instant::now();
    let socket = client.socket_path().to_path_buf();
    let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());
    // Placeholder sender; `run_loop` installs the live channel it actually drains.
    let program_templates_tx = mpsc::unbounded_channel().0;
    let mut app = App {
        client: client.clone(),
        last_reported_view: None,
        sessions,
        groups,
        selection: initial_window_sel,
        // Default focus is the view — the selected session is usually
        // what the user wants to interact with first. List navigation
        // is one `C-x o` / `Tab` away.
        focus: initial_focus,
        next_window_id: initial_main_windows.max_id().saturating_add(1),
        active_window_id: initial_active_window_id,
        main_windows: initial_main_windows,
        subagent_collapsed: HashSet::new(),
        transcript: Vec::new(),
        transcript_session: None,
        transcript_scroll: 0,
        minibuffer: None,
        harnesses,
        program_templates,
        program_templates_tx,
        theme,
        help_visible: false,
        profile,
        keymap,
        chord_state: ChordState::default(),
        chord_label: String::new(),
        status: None,
        update_notice: None,
        last_diff: None,
        should_quit: false,
        connected: true,
        remote_clients: 0,
        view: ViewMode::Chat,
        histories: HashMap::new(),
        terminal_replayed_sessions_this_frame: HashSet::new(),
        block_hits: HashMap::new(),
        matrix_reveal_hits: Vec::new(),
        orchestrator_desired_size: None,
        tasks_popup: None,
        session_picker: None,
        program_popup: None,
        program_popups: HashMap::new(),
        program_view_memory: HashMap::new(),
        program_runs: HashMap::new(),
        program_run_dispatch: HashMap::new(),
        program_settle_flourishes: HashMap::new(),
        program_collaborators: HashMap::new(),
        program_agent_reveal_receipts: HashMap::new(),
        own_program_client_id: None,
        program_clipboard: None,
        remote_control_popup: None,
        remote_control_task: None,
        terminal_pane_size: (100, 30),
        window_pane_sizes: HashMap::new(),
        zoom: initial_zoom,
        list_scroll_offset: 0,
        view_scrollback: 0,
        window_scrollback: HashMap::new(),
        window_views: HashMap::new(),
        terminal_scrollbar_visible_until: HashMap::new(),
        skip_redraw_after_event: false,
        notification_dirtied_view: true,
        hydrating_sessions: HashSet::new(),
        orchestrator_scrollback: 0,
        operator_monolog: None,
        operator_utterance: String::new(),
        orchestrator_panel_h: persisted.orchestrator_panel_h,
        resizing_orchestrator_panel: None,
        dragging_terminal_scrollbar: None,
        pty_activity: HashMap::new(),
        start_instant: now,
        layout: LayoutSnapshot::default(),
        session_title_menu: None,
        mouse_pos: None,
        mouse_capture_enabled: true,
        orchestrator_id: initial_orch_id,
        list_panel_w: persisted.list_panel_w.unwrap_or(LIST_PANEL_W_DEFAULT),
        resizing_list: None,
        pin_strip_h: persisted.pin_strip_h,
        resizing_pin_strip: None,
        matrix_rain_h: persisted.matrix_rain_h,
        resizing_matrix_rain: None,
        resizing_main_window: None,
        resizing_program_popup: None,
        list_collapsed: persisted.list_collapsed,
        editor_states: HashMap::new(),
        agent_statuses: HashMap::new(),
        pending_tool_approvals: HashMap::new(),
        browser_previews: HashMap::new(),
        ui_panels: HashMap::new(),
        dynamic_ui_popover_open: None,
        dynamic_ui_selected: persisted
            .widgets
            .iter()
            .flat_map(|(session_id, state)| {
                state
                    .visible
                    .iter()
                    .map(move |panel_id| (session_id.clone(), panel_id.clone()))
            })
            .collect(),
        dynamic_ui_temporary_until: HashMap::new(),
        dynamic_ui_hover: None,
        dynamic_ui_focused: None,
        dynamic_ui_scroll_offsets: HashMap::new(),
        image_resize_cache: Vec::new(),
        session_transitions: HashMap::new(),
        pin_transitions: HashMap::new(),
        matrix_rain: crate::matrix_rain::MatrixRain::default(),
        matrix_rain_intensity: 0.0,
        matrix_rain_intensity_updated_at: now,
        matrix_rain_foreground_epoch: now,
        matrix_rain_active_drops: HashMap::new(),
        matrix_widget_pinned: None,
        matrix_widget_hover: None,
        matrix_rain_hidden: persisted.matrix_rain_hidden,
        show_archived_ungrouped: false,
        show_archived_groups: HashSet::new(),
        show_archived_subagents: HashSet::new(),
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
        app.set_active_view(ViewMode::Terminal);
    }
    app.restore_open_program_popups(&persisted.open_program_session_ids)
        .await;

    // Subscribe to all session events.
    if let Err(e) = client.subscribe(None).await {
        app.status = Some((format!("subscribe failed: {e}"), Instant::now()));
    }
    // Do not hydrate the selected or pinned sessions here: full transcript
    // replay can be hundreds of MB for long-lived sessions. The run loop paints
    // the first frame immediately, then starts background hydration and renders
    // loading placeholders until each history is ready.

    // One-line "update available" notice, sourced from an on-disk cache so it
    // never blocks startup (a stale cache refreshes in the background for the
    // next launch). Held in a dedicated field so it persists in the modeline
    // until the user upgrades, rather than expiring after a few seconds like a
    // transient status. Opt out with CONSTRUCT_NO_UPDATE_CHECK=1.
    app.update_notice = crate::upgrade::cached_update_notice();

    if app.selected_needs_hydration() {
        if let Some(id) = app.selection.session_id() {
            app.hydrating_sessions.insert(id.to_string());
        }
    }
    for id in app
        .main_window_sessions_needing_hydration()
        .into_iter()
        .chain(app.pinned_sessions_needing_hydration())
        .chain(app.orchestrator_session_needing_hydration())
    {
        app.hydrating_sessions.insert(id);
    }

    // Terminal setup.
    enable_raw_mode().context("enable raw mode")?;
    // Now that the terminal is in raw mode (and before the event loop starts
    // consuming stdin), resolve the palette against the terminal background for
    // `mode = "auto"`. Forced light/dark skip the query; a non-answering
    // terminal falls back to dark.
    let detected_light = if theme_config.mode == crate::theme::ThemeMode::Auto {
        crate::theme::detect_terminal_is_light(std::time::Duration::from_millis(120))
    } else {
        None
    };
    app.theme = theme_config.resolve(detected_light);
    tracing::info!(mode = ?theme_config.mode, ?detected_light, "tui theme resolved");
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )
    .context("enter alternate screen / enable mouse")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("create terminal")?;

    let result = run_loop(&mut terminal, &mut app, socket).await;

    // Teardown — best effort.
    let _ = disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    );
    terminal.show_cursor().ok();

    app.save_open_program_popups().await;

    let mut widgets: HashMap<String, crate::tui_state::WidgetState> = HashMap::new();
    for (session_id, panel_id) in &app.dynamic_ui_selected {
        widgets
            .entry(session_id.clone())
            .or_default()
            .visible
            .push(panel_id.clone());
    }
    for state in widgets.values_mut() {
        state.visible.sort();
        state.visible.dedup();
    }
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
        main_windows: Some(app.main_windows.clone()),
        active_window_id: Some(app.active_window_id),
        open_program_session_ids: app.open_program_session_ids(),
        widgets,
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
    // Live-reload channel for program templates. The sender lives on `app` so
    // `open_program_popup` can kick off a background refresh; this loop drains the
    // receiver and applies the freshest list.
    let (program_templates_tx, mut program_templates_rx) =
        mpsc::unbounded_channel::<Vec<agentd_protocol::ProgramTemplate>>();
    app.program_templates_tx = program_templates_tx;
    let mut reconnect: Option<ReconnectState> = None;
    // Tick at the spinner frame boundary so each frame gets one redraw.
    let mut tick = tokio::time::interval(Duration::from_millis(SPINNER_FRAME_MS as u64));
    // Re-probe harness availability while the welcome card (no session
    // selected) is showing, so installing a CLI or exporting an API key
    // updates the card's live status without a TUI restart. Gated to that
    // view so a busy fleet doesn't pay an extra IPC round trip every 5s for
    // no visible benefit.
    let mut harness_refresh = tokio::time::interval(Duration::from_secs(5));
    harness_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Wall-clock of the last actual `terminal.draw`. The notification arm uses
    // it as a heartbeat: a burst that touched nothing visible skips its repaint
    // only while a draw happened within the last tick, so sustained background
    // output still repaints (spinner / rain / list) at ~8fps and can never
    // freeze the foreground.
    let mut last_draw = Instant::now();

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
    // The PTY size we last pushed to each visible session, keyed by id.
    // The resize fire below is otherwise gated on the *active* pane's
    // size; a passive split pane whose own size changed (split created,
    // divider dragged, sibling swapped) without the active pane changing
    // would never get resized, leaving its child emitting at a stale
    // width that the pane then renders into a different-width grid —
    // the garbled split pane that only a window resize cleared. Tracking
    // per-session sizes lets the gate fire on any pane's divergence.
    let mut last_pane_sizes_sent: HashMap<String, (u16, u16)> = HashMap::new();
    let mut hydration_tasks: tokio::task::JoinSet<(String, Result<SessionHydration>)> =
        tokio::task::JoinSet::new();
    let mut hydration_sessions: HashSet<String> = HashSet::new();
    let mut pinned_hydration_queue: std::collections::VecDeque<String> =
        std::collections::VecDeque::new();
    let mut pinned_hydration_task: Option<tokio::task::JoinHandle<Result<SessionHydration>>> = None;
    let mut pinned_hydration_session: Option<String> = None;
    while !app.should_quit {
        while let Ok(msg) = app.pty_input_errors.try_recv() {
            app.set_status(msg);
        }
        if app.connected && app.client.is_disconnected() {
            app.connected = false;
            reconnect = Some(ReconnectState::new(Instant::now()));
            app.set_status(
                "daemon disconnected — reconnecting… (press C-x C-c to quit TUI)".to_string(),
            );
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
                        last_pane_sizes_sent.clear();
                        hydration_sessions.clear();
                        pinned_hydration_session = None;
                        pinned_hydration_queue.clear();
                        app.hydrating_sessions.clear();
                        hydration_tasks.abort_all();
                        if let Some(task) = pinned_hydration_task.take() {
                            task.abort();
                        }
                    }
                    Err(e) => {
                        state.schedule_next(now);
                        app.set_status(format!(
                            "daemon disconnected — reconnecting… (press C-x C-c to quit TUI; last error: {e})"
                        ));
                    }
                }
            }
        }
        // Skip the paint when the previous event was a PTY-passthrough
        // keystroke that produced no local display change — its
        // visible effect arrives as PTY output, which sets a fresh
        // loop iteration with the flag cleared. This is what makes a
        // held key (delete / arrow across long text) cheap: one
        // render per output batch instead of one per keypress.
        app.report_view();
        let skip_draw = std::mem::take(&mut app.skip_redraw_after_event);
        if !skip_draw {
            terminal.draw(|f| ui::render(f, app))?;
            last_draw = Instant::now();
        }

        // A PTY-passthrough keystroke has already queued bytes to the input
        // pump and deliberately skipped the stale immediate draw. Keep that
        // fast path focused on input/output readiness: the visible update is
        // the child PTY echo, so do not spend this iteration walking hydration
        // queues or resize debounce state before polling notifications again.
        if !skip_draw {
            // A session switch should stay interactive while history-sized
            // work runs. Selection handlers only mark the transcript as
            // stale; after the frame above has painted the new list highlight
            // / placeholder view, start transcript + PTY hydration in the
            // background. If the user switches again, keep the old hydration
            // running so its history is warm if they switch back.
            if app.selected_needs_hydration() {
                if let Some(req) = app.selected_hydration_request() {
                    if hydration_sessions.insert(req.session_id.clone()) {
                        app.hydrating_sessions.insert(req.session_id.clone());
                        hydration_tasks.spawn(async move {
                            let session_id = req.session_id.clone();
                            let loaded = load_session_hydration(req).await;
                            (session_id, loaded)
                        });
                    }
                }
            }

            let selected_hydrating = &hydration_sessions;
            for id in app
                .main_window_sessions_needing_hydration()
                .into_iter()
                .chain(app.pinned_sessions_needing_hydration())
                .chain(app.orchestrator_session_needing_hydration())
                .chain(app.program_referenced_sessions_needing_hydration())
            {
                if selected_hydrating.contains(&id)
                    || pinned_hydration_session.as_deref() == Some(id.as_str())
                    || pinned_hydration_queue.iter().any(|queued| queued == &id)
                {
                    continue;
                }
                app.hydrating_sessions.insert(id.clone());
                pinned_hydration_queue.push_back(id);
            }
            if pinned_hydration_task.is_none() {
                while let Some(id) = pinned_hydration_queue.pop_front() {
                    if selected_hydrating.contains(&id) || app.histories.contains_key(&id) {
                        app.hydrating_sessions.remove(&id);
                        continue;
                    }
                    let req = app.session_hydration_request(&id, true);
                    pinned_hydration_session = Some(id);
                    pinned_hydration_task = Some(tokio::spawn(load_session_hydration(req)));
                    break;
                }
            }

            // Right pane (main session) resize — debounced fire. Also
            // refires if the *selected* session changed since last sent,
            // or if any visible split pane's size diverged from what its
            // child was last told (a passive pane the active-pane gate
            // would otherwise miss).
            let cur = app.active_pane_size();
            let split_sizes = app.window_session_pane_sizes();
            let cur_session = app.selected_id();
            let session_changed = cur_session != last_session_sent;
            let pane_sizes_diverged = pane_sizes_diverged(&split_sizes, &last_pane_sizes_sent);
            if cur.0 > 0
                && cur.1 > 0
                && (cur != last_size_sent || session_changed || pane_sizes_diverged)
            {
                match pending_size {
                    Some((p, _)) if p == cur && !session_changed && !pane_sizes_diverged => {}
                    _ => pending_size = Some((cur, Instant::now())),
                }
            } else {
                pending_size = None;
            }
            if let Some((size, at)) = pending_size {
                if at.elapsed() >= resize_debounce || session_changed {
                    if split_sizes.is_empty() {
                        app.notify_pane_size(size.0, size.1).await;
                    } else {
                        let sessions = app.sessions.clone();
                        for (id, (cols, rows)) in &split_sizes {
                            if sessions
                                .iter()
                                .any(|s| s.id == *id && s.has_pty && !s.state.is_terminal())
                            {
                                let _ = app.client.pty_resize(id, *cols, *rows).await;
                                last_pane_sizes_sent.insert(id.clone(), (*cols, *rows));
                            }
                        }
                        // Forget sizes for sessions no longer visible so a
                        // stale entry can't suppress a needed resize when
                        // they reappear, and the map stays bounded.
                        last_pane_sizes_sent
                            .retain(|id, _| split_sizes.iter().any(|(vid, _)| vid == id));
                    }
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
                        // Fire-and-forget: the adapter may still be in its startup
                        // health-check when the panel first opens (model broken,
                        // OAuth refresh in flight, etc.).  Awaiting it directly
                        // here — before tokio::select! — blocks the entire render
                        // loop (matrix rain, notifications, keystrokes) for up to
                        // the 60 s adapter.request timeout and freezes the TUI.
                        let client = app.client.clone();
                        tokio::spawn(async move {
                            let _ = client.pty_resize(&orch_id, size.0, size.1).await;
                        });
                    }
                    last_orch_sent = size;
                    pending_orch = None;
                }
            }
        }
        tokio::select! {
            // Poll arms top-to-bottom by priority rather than at
            // random: a ready keystroke is handled before we drain the
            // background notification batch below. When several
            // sessions flood PTY output, `notifications.recv()` is
            // almost always ready, so an unbiased `select!` would
            // service the feed work as often as input — adding
            // keystroke→render latency in the focused session. Input is
            // bursty (human-paced), so giving it top priority can't
            // starve the lower arms: between keystrokes `input_stream`
            // is Pending and the notification/tick arms run as before.
            biased;
            ev = input_stream.next() => {
                match ev {
                    Some(Ok(ev)) => {
                        // Only enter the mouse-burst drain when the
                        // event we just handled was itself a left-drag
                        // or wheel event.
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
                        let drain_mouse_burst = should_drain_after(&ev);
                        app.on_term_event(ev).await;
                        if drain_mouse_burst {
                            const MAX_MOUSE_DRAIN: usize = 64;
                            let mut drained = 0;
                            while drained < MAX_MOUSE_DRAIN {
                                match input_stream.next().now_or_never() {
                                    Some(Some(Ok(CtEvent::Mouse(m))))
                                        if drainable_mouse_burst_kind(&m.kind) =>
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
            hydrated = hydration_tasks.join_next(), if !hydration_tasks.is_empty() => {
                match hydrated {
                    Some(Ok((id, Ok(h)))) => {
                        hydration_sessions.remove(&id);
                        app.apply_session_hydration(h).await;
                    }
                    Some(Ok((id, Err(e)))) => {
                        hydration_sessions.remove(&id);
                        app.hydrating_sessions.remove(&id);
                        app.set_status(format!("load transcript: {e}"));
                    }
                    Some(Err(e)) if e.is_cancelled() => {
                        // The JoinSet was explicitly aborted during reconnect or shutdown;
                        // `hydration_sessions` is cleared alongside `abort_all`.
                    }
                    Some(Err(e)) => {
                        app.set_status(format!("load transcript task failed: {e}"));
                    }
                    None => {}
                }
            }
            pinned_hydrated = async {
                match pinned_hydration_task.as_mut() {
                    Some(task) => task.await,
                    None => futures::future::pending().await,
                }
            }, if pinned_hydration_task.is_some() => {
                pinned_hydration_task = None;
                let completed_session = pinned_hydration_session.take();
                match pinned_hydrated {
                    Ok(Ok(h)) => app.apply_pinned_session_hydration(h).await,
                    Ok(Err(e)) => {
                        if let Some(id) = completed_session.as_ref() {
                            app.hydrating_sessions.remove(id);
                        }
                        app.set_status(format!("load pinned transcript: {e}"));
                    }
                    Err(e) if e.is_cancelled() => {
                        if let Some(id) = completed_session.as_ref() {
                            app.hydrating_sessions.remove(id);
                        }
                    }
                    Err(e) => {
                        if let Some(id) = completed_session.as_ref() {
                            app.hydrating_sessions.remove(id);
                        }
                        app.set_status(format!("load pinned transcript task failed: {e}"));
                    }
                }
            }
            // Gate on `reconnect.is_none()`: once the daemon drops, the
            // old `notifications` channel is closed, so `recv()` is
            // *immediately* ready with `None` on every poll. Left
            // ungated, this arm wins the select every iteration and
            // spins the loop at 100% CPU (repainting each pass) for the
            // entire reconnect window — which, during a slow daemon
            // restart, reads as a frozen UI. Disabling the arm while a
            // reconnect is pending lets the loop idle on `tick` (which
            // also drives the reconnect-retry cadence) instead. The
            // top-of-loop `is_disconnected()` check still catches the
            // initial drop, so detection isn't lost.
            notif = notifications.recv(), if reconnect.is_none() => {
                match notif {
                    Some(n) => {
                        app.on_notification(n).await;
                        let mut dirtied_view = app.notification_dirtied_view;
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
                        let drain_started = Instant::now();
                        let mut drained = 0;
                        while should_continue_notification_drain(
                            drained,
                            drain_started,
                            Instant::now(),
                        ) {
                            match notifications.try_recv() {
                                Ok(n) => {
                                    app.on_notification(n).await;
                                    dirtied_view |= app.notification_dirtied_view;
                                    drained += 1;
                                }
                                Err(_) => break,
                            }
                        }
                        // If the whole burst only touched off-screen session
                        // state (the common case with a large mostly-idle
                        // fleet — background `Pty` chunks warming history /
                        // spinner / rain), skip this iteration's full-frame
                        // render. The heartbeat still forces a draw once a tick
                        // has elapsed since the last one, so the list + rain
                        // keep animating and the foreground never freezes under
                        // sustained background output.
                        if !dirtied_view
                            && last_draw.elapsed()
                                < Duration::from_millis(SPINNER_FRAME_MS as u64)
                        {
                            app.skip_redraw_after_event = true;
                        }
                    }
                    None => {
                        if app.connected {
                            app.connected = false;
                            reconnect = Some(ReconnectState::new(Instant::now()));
                            app.set_status(
                                "daemon disconnected — reconnecting… (press C-x C-c to quit TUI)"
                                    .to_string(),
                            );
                        }
                    }
                }
            }
            templates = program_templates_rx.recv() => {
                // Live-reloaded program templates from a background fetch
                // (`refresh_program_templates`, kicked off when the program pane
                // opens). Apply only the freshest value: drain any queued
                // refreshes so a burst of opens collapses to the latest list.
                if let Some(mut latest) = templates {
                    while let Ok(next) = program_templates_rx.try_recv() {
                        latest = next;
                    }
                    if !latest.is_empty() {
                        app.program_templates = latest;
                    }
                }
            }
            _ = tick.tick() => {
                if let Some((_, at)) = &app.status {
                    if at.elapsed() > Duration::from_secs(5) {
                        app.status = None;
                    }
                }
                app.update_browser_preview_hover_and_expiry();
                app.expire_program_runs(Instant::now());
            }
            _ = harness_refresh.tick(), if app.connected && app.selected_id().is_none() => {
                if let Ok(list) = app.client.harnesses().await {
                    app.harnesses = list;
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
        let groups = client.list_projects().await.unwrap_or_default();
        let harnesses = client.harnesses().await.unwrap_or_default();
        let program_templates = client
            .program_templates()
            .await
            .map(|r| r.templates)
            .unwrap_or_default();
        let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());

        self.client = client;
        self.pty_input_tx = pty_input_tx;
        self.pty_input_errors = pty_input_errors;
        self.sessions = sessions;
        self.groups = groups;
        self.harnesses = harnesses;
        if !program_templates.is_empty() {
            self.program_templates = program_templates;
        }
        // A daemon restart respawns every PTY session and truncates each
        // session's pty.log so the new child renders into a clean slate
        // (see Manager::respawn). But our in-memory terminal histories
        // still hold the *previous* child's screen state. Codex/claude/
        // shell repaint on resume without a full screen clear, so feeding
        // the new child's output on top of the stale grid leaves the pane
        // half-rendered — typically blank until the user resizes. Drop the
        // histories so the visible/pinned sessions re-hydrate from the
        // daemon's (now clean) pty.log, mirroring the daemon-side truncate.
        self.histories.clear();
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

    fn insert_browser_preview(
        &mut self,
        session_id: String,
        preview: agentd_protocol::BrowserPreview,
    ) {
        let decoded = decode_browser_preview_image(&preview.image);
        let now = Instant::now();
        self.browser_previews.insert(
            session_id,
            BrowserPreviewState {
                hide_after: now + BROWSER_PREVIEW_TTL,
                hover_started: None,
                decoded,
                revealed_at: now,
            },
        );
    }

    fn update_browser_preview_hover_and_expiry(&mut self) {
        let now = Instant::now();
        let selected_sid = self.selected_id();
        let mouse_pos = self.mouse_pos;
        let preview_area = self.layout.browser_preview_area;

        self.browser_previews.retain(|sid, state| {
            // Check if this preview is currently being hovered.
            // A preview can only be hovered if it belongs to the selected session,
            // the preview area is currently rendered, and the mouse is inside that area.
            let is_hovered = if Some(sid.as_str()) == selected_sid.as_deref() {
                if let (Some(area), Some((mx, my))) = (preview_area, mouse_pos) {
                    mx >= area.x
                        && mx < area.x + area.width
                        && my >= area.y
                        && my < area.y + area.height
                } else {
                    false
                }
            } else {
                false
            };

            if is_hovered {
                state.hover_started.get_or_insert(now);
                true
            } else {
                if state.hover_started.take().is_some() {
                    state.hide_after = now + BROWSER_PREVIEW_TTL;
                }
                now < state.hide_after
            }
        });
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

    async fn on_paste(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        if self.program_search_active() {
            self.append_program_search_query_text(&text);
            return;
        }

        // Mirror the keystroke routing precedence (see `on_key`): pasted
        // text lands in the program only when no minibuffer/palette overlay is
        // capturing input *and* the view pane holds focus. With an overlay open,
        // `dispatch_paste_text` below routes the paste to the minibuffer /
        // orchestrator instead; with focus on the list, it routes to the
        // selected session rather than editing the program.
        if self.program_popup.is_some()
            && self.minibuffer.is_none()
            && self.focus == PaneFocus::View
        {
            self.insert_program_text(&text);
            return;
        }

        if text.chars().count() >= LARGE_TEXT_PASTE_CHARS {
            if let Some(session_id) = self.large_text_paste_target() {
                use base64::Engine as _;

                let data = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
                match self
                    .client
                    .attach_clipboard(
                        &session_id,
                        data,
                        Some("clipboard.txt".to_string()),
                        Some("text/plain".to_string()),
                    )
                    .await
                {
                    Ok(result) => {
                        self.dispatch_paste_text(result.reference);
                        self.set_status(format!(
                            "saved paste as attachment for {}",
                            short_id(&session_id)
                        ));
                    }
                    Err(e) => self.set_status(format!("paste attachment failed: {e}")),
                }
                return;
            }
        }

        self.dispatch_paste_text(text);
    }

    fn large_text_paste_target(&self) -> Option<String> {
        match self.minibuffer.as_ref().map(|m| &m.intent) {
            Some(MinibufferIntent::SendInput { session_id }) => Some(session_id.clone()),
            Some(MinibufferIntent::Orchestrator) => self.orchestrator_id.clone(),
            Some(_) => None,
            None if self.is_pty_captured() => self.selected_id(),
            None => None,
        }
    }

    fn dispatch_paste_text(&mut self, text: String) {
        match self.minibuffer.as_ref().map(|m| &m.intent) {
            Some(MinibufferIntent::Orchestrator) => {
                if let Some(orch_id) = self.orchestrator_id.clone() {
                    self.orchestrator_scrollback = 0;
                    let bytes = self.encode_paste_for_pty(&orch_id, text);
                    self.queue_pty_input(orch_id, bytes, "orchestrator pty_input");
                }
            }
            Some(_) => {
                if let Some(mb) = self.minibuffer.as_mut() {
                    minibuffer::insert_minibuffer_text(mb, &text);
                }
            }
            None if self.is_pty_captured() => {
                let active_window = Some(self.active_window_id);
                self.set_scrollback_for_window(active_window, 0);
                if let Some(id) = self.selected_id() {
                    let bytes = self.encode_paste_for_pty(&id, text);
                    self.queue_pty_input(id, bytes, "pty_input");
                }
            }
            None => {}
        }
    }

    /// Encode pasted text for forwarding to a child PTY. If the child has
    /// DEC bracketed-paste mode enabled, wrap the payload in the
    /// `ESC[200~` / `ESC[201~` markers a real terminal would send, so the
    /// harness runs its own paste handling — claude code's image-path drag
    /// detection (a dragged image becomes `[image #N]` instead of a raw
    /// path), its multiline-paste guard, shell readline's "don't execute
    /// until Enter", etc. Without the markers the harness sees the bytes as
    /// ordinary typed keystrokes, which is why a dragged image path used to
    /// land as literal text.
    ///
    /// The closing marker is stripped from the payload first so an embedded
    /// `ESC[201~` can't terminate the paste early — the same paste-injection
    /// guard real terminals apply. When the child has not enabled mode 2004
    /// (or we have no parser yet) the text is forwarded raw, preserving the
    /// prior behavior.
    fn encode_paste_for_pty(&self, session_id: &str, text: String) -> Vec<u8> {
        let bracketed = self
            .histories
            .get(session_id)
            .map(|h| h.bracketed_paste_enabled())
            .unwrap_or(false);
        if !bracketed {
            return text.into_bytes();
        }
        const START: &[u8] = b"\x1b[200~";
        const END: &[u8] = b"\x1b[201~";
        let sanitized = text.replace("\x1b[201~", "");
        let mut out = Vec::with_capacity(START.len() + sanitized.len() + END.len());
        out.extend_from_slice(START);
        out.extend_from_slice(sanitized.as_bytes());
        out.extend_from_slice(END);
        out
    }

    pub fn start_session_transition(&mut self) {
        self.start_window_transition(self.active_window_id);
    }

    pub fn start_window_transition(&mut self, window_id: u64) {
        self.session_transitions.insert(
            window_id,
            SessionTransition {
                started_at: Instant::now(),
            },
        );
    }

    pub fn start_pin_transition(&mut self, session_id: impl Into<String>) {
        self.pin_transitions
            .insert(session_id.into(), Instant::now());
    }

    pub fn select_session(&mut self, id: String) {
        self.select_session_inner(id, true);
    }

    fn select_session_without_transition(&mut self, id: String) {
        self.select_session_inner(id, false);
    }

    fn select_session_inner(&mut self, id: String, transition: bool) {
        if transition && self.selection.session_id() != Some(id.as_str()) {
            self.start_session_transition();
        }
        // Switching to a session consumes its "needs you" marker.
        self.selection = Selection::Session(id);
        self.report_focused_sessions();
        self.transcript.clear();
        self.transcript_session = None;
        self.transcript_scroll = u16::MAX;
        let active_window = Some(self.active_window_id);
        self.set_scrollback_for_window(active_window, 0);
        // Navigating to a new session in the focused pane resets it to that
        // session's natural mode, recorded per-window so the pane keeps it.
        let natural = if self.selected_session().map(|s| s.has_pty).unwrap_or(false) {
            ViewMode::Terminal
        } else {
            ViewMode::Chat
        };
        self.set_active_view(natural);
        // The program is a per-session surface: keep it attached to whatever
        // session is now selected. The outgoing session's program is stashed
        // and the incoming one revealed (if it has a program open). Navigation
        // never *closes* a program — only its title-glyph toggle / C-x Space do.
        self.sync_program_popup_with_selection();
    }

    /// Tell the daemon which surface we're showing the focused session through
    /// (chat vs terminal) so the AskUserQuestion chat-gate can degrade the
    /// picker to text when a chat viewer is active. Debounced + fire-and-forget.
    fn report_view(&mut self) {
        let Some(sid) = self.selected_id() else {
            return;
        };
        let view = match self.view {
            ViewMode::Chat => agentd_protocol::ClientView::Chat,
            ViewMode::Terminal => agentd_protocol::ClientView::Terminal,
        };
        if self.last_reported_view.as_ref() == Some(&(sid.clone(), view)) {
            return;
        }
        self.last_reported_view = Some((sid.clone(), view));
        let client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.set_view(&sid, view).await;
        });
    }

    /// Tell the daemon we've switched to (and thus consumed) a session, so it
    /// clears that session's `needs_attention` marker and records it as the
    /// focused session. Fire-and-forget.
    fn report_seen(&self, id: &str) {
        let client = self.client.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let _ = client.mark_seen(&id).await;
        });
    }

    /// Tell the daemon which sessions are currently visible on the screen so it
    /// can suppress / clear the unblock markers (`needs_attention`) for all of them.
    fn report_focused_sessions(&self) {
        let client = self.client.clone();
        let ids: Vec<String> = self
            .main_windows
            .visible_session_ids()
            .iter()
            .map(|s| s.to_string())
            .collect();
        tokio::spawn(async move {
            let _ = client.set_focused_sessions(ids).await;
        });
    }

    pub fn select_group(&mut self, id: String) {
        if self.selection.group_id() != Some(id.as_str()) {
            self.start_session_transition();
        }
        self.selection = Selection::Group(id);
        self.transcript.clear();
        self.transcript_session = None;
        self.transcript_scroll = u16::MAX;
        let active_window = Some(self.active_window_id);
        self.set_scrollback_for_window(active_window, 0);
    }

    /// Select a section's "N archived" disclosure row. Like [`Self::select_group`]
    /// it isn't a session, so the main view shows nothing for it.
    pub fn select_archive_row(&mut self, section: ArchiveSection) {
        if self.selection.archive_section() != Some(&section) {
            self.start_session_transition();
        }
        self.selection = Selection::ArchivedRow(section);
        self.transcript.clear();
        self.transcript_session = None;
        self.transcript_scroll = u16::MAX;
        let active_window = Some(self.active_window_id);
        self.set_scrollback_for_window(active_window, 0);
    }

    pub fn prune_finished_transitions(&mut self) {
        let done = |started: Instant| started.elapsed().as_millis() >= SESSION_TRANSITION_MS;
        self.session_transitions
            .retain(|_, transition| !done(transition.started_at));
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

    /// Collect the ids of every archived session a "N archived" disclosure
    /// row stands in for. Mirrors the membership filters in `list_items` so
    /// the count shown on the row matches what a cascade delete removes.
    fn archived_sessions_in_section(&self, section: &ArchiveSection) -> Vec<String> {
        let orch_id = self.orchestrator_id.as_deref();
        match section {
            ArchiveSection::Ungrouped => self
                .sessions
                .iter()
                .filter(|s| s.archived)
                .filter(|s| s.group_id.is_none())
                .filter(|s| Some(s.id.as_str()) != orch_id)
                .filter(|s| is_user_list_session(s))
                .map(|s| s.id.clone())
                .collect(),
            ArchiveSection::Group(group_id) => self
                .sessions
                .iter()
                .filter(|s| s.archived)
                .filter(|s| s.group_id.as_deref() == Some(group_id.as_str()))
                .filter(|s| is_user_list_session(s))
                .map(|s| s.id.clone())
                .collect(),
            ArchiveSection::Subagents(parent_id) => self
                .sessions
                .iter()
                .filter(|s| s.archived)
                .filter(|s| is_subagent_session(s))
                .filter(|s| s.parent_session_id.as_deref() == Some(parent_id.as_str()))
                .map(|s| s.id.clone())
                .collect(),
        }
    }

    /// Human-readable name for an archive section, used in confirm prompts.
    fn archive_section_label(&self, section: &ArchiveSection) -> String {
        match section {
            ArchiveSection::Ungrouped => "ungrouped".to_string(),
            ArchiveSection::Group(group_id) => self
                .groups
                .iter()
                .find(|g| g.id == *group_id)
                .map(|g| format!("project '{}'", g.name))
                .unwrap_or_else(|| "project".to_string()),
            ArchiveSection::Subagents(parent_id) => {
                format!("subagents of {}", short_id(parent_id))
            }
        }
    }

    fn selected_session_has_subagents(&self) -> Option<String> {
        let id = self.selection.session_id()?;
        self.sessions
            .iter()
            .any(|s| s.parent_session_id.as_deref() == Some(id))
            .then(|| id.to_string())
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
        Some(self.session_hydration_request(&id, needs_history))
    }

    fn session_hydration_request(&self, id: &str, needs_history: bool) -> SessionHydrationRequest {
        let is_headless = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .map(crate::ui::is_headless)
            .unwrap_or(false);
        SessionHydrationRequest {
            socket: self.client.socket_path().to_path_buf(),
            session_id: id.to_string(),
            needs_history,
            // Hydrate at the session's *own* pane width when it's visible in a
            // split — not the active pane's — so its parser grid matches the
            // width it will render at. Falls back to the active pane for a
            // session not currently in a window leaf (e.g. pinned-strip).
            terminal_pane_size: self
                .session_pane_size(id)
                .unwrap_or_else(|| self.active_pane_size()),
            is_headless,
        }
    }

    fn main_window_sessions_needing_hydration(&self) -> Vec<String> {
        let mut ids = Vec::new();
        fn collect(node: &MainWindowTree, out: &mut Vec<String>) {
            match node {
                MainWindowTree::Leaf { selection, .. } => {
                    if let Some(id) = selection.session_id() {
                        if !out.iter().any(|existing| existing == id) {
                            out.push(id.to_string());
                        }
                    }
                }
                MainWindowTree::Split { first, second, .. } => {
                    collect(first, out);
                    collect(second, out);
                }
            }
        }
        collect(&self.main_windows, &mut ids);
        ids.into_iter()
            .filter(|id| {
                self.sessions
                    .iter()
                    .any(|s| s.id == *id && s.has_pty && !self.histories.contains_key(&s.id))
            })
            .collect()
    }

    fn pinned_sessions_needing_hydration(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|s| s.pinned && s.has_pty && !self.histories.contains_key(&s.id))
            .map(|s| s.id.clone())
            .collect()
    }

    fn orchestrator_session_needing_hydration(&self) -> Option<String> {
        let id = self.orchestrator_id.as_ref()?;
        let needs_history = self
            .sessions
            .iter()
            .any(|s| s.id == *id && s.has_pty && !self.histories.contains_key(&s.id));
        let needs_ui_panels = !self.ui_panels.contains_key(id);
        (needs_history || needs_ui_panels).then(|| id.clone())
    }

    /// Worker sessions referenced by a `@{session:…}` smart clip in a program
    /// that is currently on screen — a program restored in a main-window leaf,
    /// or the active program popup. Their PTY history must be warm so the
    /// clip-chip hover preview (spec 0060) can paint a cropped live terminal
    /// tail the instant the pointer lands on the clip. (The shimmer-text hover
    /// always targets the program's own dispatching session, which is already
    /// warm, so it doesn't need this.) These workers are usually neither
    /// selected, pinned, nor the orchestrator, so without this nothing else
    /// hydrates them and the clip-chip preview silently degrades to the bare
    /// text tooltip.
    fn program_referenced_sessions_needing_hydration(&self) -> Vec<String> {
        fn collect_leaf_sessions(node: &MainWindowTree, out: &mut Vec<String>) {
            match node {
                MainWindowTree::Leaf { selection, .. } => {
                    if let Some(id) = selection.session_id() {
                        if !out.iter().any(|existing| existing == id) {
                            out.push(id.to_string());
                        }
                    }
                }
                MainWindowTree::Split { first, second, .. } => {
                    collect_leaf_sessions(first, out);
                    collect_leaf_sessions(second, out);
                }
            }
        }
        let mut visible_program_owners = Vec::new();
        collect_leaf_sessions(&self.main_windows, &mut visible_program_owners);

        let mut referenced: Vec<String> = Vec::new();
        let add_refs = |markdown: &str, referenced: &mut Vec<String>| {
            for id in program_referenced_session_ids(markdown) {
                if !referenced.iter().any(|existing| existing == &id) {
                    referenced.push(id);
                }
            }
        };
        for owner in &visible_program_owners {
            if let Some(popup) = self.program_popups.get(owner) {
                add_refs(&popup.buffer, &mut referenced);
            }
        }
        if let Some(popup) = self.program_popup.as_ref() {
            add_refs(&popup.buffer, &mut referenced);
        }

        referenced
            .into_iter()
            .filter(|id| {
                self.sessions.iter().any(|s| &s.id == id && s.has_pty)
                    && !self.histories.contains_key(id)
            })
            .collect()
    }

    async fn apply_session_hydration(&mut self, hydration: SessionHydration) {
        if self.selection.session_id() != Some(hydration.session_id.as_str()) {
            // Selection changed while this background load was still running.
            // Keep the expensive PTY/history work instead of throwing it away;
            // just avoid replacing the currently-visible transcript.
            self.apply_hydration_state(hydration, false).await;
            return;
        }

        self.apply_hydration_state(hydration, true).await;
    }

    async fn apply_pinned_session_hydration(&mut self, hydration: SessionHydration) {
        self.apply_hydration_state(hydration, false).await;
    }

    async fn apply_hydration_state(
        &mut self,
        hydration: SessionHydration,
        update_transcript: bool,
    ) {
        let session_id = hydration.session_id;
        self.hydrating_sessions.remove(&session_id);
        if update_transcript {
            self.transcript = hydration.transcript;
            self.transcript_session = Some(session_id.clone());
            self.transcript_scroll = u16::MAX;
        }

        if let Some(history) = hydration.history {
            self.histories.insert(session_id.clone(), history);
            // Resize this session's child to its own pane width, not the whole
            // detail area — a passive split pane is narrower than that.
            let (cols, rows) = self
                .session_pane_size(&session_id)
                .unwrap_or(self.terminal_pane_size);
            // For alt-screen PTY sessions (grok, interactive codex, etc.), the
            // daemon's pty_resize dedup silently drops a same-size resize, which
            // means the child never gets SIGWINCH and doesn't repaint after a
            // TUI-only restart. Send a one-column bump first — mirroring the
            // force_redraw_size_on_resume pattern — to guarantee a SIGWINCH even
            // when the terminal dimensions haven't changed since the last session.
            if hydration.history_is_alt_screen && cols > 1 {
                let _ = self
                    .client
                    .pty_resize(&session_id, cols.saturating_add(1), rows)
                    .await;
            }
            let _ = self.client.pty_resize(&session_id, cols, rows).await;
        }
        if let Some(state) = hydration.editor_state {
            self.editor_states.insert(session_id.clone(), state);
        }
        if let Some(status) = hydration.agent_status {
            self.agent_statuses.insert(session_id.clone(), status);
        }
        self.ui_panels
            .insert(session_id.clone(), hydration.ui_panels);
        if let Some(msg) = hydration.status_messages.last() {
            self.set_status(msg.clone());
        }
    }

    fn user_sessions(&self) -> Vec<&SessionSummary> {
        self.sessions
            .iter()
            .filter(|s| is_user_list_session(s))
            .collect()
    }

    /// Materialize the rendered list: ungrouped sessions (sorted by
    /// position) on top, then groups in position order with each group's
    /// members indented underneath (skipped entirely when the group is
    /// collapsed). Subagents are nested under their parent session and are
    /// expanded by default.
    pub fn list_items(&self) -> Vec<ListItem> {
        let mut out: Vec<ListItem> = Vec::new();

        let orch_id = self.orchestrator_id.as_deref();
        let mut subagents_by_parent: HashMap<&str, Vec<&SessionSummary>> = HashMap::new();
        for s in self.sessions.iter().filter(|s| is_subagent_session(s)) {
            if let Some(parent) = s.parent_session_id.as_deref() {
                subagents_by_parent.entry(parent).or_default().push(s);
            }
        }
        for children in subagents_by_parent.values_mut() {
            children.sort_by(|a, b| {
                a.position
                    .cmp(&b.position)
                    .then_with(|| b.created_at.cmp(&a.created_at))
            });
        }

        let push_session = |out: &mut Vec<ListItem>, s: &SessionSummary, indented: bool| {
            let children = subagents_by_parent.get(s.id.as_str());
            let has_children = children.map(|v| !v.is_empty()).unwrap_or(false);
            let children_expanded = has_children && !self.subagent_collapsed.contains(&s.id);
            out.push(ListItem::Session {
                summary: s.clone(),
                indented,
                has_children,
                children_expanded,
            });
            if children_expanded {
                if let Some(children) = children {
                    let (active_children, archived_children): (
                        Vec<&SessionSummary>,
                        Vec<&SessionSummary>,
                    ) = children.iter().copied().partition(|child| !child.archived);
                    for child in active_children {
                        out.push(ListItem::Session {
                            summary: child.clone(),
                            indented: true,
                            has_children: false,
                            children_expanded: false,
                        });
                    }
                    if !archived_children.is_empty() {
                        let section = ArchiveSection::Subagents(s.id.clone());
                        let expanded = self.show_archived_subagents.contains(&s.id);
                        out.push(ListItem::ArchivedRow {
                            section,
                            count: archived_children.len(),
                            expanded,
                            indented: true,
                        });
                        if expanded {
                            for child in archived_children {
                                out.push(ListItem::Session {
                                    summary: child.clone(),
                                    indented: true,
                                    has_children: false,
                                    children_expanded: false,
                                });
                            }
                        }
                    }
                }
            }
        };

        let mut ungrouped: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| s.group_id.is_none())
            // Hide the orchestrator from the list — it's rendered in
            // the minibuffer instead. Subagents render only as children
            // of their parent session.
            .filter(|s| Some(s.id.as_str()) != orch_id)
            .filter(|s| is_user_list_session(s))
            .collect();
        ungrouped.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        // Active sessions render directly; archived ones sit behind an
        // expandable "N archived" row that ends the section.
        let (ungrouped_active, ungrouped_archived): (Vec<&SessionSummary>, Vec<&SessionSummary>) =
            ungrouped.into_iter().partition(|s| !s.archived);
        for s in ungrouped_active {
            push_session(&mut out, s, false);
        }
        if !ungrouped_archived.is_empty() {
            let expanded = self.show_archived_ungrouped;
            out.push(ListItem::ArchivedRow {
                section: ArchiveSection::Ungrouped,
                count: ungrouped_archived.len(),
                expanded,
                indented: false,
            });
            if expanded {
                for s in ungrouped_archived {
                    push_session(&mut out, s, false);
                }
            }
        }

        let mut groups: Vec<&GroupSummary> = self.groups.iter().collect();
        groups.sort_by_key(|g| g.position);
        for g in groups {
            let mut members: Vec<&SessionSummary> = self
                .sessions
                .iter()
                .filter(|s| s.group_id.as_deref() == Some(g.id.as_str()))
                .filter(|s| is_user_list_session(s))
                .collect();
            members.sort_by_key(|s| s.position);
            let (active, archived): (Vec<&SessionSummary>, Vec<&SessionSummary>) =
                members.into_iter().partition(|s| !s.archived);
            out.push(ListItem::GroupHeader {
                group: g.clone(),
                member_count: active.len(),
            });
            if !g.collapsed {
                for s in active {
                    push_session(&mut out, s, true);
                }
                if !archived.is_empty() {
                    let expanded = self.show_archived_groups.contains(&g.id);
                    out.push(ListItem::ArchivedRow {
                        section: ArchiveSection::Group(g.id.clone()),
                        count: archived.len(),
                        expanded,
                        indented: true,
                    });
                    if expanded {
                        for s in archived {
                            push_session(&mut out, s, true);
                        }
                    }
                }
            }
        }
        out
    }

    /// Toggle whether a section's archived sessions are revealed — the click
    /// target of an "N archived" row.
    pub fn archive_section_revealed(&self, section: &ArchiveSection) -> bool {
        match section {
            ArchiveSection::Ungrouped => self.show_archived_ungrouped,
            ArchiveSection::Group(id) => self.show_archived_groups.contains(id),
            ArchiveSection::Subagents(id) => self.show_archived_subagents.contains(id),
        }
    }

    /// Set whether a section's archived sessions are revealed. Returns whether
    /// the state actually changed.
    pub fn set_archive_section_revealed(
        &mut self,
        section: &ArchiveSection,
        revealed: bool,
    ) -> bool {
        match section {
            ArchiveSection::Ungrouped => {
                let changed = self.show_archived_ungrouped != revealed;
                self.show_archived_ungrouped = revealed;
                changed
            }
            ArchiveSection::Group(id) => {
                if revealed {
                    self.show_archived_groups.insert(id.clone())
                } else {
                    self.show_archived_groups.remove(id)
                }
            }
            ArchiveSection::Subagents(id) => {
                if revealed {
                    self.show_archived_subagents.insert(id.clone())
                } else {
                    self.show_archived_subagents.remove(id)
                }
            }
        }
    }

    pub fn toggle_archive_section(&mut self, section: &ArchiveSection) {
        let revealed = !self.archive_section_revealed(section);
        self.set_archive_section_revealed(section, revealed);
        self.set_status(
            if revealed {
                "showing archived sessions"
            } else {
                "hiding archived sessions"
            }
            .to_string(),
        );
    }

    /// `/archived` keyboard entry point: reveal/hide archived sessions for the
    /// section the current selection lives in — the archived row itself, the
    /// selected project, the selected session's project, or the ungrouped run.
    pub fn toggle_archived_for_selection(&mut self) {
        let section = match &self.selection {
            Selection::ArchivedRow(section) => section.clone(),
            Selection::Group(id) => ArchiveSection::Group(id.clone()),
            Selection::Session(id) => self
                .sessions
                .iter()
                .find(|s| s.id == *id)
                .and_then(|s| s.group_id.clone())
                .map(ArchiveSection::Group)
                .unwrap_or(ArchiveSection::Ungrouped),
            Selection::None => ArchiveSection::Ungrouped,
        };
        self.toggle_archive_section(&section);
    }

    /// Find the index of the currently-selected item in the materialized
    /// list. Returns `None` if there is no selection or the item went away.
    pub fn selected_list_index(&self) -> Option<usize> {
        let items = self.list_items();
        items.iter().position(|it| it.matches(&self.selection))
    }

    fn chat_scroll_line_count(&self) -> usize {
        let mut count = 0usize;
        let mut previous = ChatScrollKind::Hidden;
        for ev in &self.transcript {
            let kind = chat_scroll_kind(&ev.event);
            if kind == ChatScrollKind::Hidden {
                continue;
            }
            if (kind == ChatScrollKind::AssistantMessage
                && previous == ChatScrollKind::AssistantMessage)
                || (kind == ChatScrollKind::Reasoning && previous == ChatScrollKind::Reasoning)
            {
                // Streaming assistant/reasoning chunks render as one aggregated chat row.
                continue;
            }
            if count > 0 && chat_scroll_needs_gap(previous, kind) {
                count += 1;
            }
            count += 1;
            previous = kind;
        }
        count.max(1)
    }

    fn chat_scroll_max(&self) -> u16 {
        transcript_scroll_pos(self.chat_scroll_line_count().saturating_sub(1))
    }

    fn adjust_chat_scroll(&mut self, delta: i32) {
        let max = self.chat_scroll_max();
        let current = if self.transcript_scroll == u16::MAX {
            max
        } else {
            self.transcript_scroll.min(max)
        };
        self.transcript_scroll = if delta > 0 {
            current.saturating_sub(delta as u16)
        } else {
            current.saturating_add(delta.unsigned_abs() as u16).min(max)
        };
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
        // Switching sessions in single-pane mode snaps to live for the new one.
        if !self.is_split_layout() {
            self.view_scrollback = 0;
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
            self.set_active_view(ViewMode::Terminal);
            self.bootstrap_terminal(&id).await;
        } else {
            self.set_active_view(ViewMode::Chat);
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
        let mut replayed_ui_panels: HashMap<String, agentd_protocol::UiPanel> = HashMap::new();
        let is_headless = self
            .sessions
            .iter()
            .find(|s| s.id == id)
            .map(crate::ui::is_headless)
            .unwrap_or(false);
        match self.client.transcript(id, 0, None).await {
            Ok(t) => {
                if t.events
                    .iter()
                    .any(|ev| matches!(ev.event, SessionEvent::Pty { .. }))
                {
                    // New daemons persist PTY events in the transcript as ordering
                    // markers. Prefer rebuilding from those markers so transcript-only
                    // items (smith tool blocks) are interleaved with the raw bytes in
                    // chronological order. The pty_replay path above remains the
                    // fallback for older sessions whose transcripts do not contain PTY.
                    history.clear_items();
                }
                apply_transcript_to_local_state(
                    &t.events,
                    &mut history,
                    &mut replayed_editor_state,
                    &mut replayed_agent_status,
                    &mut replayed_ui_panels,
                    is_headless,
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
        if !replayed_ui_panels.is_empty() {
            self.ui_panels.insert(id.to_string(), replayed_ui_panels);
        }
        self.histories.insert(id.to_string(), history);
        // Tell the daemon what size we'd like — this session's own pane width
        // when it's visible in a split, not the active pane's.
        let (cols, rows) = self
            .session_pane_size(id)
            .unwrap_or_else(|| self.active_pane_size());
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
                    self.set_status("project has no members".into());
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
            // The "N archived" disclosure row isn't a pin target.
            Selection::ArchivedRow(_) => {}
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
                if let Err(e) = self.client.move_project(&id, dir).await {
                    self.set_status(format!("move failed: {e}"));
                }
            }
            Selection::None => self.set_status("nothing selected".into()),
            // The "N archived" disclosure row isn't reorderable.
            Selection::ArchivedRow(_) => {}
        }
    }

    async fn refresh_sessions(&mut self) {
        match self.client.list().await {
            Ok(list) => self.sessions = list,
            Err(e) => self.set_status(format!("list failed: {e}")),
        }
        match self.client.list_projects().await {
            Ok(list) => self.groups = list,
            Err(e) => self.set_status(format!("project list failed: {e}")),
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
    /// smith), we behave as if there's no orchestrator so the user
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

    fn sync_active_window_selection(&mut self) {
        let active_id = self.active_window_id;
        let replacement = self.selection.clone();
        let previous = self.selection_for_window(active_id);
        if let (Some(previous), Selection::Session(next_session_id)) = (&previous, &replacement) {
            if previous.session_id() != Some(next_session_id.as_str()) {
                if let Some(other_id) = self
                    .main_windows
                    .find_window_with_session_except(next_session_id, active_id)
                {
                    self.main_windows.set_selection(other_id, previous.clone());
                    self.set_scrollback_for_window(Some(other_id), 0);
                }
            }
        }
        self.main_windows.set_selection(active_id, replacement);
    }

    pub(crate) fn selection_for_window(&self, target: u64) -> Option<Selection> {
        fn find(node: &MainWindowTree, target: u64) -> Option<Selection> {
            match node {
                MainWindowTree::Leaf { id, selection } if *id == target => Some(selection.clone()),
                MainWindowTree::Leaf { .. } => None,
                MainWindowTree::Split { first, second, .. } => {
                    find(first, target).or_else(|| find(second, target))
                }
            }
        }
        find(&self.main_windows, target)
    }

    pub fn scrollback_for_window(&self, window_id: Option<u64>) -> usize {
        match window_id {
            Some(id) => self.window_scrollback.get(&id).copied().unwrap_or(0),
            None => self.view_scrollback,
        }
    }

    pub fn set_scrollback_for_window(&mut self, window_id: Option<u64>, scrollback: usize) {
        if window_id == Some(self.active_window_id) {
            self.view_scrollback = scrollback;
        }
        if let Some(id) = window_id {
            if scrollback == 0 {
                self.window_scrollback.remove(&id);
            } else {
                self.window_scrollback.insert(id, scrollback);
            }
        } else {
            self.view_scrollback = scrollback;
        }
    }

    pub fn is_split_layout(&self) -> bool {
        matches!(self.main_windows, MainWindowTree::Split { .. })
    }

    /// Natural view mode for a window's session: Terminal when the session has
    /// a PTY, Chat otherwise. Used as the default when a window has no explicit
    /// per-window override in `window_views`.
    fn natural_view_for_window(&self, window_id: u64) -> ViewMode {
        let has_pty = self
            .selection_for_window(window_id)
            .and_then(|sel| {
                sel.session_id()
                    .and_then(|sid| self.sessions.iter().find(|s| s.id == sid))
                    .map(|s| s.has_pty)
            })
            .unwrap_or(false);
        if has_pty {
            ViewMode::Terminal
        } else {
            ViewMode::Chat
        }
    }

    /// View mode for a specific split pane. The focused window uses the live
    /// `self.view`; non-focused windows use their remembered per-window mode
    /// (or, absent one, their session's natural mode). `None` and the active
    /// window id both resolve to the live mode so single-window and zoomed
    /// renders are unaffected.
    pub fn view_for_window(&self, window_id: Option<u64>) -> ViewMode {
        match window_id {
            Some(id) if id != self.active_window_id => self
                .window_views
                .get(&id)
                .copied()
                .unwrap_or_else(|| self.natural_view_for_window(id)),
            _ => self.view,
        }
    }

    /// Set the focused window's view mode, remembering it per-window so each
    /// split pane keeps an independent transcript/terminal mode across focus
    /// changes.
    fn set_active_view(&mut self, mode: ViewMode) {
        self.view = mode;
        self.window_views.insert(self.active_window_id, mode);
    }

    fn leaf_window_ids(&self) -> Vec<u64> {
        fn collect(node: &MainWindowTree, out: &mut Vec<u64>) {
            match node {
                MainWindowTree::Leaf { id, .. } => out.push(*id),
                MainWindowTree::Split { first, second, .. } => {
                    collect(first, out);
                    collect(second, out);
                }
            }
        }
        let mut out = Vec::new();
        collect(&self.main_windows, &mut out);
        out
    }

    /// Focus the Nth pane (0-based) in the same `[list, …split windows]`
    /// ordering that `C-x o` cycles through: index 0 is the session list,
    /// index `k >= 1` is the `(k - 1)`th split window. Returns `false`
    /// (a no-op) when the requested pane doesn't exist — e.g. `C-5` with
    /// only two split windows open. Bound to `C-2`..`C-5` (so `C-2`
    /// focuses the first split window).
    fn focus_pane_by_index(&mut self, index: usize) -> bool {
        if index == 0 {
            // Pane 1 is the session list.
            self.collapse_orchestrator_panel_on_focus_change();
            if matches!(self.zoom, ZoomMode::View) {
                self.zoom = ZoomMode::List;
            }
            self.focus = PaneFocus::List;
            self.set_status("focus: list".into());
            return true;
        }
        let ids = self.leaf_window_ids();
        let Some(id) = ids.get(index - 1).copied() else {
            return false;
        };
        self.collapse_orchestrator_panel_on_focus_change();
        // Asking for a window implies the view side should be visible.
        if matches!(self.zoom, ZoomMode::List) {
            self.zoom = ZoomMode::View;
        }
        self.focus_main_window(id);
        self.set_status(format!("focus: window {id}"));
        true
    }

    /// Find the split window spatially adjacent to the active window in the
    /// given direction, using last-frame pane geometry. Returns the target
    /// window id, or `None` when there's no neighbor that way (emacs
    /// `windmove` semantics). Only considers panes whose perpendicular span
    /// overlaps the active pane, and picks the closest one in the travel
    /// direction.
    fn adjacent_window_id(&self, dir: FocusDir) -> Option<u64> {
        let panes = &self.layout.main_window_areas;
        let cur = panes.iter().find(|p| p.id == self.active_window_id)?.area;
        let mut best: Option<(u64, u16)> = None;
        for p in panes {
            if p.id == self.active_window_id {
                continue;
            }
            let a = p.area;
            let v_overlap = a.top() < cur.bottom() && a.bottom() > cur.top();
            let h_overlap = a.left() < cur.right() && a.right() > cur.left();
            let (in_dir, dist) = match dir {
                FocusDir::Left => (v_overlap && a.x < cur.x, cur.x.saturating_sub(a.x)),
                FocusDir::Right => (v_overlap && a.x > cur.x, a.x.saturating_sub(cur.x)),
                FocusDir::Up => (h_overlap && a.y < cur.y, cur.y.saturating_sub(a.y)),
                FocusDir::Down => (h_overlap && a.y > cur.y, a.y.saturating_sub(cur.y)),
            };
            if in_dir && best.is_none_or(|(_, d)| dist < d) {
                best = Some((p.id, dist));
            }
        }
        best.map(|(id, _)| id)
    }

    /// Move keyboard focus to the split window adjacent to the active one in
    /// `dir`. Returns `false` (a no-op) when there's no neighbor that way.
    fn focus_adjacent_window(&mut self, dir: FocusDir) -> bool {
        let Some(id) = self.adjacent_window_id(dir) else {
            return false;
        };
        self.collapse_orchestrator_panel_on_focus_change();
        self.focus_main_window(id);
        self.set_status(format!("focus: window {id}"));
        true
    }

    /// Directional pane focus invoked from the keymap (`C-x <arrow>`). Unlike
    /// the bare `Shift+<arrow>` intercept — which is a silent consumed no-op so
    /// it doesn't beep when held down — an explicit chord reports when there's
    /// no window that way, since the user deliberately asked for the move.
    fn focus_window_in_dir(&mut self, dir: FocusDir) {
        if !self.focus_adjacent_window(dir) {
            let dir = match dir {
                FocusDir::Up => "above",
                FocusDir::Down => "below",
                FocusDir::Left => "left",
                FocusDir::Right => "right",
            };
            self.set_status(format!("no split window {dir}"));
        }
    }

    fn focus_main_window(&mut self, id: u64) {
        if let Some(selection) = self.selection_for_window(id) {
            let changed_selection = self.selection != selection;
            self.active_window_id = id;
            self.selection = selection;
            self.focus = PaneFocus::View;
            if changed_selection {
                // Only clear the transcript when switching to a real session.
                // Focusing a welcome-screen pane (Selection::None) has no
                // session to hydrate, so clearing here leaves other split
                // panes with an empty transcript and the "No structured chat
                // events" message until focus moves away again.
                if self.selection.session_id().is_some() {
                    self.transcript.clear();
                    self.transcript_session = None;
                }
                if !self.is_split_layout() {
                    self.view_scrollback = 0;
                }
                // Restore the focused window's view mode: a per-window `C-x t`
                // toggle persists across focus changes; without one, fall back
                // to the session's natural mode (Terminal for PTY sessions).
                self.view = self
                    .window_views
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| self.natural_view_for_window(id));
            }
            // Keep the program attached to the focused pane's session (stash
            // the outgoing one, reveal the incoming). A no-op when the
            // selection didn't actually change.
            self.sync_program_popup_with_selection();
            self.report_focused_sessions();
        }
    }

    fn split_active_window(&mut self, direction: WindowSplitDirection) {
        fn split(
            node: &mut MainWindowTree,
            target: u64,
            new_id: u64,
            direction: WindowSplitDirection,
        ) -> bool {
            match node {
                MainWindowTree::Leaf { id, selection } if *id == target => {
                    let old_id = *id;
                    let sel = selection.clone();
                    *node = MainWindowTree::Split {
                        direction,
                        ratio_percent: 50,
                        first: Box::new(MainWindowTree::Leaf {
                            id: old_id,
                            selection: sel.clone(),
                        }),
                        second: Box::new(MainWindowTree::Leaf {
                            id: new_id,
                            selection: sel,
                        }),
                    };
                    true
                }
                MainWindowTree::Leaf { .. } => false,
                MainWindowTree::Split { first, second, .. } => {
                    split(first, target, new_id, direction)
                        || split(second, target, new_id, direction)
                }
            }
        }
        let new_id = self.next_window_id;
        self.next_window_id = self.next_window_id.saturating_add(1);
        self.sync_active_window_selection();
        if split(
            &mut self.main_windows,
            self.active_window_id,
            new_id,
            direction,
        ) {
            self.focus_main_window(new_id);
            self.set_status(
                match direction {
                    WindowSplitDirection::Below => "split below — C-x o cycles windows",
                    WindowSplitDirection::Right => "split right — C-x o cycles windows",
                }
                .into(),
            );
        }
    }

    fn delete_active_window(&mut self) {
        fn is_target_leaf(node: &MainWindowTree, target: u64) -> bool {
            matches!(node, MainWindowTree::Leaf { id, .. } if *id == target)
        }

        fn remove(node: &mut MainWindowTree, target: u64) -> bool {
            match node {
                MainWindowTree::Leaf { .. } => false,
                MainWindowTree::Split { first, second, .. } => {
                    if is_target_leaf(first, target) {
                        *node = (**second).clone();
                        true
                    } else if is_target_leaf(second, target) {
                        *node = (**first).clone();
                        true
                    } else {
                        remove(first, target) || remove(second, target)
                    }
                }
            }
        }
        if self.leaf_window_ids().len() <= 1 {
            self.set_status("only one window".into());
            return;
        }
        let target = self.active_window_id;
        if remove(&mut self.main_windows, target) {
            if let Some(id) = self.leaf_window_ids().first().copied() {
                self.focus_main_window(id);
            }
            self.set_status("window deleted".into());
        }
    }

    fn delete_other_windows(&mut self) {
        self.sync_active_window_selection();
        let selection = self.selection.clone();
        self.main_windows = MainWindowTree::single(self.active_window_id, selection);
        self.set_status("only current window".into());
        self.report_focused_sessions();
    }

    fn set_split_ratio_by_order(&mut self, target_parent: u64, ratio: u16) -> bool {
        fn set(node: &mut MainWindowTree, target_parent: u64, next: &mut u64, ratio: u16) -> bool {
            match node {
                MainWindowTree::Leaf { .. } => false,
                MainWindowTree::Split {
                    ratio_percent,
                    first,
                    second,
                    ..
                } => {
                    let current = *next;
                    *next += 1;
                    if current == target_parent {
                        *ratio_percent = ratio.clamp(10, 90);
                        true
                    } else {
                        set(first, target_parent, next, ratio)
                            || set(second, target_parent, next, ratio)
                    }
                }
            }
        }
        let mut next = 1;
        set(&mut self.main_windows, target_parent, &mut next, ratio)
    }

    fn resize_active_window(&mut self, delta: i16, direction: WindowSplitDirection) {
        fn resize(
            node: &mut MainWindowTree,
            target: u64,
            delta: i16,
            want: WindowSplitDirection,
        ) -> bool {
            match node {
                MainWindowTree::Leaf { id, .. } => *id == target,
                MainWindowTree::Split {
                    direction,
                    ratio_percent,
                    first,
                    second,
                } => {
                    if resize(first, target, delta, want) {
                        if *direction == want {
                            *ratio_percent = (*ratio_percent as i16 + delta).clamp(10, 90) as u16;
                        }
                        true
                    } else if resize(second, target, delta, want) {
                        if *direction == want {
                            *ratio_percent = (*ratio_percent as i16 - delta).clamp(10, 90) as u16;
                        }
                        true
                    } else {
                        false
                    }
                }
            }
        }
        if resize(
            &mut self.main_windows,
            self.active_window_id,
            delta,
            direction,
        ) {
            self.set_status("window resized".into());
        }
    }

    /// Move the selection up or down by one row in the materialized list,
    /// wrapping at the ends. No-op if the list is empty.
    async fn step_selection(&mut self, delta: i32) {
        let items = self.list_items();
        if items.is_empty() {
            return;
        }
        // Every materialized row is a navigation stop, including "N archived"
        // disclosure rows (so left/right can expand/collapse them).
        let cur = items
            .iter()
            .position(|it| it.matches(&self.selection))
            .unwrap_or(0);
        let n = items.len() as i32;
        let mut next = ((cur as i32 + delta).rem_euclid(n)) as usize;
        if Self::is_collapsed_ungrouped_archive_row(&items[next]) {
            let direction = delta.signum();
            let primary_beyond_archive = if direction > 0 {
                items[next + 1..]
                    .iter()
                    .position(Self::is_primary_list_target)
                    .map(|offset| next + 1 + offset)
            } else {
                items[..next].iter().rposition(Self::is_primary_list_target)
            };
            if let Some(index) = primary_beyond_archive {
                next = index;
            }
        }
        match &items[next] {
            ListItem::Session { summary, .. } => self.select_session(summary.id.clone()),
            ListItem::GroupHeader { group, .. } => self.select_group(group.id.clone()),
            ListItem::ArchivedRow { section, .. } => self.select_archive_row(section.clone()),
        }
        self.sync_active_window_selection();
    }

    fn is_collapsed_ungrouped_archive_row(item: &ListItem) -> bool {
        matches!(
            item,
            ListItem::ArchivedRow {
                section: ArchiveSection::Ungrouped,
                expanded: false,
                ..
            }
        )
    }

    fn is_primary_list_target(item: &ListItem) -> bool {
        matches!(
            item,
            ListItem::Session { summary, .. } if !summary.archived
        ) || matches!(item, ListItem::GroupHeader { .. })
    }

    /// After any list mutation, make sure `self.selection` still refers to
    /// an item we know about. Fall back to the first list item if not.
    /// If `id` is selected or visible in a split pane, move it to its nearest
    /// visible neighbor in the list. Must be called *before* the session is
    /// removed from or hidden in `list_items()` (i.e. before removing it from
    /// `self.sessions` or marking it archived).
    fn focus_neighbor_of(&mut self, id: &str) {
        let items = self.list_items();
        let Some(pos) = items
            .iter()
            .position(|it| matches!(it, ListItem::Session { summary, .. } if summary.id == id))
        else {
            return;
        };
        let pick_active = |it: &ListItem| -> Option<Selection> {
            match it {
                ListItem::Session { summary, .. } if !summary.archived => {
                    Some(Selection::Session(summary.id.clone()))
                }
                ListItem::GroupHeader { group, .. } => Some(Selection::Group(group.id.clone())),
                _ => None,
            }
        };
        let candidate = items[pos + 1..]
            .iter()
            .find_map(pick_active)
            .or_else(|| items[..pos].iter().rev().find_map(pick_active));
        let replacement = candidate.unwrap_or(Selection::None);
        self.main_windows
            .replace_session_selection(id, &replacement);
        if self.selection == Selection::Session(id.to_string()) {
            self.selection = replacement;
            self.transcript.clear();
            self.transcript_session = None;
            self.transcript_scroll = u16::MAX;
            let active_window = Some(self.active_window_id);
            self.set_scrollback_for_window(active_window, 0);
            let natural = if self.selected_session().map(|s| s.has_pty).unwrap_or(false) {
                ViewMode::Terminal
            } else {
                ViewMode::Chat
            };
            self.set_active_view(natural);
        }
    }

    fn ensure_selection_valid(&mut self) {
        // A session selection stays valid as long as the session still
        // exists, even when it has no navigable list row — e.g. a subagent,
        // which renders only as a child of its parent (and never at all when
        // the parent is the hidden orchestrator) but is reachable through a
        // program session clip. The main view can display any live session;
        // the list simply won't highlight a row for it. Without this, clicking
        // such a clip selects the session and the next session-list refresh
        // would immediately revert the selection (popping the stashed program
        // back open over the would-be target).
        if let Selection::Session(id) = &self.selection {
            if self.sessions.iter().any(|s| s.id == *id) {
                return;
            }
        }
        let items = self.list_items();
        if items.iter().any(|it| it.matches(&self.selection)) {
            return;
        }
        // Prefer a real session/group; fall back to an archived row only if
        // that's the only thing left in the list.
        self.selection = items
            .iter()
            .find_map(|it| match it {
                ListItem::Session { summary, .. } => Some(Selection::Session(summary.id.clone())),
                ListItem::GroupHeader { group, .. } => Some(Selection::Group(group.id.clone())),
                ListItem::ArchivedRow { .. } => None,
            })
            .or_else(|| {
                items.iter().find_map(|it| match it {
                    ListItem::ArchivedRow { section, .. } => {
                        Some(Selection::ArchivedRow(section.clone()))
                    }
                    _ => None,
                })
            })
            .unwrap_or(Selection::None);
    }

    /// True when the session is currently rendered somewhere on screen:
    /// a pane of the active window (including splits), the orchestrator
    /// panel, or the pinned strip. Used to decide whether a background
    /// `Pty` chunk needs an immediate full-frame repaint. Conservative —
    /// treats pinned / orchestrator as always-visible so the gate never
    /// drops a needed redraw; the cost of an occasional extra paint is
    /// far cheaper than missing one.
    fn session_visible_on_screen(&self, id: &str) -> bool {
        if self
            .main_windows
            .visible_session_ids()
            .iter()
            .any(|s| *s == id)
        {
            return true;
        }
        if self.orchestrator_id.as_deref() == Some(id) {
            return true;
        }
        self.sessions.iter().any(|s| s.id == id && s.pinned)
    }

    async fn on_notification(&mut self, n: agentd_protocol::Notification) {
        // Default: assume the notification changes something visible, so the
        // run loop repaints. Only an off-screen `Pty` chunk clears this (see
        // the `Pty` arm below) — every other event kind keeps it set.
        self.notification_dirtied_view = true;
        match n.method.as_str() {
            m if m == agentd_protocol::ipc_notif::EVENT => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<EventNotificationPayload>(p) {
                        self.matrix_rain.observe_event(
                            &payload.event,
                            self.matrix_rain_intensity,
                            &payload.session_id,
                        );
                        // Tool-approval prompt: if no minibuffer is in use,
                        // open the approval prompt for the matching session.
                        // Otherwise the user sees the request in the
                        // transcript and can resume via `C-x .` (future).
                        if let SessionEvent::ToolApprovalRequest {
                            call_id,
                            tool,
                            args_summary,
                            risk,
                            allow_auto_review,
                        } = &payload.event
                        {
                            self.pending_tool_approvals
                                .entry(payload.session_id.clone())
                                .or_default()
                                .insert(call_id.clone());
                            self.maybe_open_approval_prompt(
                                payload.session_id.clone(),
                                call_id.clone(),
                                tool.clone(),
                                args_summary.clone(),
                                *risk,
                                *allow_auto_review,
                            );
                            // Also fall through so the transcript records it.
                        }
                        // Approval resolved (answered here, in the smith
                        // PTY, or by another client): close our minibuffer
                        // prompt for it if it's still up, so it doesn't
                        // linger after the decision was already made.
                        if let SessionEvent::ToolApprovalResolved { call_id } = &payload.event {
                            self.clear_pending_tool_approval(&payload.session_id, call_id);
                            self.dismiss_approval_prompt(&payload.session_id, call_id);
                        }
                        if matches!(payload.event, SessionEvent::Reset) {
                            self.histories.remove(&payload.session_id);
                            self.block_hits.remove(&payload.session_id);
                            self.editor_states.remove(&payload.session_id);
                            self.agent_statuses.remove(&payload.session_id);
                            self.pending_tool_approvals.remove(&payload.session_id);
                            self.browser_previews.remove(&payload.session_id);
                            self.ui_panels.remove(&payload.session_id);
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
                        if let SessionEvent::ToolUse { tool, args, .. } = &payload.event {
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
                        // Typed client-routed slash commands. The adapter sent
                        // a `CommandId`; reconstruct the canonical verb from the
                        // registry and reuse the same dispatcher as the palette.
                        if let SessionEvent::ClientCommand { id, args } = &payload.event {
                            let verb = agentd_protocol::slash::SlashCommand::by_id(*id)
                                .name
                                .trim_start_matches('/');
                            let full = match args {
                                Some(a) => format!("{verb} {a}"),
                                None => verb.to_string(),
                            };
                            self.run_slash_command(&full).await;
                        }
                        // PTY events: feed into the per-session items history.
                        if let SessionEvent::Pty { .. } = &payload.event {
                            let now = Instant::now();
                            let bytes = payload.event.pty_bytes();
                            let mut is_active = true;
                            if let Some(b) = bytes.as_deref() {
                                if !agentd_protocol::is_pty_active_payload(b) {
                                    is_active = false;
                                }
                                let history = self
                                    .histories
                                    .entry(payload.session_id.clone())
                                    .or_default();
                                history.feed_pty(b);
                            }
                            if is_active {
                                // Mark the session as freshly active for the spinner.
                                self.pty_activity.insert(payload.session_id.clone(), now);
                            }
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
                            // A PTY chunk only changes the screen if this
                            // session is currently rendered. For off-screen
                            // sessions we've still warmed history / spinner /
                            // rain above, but the visible frame is unchanged —
                            // let the loop skip the immediate repaint (the tick
                            // still animates the list + rain at ~8fps).
                            self.notification_dirtied_view =
                                self.session_visible_on_screen(&payload.session_id);
                            return;
                        }
                        // Tool events feed the same history so the
                        // items-model renderer can synthesize block
                        // visuals from structured content. The
                        // adapter writes OSC fences around each tool
                        // block in the PTY stream; the history pairs
                        // ToolUse events to those fences by FIFO
                        // arrival order, and matches ToolResults by
                        // the explicit `call_id` field (with a legacy
                        // fallback to the `tool` field for old
                        // transcripts). Tool events from the
                        // orchestrator session also land here.
                        if let SessionEvent::ToolUse { tool, args, .. } = &payload.event {
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
                        if let SessionEvent::ToolResult {
                            tool,
                            ok,
                            output,
                            call_id,
                        } = &payload.event
                        {
                            let history = self
                                .histories
                                .entry(payload.session_id.clone())
                                .or_default();
                            // Correlate by the explicit `call_id` when present;
                            // legacy transcripts (call_id == None) still carry
                            // the id in the `tool` field, so fall back to it.
                            history.feed_tool_result(
                                call_id.as_deref().unwrap_or(tool),
                                *ok,
                                crate::pty_render::tool_output_preview_for_history(output),
                            );
                        }
                        // Headless sessions (any harness) emit their
                        // conversation as structured Message/Reasoning
                        // events with no PTY, so fold the prose into the
                        // items history. PTY-backed sessions already carry
                        // it in the PTY stream, so skip them to avoid
                        // double-rendering. (Streaming arrives as many
                        // same-kind deltas; `feed_message` coalesces.)
                        let msg = match &payload.event {
                            SessionEvent::Message { role, text } => Some((
                                match role {
                                    agentd_protocol::MessageRole::User => {
                                        crate::pty_render::MessageKind::User
                                    }
                                    _ => crate::pty_render::MessageKind::Assistant,
                                },
                                text.clone(),
                            )),
                            SessionEvent::Reasoning { text } => {
                                Some((crate::pty_render::MessageKind::Reasoning, text.clone()))
                            }
                            _ => None,
                        };
                        if let Some((kind, text)) = msg {
                            let headless = self
                                .sessions
                                .iter()
                                .find(|s| s.id == payload.session_id)
                                .map(crate::ui::is_headless)
                                .unwrap_or(false);
                            if headless {
                                let history = self
                                    .histories
                                    .entry(payload.session_id.clone())
                                    .or_default();
                                history.feed_message(kind, &text);
                            }
                            // Accumulate the orchestrator's streaming assistant
                            // text; finalized into a typewriter monolog at turn
                            // end (the AgentStatus active=false handler below).
                            if matches!(kind, crate::pty_render::MessageKind::Assistant)
                                && self.orchestrator_id.as_deref()
                                    == Some(payload.session_id.as_str())
                            {
                                self.operator_utterance.push_str(&text);
                            }
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
                        if let SessionEvent::BrowserPreview(preview) = &payload.event {
                            self.insert_browser_preview(
                                payload.session_id.clone(),
                                preview.clone(),
                            );
                            return;
                        }
                        match &payload.event {
                            SessionEvent::Diff { .. } => {}
                            SessionEvent::UiPanel(panel) => {
                                self.ui_panels
                                    .entry(payload.session_id.clone())
                                    .or_default()
                                    .insert(panel.id.clone(), panel.clone());
                                if panel.placement == agentd_protocol::UiPlacement::Inline {
                                    self.dynamic_ui_focused =
                                        Some((payload.session_id.clone(), panel.id.clone()));
                                } else {
                                    let until = Instant::now()
                                        + Duration::from_secs(DYNAMIC_UI_AUTOHIDE_SECS);
                                    self.dynamic_ui_temporary_until.insert(
                                        (payload.session_id.clone(), panel.id.clone()),
                                        until,
                                    );
                                    if self.orchestrator_id.as_deref()
                                        == Some(payload.session_id.as_str())
                                    {
                                        self.matrix_widget_hover = Some(MatrixWidgetHover {
                                            panel_id: panel.id.clone(),
                                            until,
                                        });
                                    }
                                }
                            }
                            SessionEvent::UiDelete { id } => {
                                if let Some(panels) = self.ui_panels.get_mut(&payload.session_id) {
                                    panels.remove(id);
                                    if panels.is_empty() {
                                        self.ui_panels.remove(&payload.session_id);
                                    }
                                }
                                let key = (payload.session_id.clone(), id.clone());
                                self.dynamic_ui_selected.remove(&key);
                                self.dynamic_ui_temporary_until.remove(&key);
                                if self
                                    .dynamic_ui_hover
                                    .as_ref()
                                    .is_some_and(|h| h.session_id == key.0 && h.panel_id == key.1)
                                {
                                    self.dynamic_ui_hover = None;
                                }
                                if self.dynamic_ui_focused.as_ref() == Some(&key) {
                                    self.dynamic_ui_focused = None;
                                }
                                if self.orchestrator_id.as_deref()
                                    == Some(payload.session_id.as_str())
                                {
                                    if self.matrix_widget_pinned.as_deref() == Some(id.as_str()) {
                                        self.matrix_widget_pinned = None;
                                    }
                                    if self
                                        .matrix_widget_hover
                                        .as_ref()
                                        .is_some_and(|h| h.panel_id == *id)
                                    {
                                        self.matrix_widget_hover = None;
                                    }
                                }
                            }
                            _ => {}
                        }
                        if let SessionEvent::AgentStatus(status) = &payload.event {
                            let is_orchestrator = self.orchestrator_id.as_deref()
                                == Some(payload.session_id.as_str());
                            if status.active {
                                // NOTE: `active=true` fires on *every* delta (a
                                // per-token "Working" heartbeat), not just at
                                // turn start — so we must NOT clear the utterance
                                // here, or only the final delta would survive
                                // (e.g. "noted" → "ed"). The accumulator is
                                // cleared at turn end (finalize, below), so each
                                // turn already starts clean.
                                self.agent_statuses
                                    .insert(payload.session_id.clone(), status.clone());
                            } else {
                                self.agent_statuses.remove(&payload.session_id);
                                // Turn end — consolidate the accumulated text into
                                // a single typewriter monolog over the matrix rain.
                                if is_orchestrator {
                                    if let Some(text) =
                                        operator_monolog_text(&self.operator_utterance)
                                    {
                                        self.operator_monolog = Some(OperatorMonolog {
                                            text,
                                            started_at: Instant::now(),
                                        });
                                    }
                                    self.operator_utterance.clear();
                                }
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
                        // Move focus before the session is hidden behind the
                        // archive disclosure row.
                        if payload.session.archived {
                            self.focus_neighbor_of(&id);
                        }
                        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
                            self.sessions[i] = payload.session;
                        } else {
                            self.sessions.push(payload.session);
                            self.sessions
                                .sort_by(|a, b| b.created_at.cmp(&a.created_at));
                        }
                        // If the session that just changed is visible on screen,
                        // consume its marker right away.
                        if self.session_visible_on_screen(&id)
                            && self
                                .sessions
                                .iter()
                                .find(|s| s.id == id)
                                .map(|s| s.needs_attention)
                                .unwrap_or(false)
                        {
                            self.report_seen(&id);
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
            m if m == agentd_protocol::ipc_notif::PROJECT_STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::ProjectStateNotificationPayload,
                    >(p)
                    {
                        self.on_group_state(payload.project).await;
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
            m if m == agentd_protocol::ipc_notif::PROJECT_DELETED => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::ProjectDeletedNotificationPayload,
                    >(p)
                    {
                        self.on_group_deleted(&payload.project_id).await;
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
            m if m == agentd_protocol::ipc_notif::PROGRAM_STATE => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::ProgramStateNotificationPayload,
                    >(p)
                    {
                        self.on_program_state(payload.program, payload.active_run, payload.blocks);
                    }
                }
            }
            m if m == agentd_protocol::ipc_notif::PROGRAM_CURSOR => {
                if let Some(p) = n.params {
                    if let Ok(payload) = serde_json::from_value::<
                        agentd_protocol::ProgramCursorNotificationPayload,
                    >(p)
                    {
                        self.on_program_cursor(payload.cursor);
                    }
                }
            }
            _ => {}
        }
    }

    fn on_program_cursor(&mut self, cursor: agentd_protocol::ProgramCursor) {
        let is_own = self.own_program_client_id.as_deref() == Some(cursor.client_id.as_str());
        if cursor.active {
            if cursor.kind == "agent" {
                let existing = self.program_agent_reveal_receipts.get(&cursor.client_id);
                let receipt = program_agent_reveal_receipt_update(
                    existing,
                    cursor.updated_at_ms,
                    Instant::now(),
                );
                self.program_agent_reveal_receipts
                    .insert(cursor.client_id.clone(), receipt);
            }
            self.program_collaborators
                .insert(cursor.client_id.clone(), cursor.clone());
            if is_own {
                self.apply_own_program_cursor(&cursor);
            }
        } else {
            self.program_collaborators.remove(&cursor.client_id);
            self.program_agent_reveal_receipts.remove(&cursor.client_id);
        }
    }

    /// Elapsed time since this agent cursor's `updated_at_ms` last genuinely
    /// advanced, per the local receipt clock `on_program_cursor` maintains.
    /// `None` when no receipt has been recorded for `client_id` (not an
    /// agent-kind cursor, or never observed live) — callers should treat that
    /// as "not fresh" rather than falling back to the daemon's timestamp,
    /// which is what made the reveal invisible in the first place.
    pub(crate) fn program_agent_reveal_elapsed(
        &self,
        client_id: &str,
        now: Instant,
    ) -> Option<Duration> {
        self.program_agent_reveal_receipts
            .get(client_id)
            .map(|(_, receipt_at)| now.saturating_duration_since(*receipt_at))
    }

    fn apply_own_program_cursor(&mut self, cursor: &agentd_protocol::ProgramCursor) {
        let updating_session_id = cursor.session_id.clone();
        let popup = if self
            .program_popup
            .as_ref()
            .is_some_and(|p| p.program.session_id == updating_session_id)
        {
            self.program_popup.as_mut()
        } else {
            self.program_popups.get_mut(&updating_session_id)
        };
        let Some(popup) = popup else {
            return;
        };
        if program_popup_has_unsaved_edits(popup) {
            return;
        }
        let buffer_len = popup.buffer.chars().count();
        let incoming_cursor = cursor.cursor.min(buffer_len);
        let incoming_selection = match (cursor.selection_anchor, cursor.selection_head) {
            (Some(anchor), Some(head)) => Some((anchor.min(buffer_len), head.min(buffer_len))),
            _ => None,
        };
        // The daemon no longer re-broadcasts a plain cursor publish back to
        // its own source connection, so most notifications reaching here are
        // genuine daemon-side rebases — our offsets shifted by another
        // client's edit — which is the case this method exists for. The
        // equality check below still matters: with adopt-side rebasing in
        // `on_program_state`, our own rebase of a just-adopted edit often
        // lands on the exact same offsets the daemon's rebase broadcast
        // carries, and applying that redundant "update" is not a no-op — it
        // resets `preferred_col` (losing C-n/C-p column stickiness) and used
        // to collapse a zero-width C-Space mark to "no selection" before the
        // first motion key could extend it.
        let local_selection = popup.selection.as_ref().map(|s| (s.anchor, s.head));
        if incoming_cursor == popup.cursor && incoming_selection == local_selection {
            return;
        }
        popup.cursor = incoming_cursor;
        // A zero-width pair is a C-Space mark awaiting its first motion, not
        // "no selection" — keep it alive across the rebase so the next motion
        // key extends from the rebased mark instead of merely moving.
        popup.selection = incoming_selection.map(|(anchor, head)| ProgramSelection {
            anchor,
            head,
            dragged: false,
        });
        popup.preferred_col = None;
        Self::update_program_smart_clip_after_cursor_move(popup);
    }

    fn adopt_daemon_program_run(
        &mut self,
        session_id: &str,
        active_run: Option<agentd_protocol::ProgramRunProgress>,
    ) {
        match active_run.and_then(ProgramRun::from_progress) {
            Some(run) => {
                self.program_runs.insert(session_id.to_string(), run);
            }
            None => {
                self.program_runs.remove(session_id);
            }
        }
    }

    fn adopt_program_state_run(
        &mut self,
        session_id: &str,
        active_run: Option<agentd_protocol::ProgramRunProgress>,
    ) {
        match active_run.and_then(ProgramRun::from_progress) {
            Some(run) => {
                self.program_runs.insert(session_id.to_string(), run);
            }
            None => {
                if self
                    .program_runs
                    .get(session_id)
                    .is_some_and(|run| run.daemon_confirmed)
                {
                    self.program_runs.remove(session_id);
                }
            }
        }
    }

    /// A program changed on the daemon (most often the owning agent edited it).
    /// Keep any open popup for that session in sync: when the user has no
    /// unsaved edits, adopt the new content live so they see the agent's
    /// changes and our tracked version stays fresh. When the user is mid-edit,
    /// leave the buffer and the (now stale) base version alone — the
    /// merge-on-save path reconciles both sides without losing either.
    fn on_program_state(
        &mut self,
        program: agentd_protocol::ProgramDocument,
        active_run: Option<agentd_protocol::ProgramRunProgress>,
        blocks: Vec<agentd_protocol::ProgramBlockView>,
    ) {
        let previous_pending = self
            .program_runs
            .get(&program.session_id)
            .map(|run| run.pending.clone());
        let next_pending = active_run.as_ref().map(program_run_progress_pending_ids);
        if let (Some(previous_pending), Some(next_pending)) =
            (previous_pending.as_ref(), next_pending.as_ref())
        {
            self.record_program_settle_flourishes(
                &program.session_id,
                previous_pending,
                next_pending,
                Instant::now(),
            );
        }
        self.adopt_program_state_run(&program.session_id, active_run);
        let updating_session_id = program.session_id.clone();
        let popup = if self
            .program_popup
            .as_ref()
            .is_some_and(|p| p.program.session_id == updating_session_id)
        {
            self.program_popup.as_mut()
        } else {
            self.program_popups.get_mut(&updating_session_id)
        };
        let Some(popup) = popup else {
            return;
        };
        if program.version <= popup.program.version {
            return;
        }
        if program_popup_has_unsaved_edits(popup) {
            // Don't clobber unsaved edits. Keep the stale base version so the
            // next save detects the conflict and 3-way merges both sides.
            return;
        }
        // Rebase the caret, selection, and search anchor through the old→new
        // content diff instead of merely clamping them to the new length
        // (spec 0065). A clamp alone leaves them pointing at whatever text
        // now occupies their old offset — usually garbage — whenever the
        // adopted change inserted or removed text before them; a rebase
        // keeps them pinned to the same logical position in the document.
        let diff_span = program_document_diff_span(&popup.buffer, &program.markdown);
        popup.buffer = program.markdown.clone();
        popup.saved_markdown = program.markdown.clone();
        popup.blocks = blocks;
        popup.program = program;
        let buffer_len = popup.buffer.chars().count();
        let rebase = |pos: usize| -> usize {
            match diff_span {
                Some(span) => program_rebase_position(pos, span),
                None => pos,
            }
            .min(buffer_len)
        };
        popup.cursor = rebase(popup.cursor);
        if let Some(selection) = popup.selection.as_mut() {
            selection.anchor = rebase(selection.anchor);
            selection.head = rebase(selection.head);
        }
        if let Some(search) = popup.search.as_mut() {
            search.anchor_cursor = rebase(search.anchor_cursor);
        }
        popup.preferred_col = None;
        popup.undo_stack.clear();
        if popup.search.is_some() {
            self.refresh_program_search_for_session(&updating_session_id);
        }
    }

    /// Open the approval prompt if there's no other minibuffer in flight.
    /// Best-effort: if the user is already typing something, we skip and
    /// leave the request visible in the transcript only.
    /// Close the minibuffer approval prompt for `call_id` if it's still
    /// showing — the approval was answered here, in the session's PTY, or
    /// by another client, so a lingering prompt would be stale.
    fn dismiss_approval_prompt(&mut self, session_id: &str, call_id: &str) {
        let is_match = matches!(
            self.minibuffer.as_ref().map(|mb| &mb.intent),
            Some(MinibufferIntent::ApproveTool { session_id: s, call_id: c, .. })
                if s == session_id && c == call_id
        );
        if is_match {
            self.minibuffer = None;
        }
    }

    fn clear_pending_tool_approval(&mut self, session_id: &str, call_id: &str) {
        let Some(pending) = self.pending_tool_approvals.get_mut(session_id) else {
            return;
        };
        pending.remove(call_id);
        if pending.is_empty() {
            self.pending_tool_approvals.remove(session_id);
        }
    }

    pub fn operator_has_pending_approval(&self) -> bool {
        let Some(orchestrator_id) = self.orchestrator_id.as_deref() else {
            return false;
        };
        self.pending_tool_approvals
            .get(orchestrator_id)
            .is_some_and(|pending| !pending.is_empty())
    }

    pub fn operator_loop_disabled(&self) -> bool {
        let Some(id) = self.orchestrator_id.as_deref() else {
            return false;
        };
        self.sessions
            .iter()
            .find(|s| s.id == id)
            .is_some_and(|s| s.operator_loop_disabled)
    }

    fn toggle_orchestrator_panel(&mut self) {
        if self.is_orchestrator_panel_open() {
            self.minibuffer = None;
        } else {
            self.open_minibuffer_for_command();
        }
    }

    fn maybe_open_approval_prompt(
        &mut self,
        session_id: String,
        call_id: String,
        tool: String,
        args_summary: String,
        risk: agentd_protocol::ToolRisk,
        allow_auto_review: bool,
    ) {
        // Smith approvals are rendered inline in the session PTY
        // (the `? approve [risk] tool(args) — y/n/a` row). The user
        // responds with a single key inside the session terminal,
        // not via a separate minibuffer prompt — so skip ours.
        if self.session_renders_approval_inline(&session_id) {
            return;
        }
        // Only surface the global minibuffer prompt for the session the
        // user is currently looking at. Background sessions still render
        // their approval request inline in their own terminal/transcript.
        if self.selection.session_id() != Some(session_id.as_str()) {
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
        let auto_review_option = if allow_auto_review {
            "  a=auto-review"
        } else {
            ""
        };
        let prompt = format!(
            "approve [{risk_label}] {tool}({}) ▸ y=approve  n=deny{auto_review_option}  f=unsafe-auto",
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
                allow_auto_review,
            },
            error: None,
        });
    }

    fn session_renders_approval_inline(&self, session_id: &str) -> bool {
        self.orchestrator_id.as_deref() == Some(session_id)
            || self
                .sessions
                .iter()
                .any(|s| s.id == session_id && s.harness == "smith")
    }

    /// Cycle the selected session's approval mode.
    pub async fn cycle_approval_mode(&mut self) {
        self.cycle_approval_mode_with_status(true).await;
    }

    pub async fn cycle_approval_mode_silent(&mut self) {
        self.cycle_approval_mode_with_status(false).await;
    }

    async fn cycle_approval_mode_with_status(&mut self, show_status: bool) {
        let Some(s) = self.selected_session() else {
            if show_status {
                self.set_status("no session selected".into());
            }
            return;
        };
        let id = s.id.clone();
        let next = match s.approval_mode {
            agentd_protocol::ApprovalMode::Manual => agentd_protocol::ApprovalMode::AutoReview,
            agentd_protocol::ApprovalMode::AutoReview => agentd_protocol::ApprovalMode::UnsafeAuto,
            agentd_protocol::ApprovalMode::UnsafeAuto => agentd_protocol::ApprovalMode::Manual,
        };
        match self.client.set_approval_mode(&id, next).await {
            Ok(()) if show_status => self.set_status(format!(
                "approval mode {}",
                next.badge().unwrap_or("manual")
            )),
            Ok(()) => {}
            Err(e) => self.set_status(format!("set_approval_mode failed: {e}")),
        }
    }

    async fn on_session_deleted(&mut self, id: &str) {
        self.focus_neighbor_of(id);
        if let Some(i) = self.sessions.iter().position(|s| s.id == id) {
            self.sessions.remove(i);
        }
        if self.transcript_session.as_deref() == Some(id) {
            self.transcript.clear();
            self.transcript_session = None;
        }
        self.histories.remove(id);
        self.block_hits.remove(id);
        self.pending_tool_approvals.remove(id);
        self.ui_panels.remove(id);
        self.pty_activity.remove(id);
        self.matrix_rain.forget_session(id);
        // Program state is keyed by session id and otherwise never expires —
        // left uncleaned, a long-running TUI that opens/closes Program views
        // across many short-lived sessions accumulates stale entries forever.
        // `open_program_session_ids()` sorts every entry in `program_popups`
        // and runs once per visible split pane per frame, so this leak turns
        // into an ever-growing per-frame cost that gets paid once per split —
        // exactly the "lag after deleting a session, then splitting" reports.
        if self
            .program_popup
            .as_ref()
            .is_some_and(|popup| popup.program.session_id == id)
        {
            self.program_popup = None;
        }
        self.program_popups.remove(id);
        self.program_runs.remove(id);
        self.program_settle_flourishes.remove(id);
        self.program_view_memory.remove(id);
        self.program_collaborators
            .retain(|_, cursor| cursor.session_id != id);
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
            CtEvent::Paste(text) => self.on_paste(text).await,
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
        const LIST_STEP: i32 = 3;
        let scrollback_step = self.mouse_scrollback_step();
        // Track every event's cell so hover-aware rendering (diamond
        // tooltip, program shimmer preview, etc.) has a current position to
        // render against.
        let next_pos = (ev.column, ev.row);
        self.mouse_pos = Some(next_pos);
        // If the cursor is over a pane whose child has grabbed the mouse
        // (e.g. Claude Code in fullscreen), forward the event into that PTY and
        // stop — construct becomes a transparent mouse pipe for the pane, so
        // the child's own scroll/click/selection handling works. Skipped while
        // a construct-owned drag gesture (pane/list resize, scrollbar, text
        // selection) is in flight so those finish under our control, and while
        // the session-title menu is open — that overlay paints over the pane
        // content, so its action rows must take mouse priority over the child
        // (otherwise a mouse-grabbing child swallows every menu click and the
        // split/close/rename actions silently do nothing).
        if self.handle_program_mouse(&ev).await {
            return;
        }
        // URL clicks must be intercepted before the child-mouse-forward path so
        // that clicking a hovered URL inside a mouse-grabbing session (e.g. Claude
        // Code in full-screen mode) actually opens the browser.  The hover
        // underline is rendered every frame — if we show the link as clickable it
        // must respond to clicks regardless of whether the child owns the mouse.
        if matches!(ev.kind, MouseEventKind::Up(MouseButton::Left)) {
            if let Some(hit) = self.url_hit_at(ev.column, ev.row) {
                match open_url(&hit.url) {
                    Ok(()) => self.set_status(format!("opened {}", hit.url)),
                    Err(e) => self.set_status(format!("open URL failed: {e}")),
                }
                return;
            }
        }
        if self.session_title_menu.is_none()
            && self.resizing_list.is_none()
            && self.resizing_pin_strip.is_none()
            && self.resizing_orchestrator_panel.is_none()
            && self.resizing_matrix_rain.is_none()
            && self.resizing_main_window.is_none()
            && self.dragging_terminal_scrollbar.is_none()
            && self.text_selection.is_none()
            && self.forward_mouse_to_child(&ev)
        {
            return;
        }
        match ev.kind {
            MouseEventKind::ScrollUp => {
                if !self.adjust_mouse_dynamic_ui_scroll(ev.column, ev.row, -LIST_STEP)
                    && !self.adjust_mouse_list_scroll(ev.column, ev.row, -LIST_STEP)
                {
                    self.adjust_mouse_scrollback(ev.column, ev.row, scrollback_step);
                }
            }
            MouseEventKind::ScrollDown => {
                if !self.adjust_mouse_dynamic_ui_scroll(ev.column, ev.row, LIST_STEP)
                    && !self.adjust_mouse_list_scroll(ev.column, ev.row, LIST_STEP)
                {
                    self.adjust_mouse_scrollback(ev.column, ev.row, -scrollback_step);
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(hit) = self
                    .layout
                    .main_window_areas
                    .iter()
                    .find(|hit| {
                        let (x_start, x_end, y) = crate::ui::view_close_button_range(hit.area);
                        ev.row == y && ev.column >= x_start && ev.column < x_end
                    })
                    .copied()
                {
                    self.focus_main_window(hit.id);
                    if let Some(session_id) = self.selected_id() {
                        self.open_session_title_menu(session_id, hit.area);
                    }
                    return;
                }
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
                // A `Below` split's resize divider is two rows tall — the upper
                // pane's bottom border *and* the lower pane's top border. That
                // lower row is the lower pane's title bar, where its program
                // status-glyph toggle sits. The toggle fires from the regular
                // click pipeline on mouse-up (`handle_left_click`); without this
                // exception the resize hit-test below would claim the mouse-down
                // first and the click never reached the toggle, so "show
                // program" silently failed on every non-top split pane. (Hide
                // kept working because the active program's own mouse handler
                // intercepts that click earlier.) The session-actions button is
                // already protected by the mouse-down handler above; let the
                // toggle glyph fall through the same way.
                let on_pane_program_toggle = self.layout.main_window_areas.iter().any(|pane| {
                    let (x_start, x_end, y) =
                        crate::ui::view_program_toggle_button_range(pane.area);
                    ev.row == y && ev.column >= x_start && ev.column < x_end
                });
                if !on_pane_program_toggle {
                    if let Some(hit) = self
                        .layout
                        .main_window_dividers
                        .iter()
                        .find(|hit| Self::rect_contains(hit.area, ev.column, ev.row))
                        .copied()
                    {
                        let anchor = match hit.direction {
                            WindowSplitDirection::Right => ev.column,
                            WindowSplitDirection::Below => ev.row,
                        };
                        self.resizing_main_window = Some((
                            hit.parent,
                            hit.direction,
                            anchor,
                            hit.ratio_percent,
                            hit.parent_area,
                        ));
                        return;
                    }
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
                // Operator/minibuffer panel: the top border is the panel's title
                // area and acts as a vertical resize handle.
                if self.is_on_orchestrator_panel_divider(ev.column, ev.row) {
                    let cur_h = self.layout.minibuffer_area.map(|a| a.height).unwrap_or(
                        self.orchestrator_panel_h
                            .unwrap_or(MINIBUFFER_PANEL_H_DEFAULT),
                    );
                    self.resizing_orchestrator_panel = Some((ev.row, cur_h));
                    return;
                }
                if self.begin_terminal_scrollbar_drag_or_jump(ev.column, ev.row) {
                    return;
                }
                if self.is_over_dynamic_ui_overlay(ev.column, ev.row) {
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
                } else if let Some((parent, direction, anchor, ratio, parent_area)) =
                    self.resizing_main_window
                {
                    let (delta, span) = match direction {
                        WindowSplitDirection::Right => (
                            ev.column as i32 - anchor as i32,
                            parent_area.width.max(1) as i32,
                        ),
                        WindowSplitDirection::Below => (
                            ev.row as i32 - anchor as i32,
                            parent_area.height.max(1) as i32,
                        ),
                    };
                    let delta_pct = (delta * 100) / span;
                    let next = (ratio as i32 + delta_pct).clamp(10, 90) as u16;
                    self.set_split_ratio_by_order(parent, next);
                } else if let Some((grab_offset, max_scrollback)) = self.dragging_terminal_scrollbar
                {
                    self.drag_terminal_scrollbar_to_row(ev.row, grab_offset, max_scrollback);
                } else if self.is_over_dynamic_ui_overlay(ev.column, ev.row) {
                    self.text_selection = None;
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
                    || self.dragging_terminal_scrollbar.is_some()
                    || self.resizing_matrix_rain.is_some()
                    || self.resizing_main_window.is_some();
                self.resizing_list = None;
                self.resizing_pin_strip = None;
                self.resizing_orchestrator_panel = None;
                self.dragging_terminal_scrollbar = None;
                self.resizing_matrix_rain = None;
                self.resizing_main_window = None;
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
                            Ok(outcome) => {
                                let n = text.chars().count();
                                self.selected_text = (!text.is_empty()).then_some(text);
                                self.selected_text_bounds = sel.bounds;
                                self.selected_text_range = self.selected_frame_range(&sel);
                                self.text_selection = None;
                                self.set_status(outcome.status(n));
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

    async fn handle_left_click(&mut self, col: u16, row: u16) {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(menu) = self.session_title_menu.clone() {
            if let Some(action) = menu.item_at(col, row) {
                self.run_session_title_menu_action(menu.session_id, action)
                    .await;
                return;
            }
            if menu.contains(col, row) {
                return;
            }
            self.session_title_menu = None;
        }
        if self.handle_dynamic_ui_overlay_click(col, row).await {
            return;
        }
        if let Some(modal) = self.layout.modal_area {
            if self.program_popup.is_some() {
                if contains(modal, col, row) {
                    self.place_program_cursor(modal, col, row);
                    return;
                }
                // A click outside the program never closes it. The program is a
                // per-session surface dismissed only via its title-glyph
                // toggle or C-x Space. Fall through so the click still selects
                // a session / focuses a pane; the program then follows the new
                // selection (the prior program is stashed, not destroyed) via
                // sync_program_popup_with_selection.
            } else if !contains(modal, col, row) {
                self.dismiss_modal();
                return;
            } else {
                // Other modals are informational/read-only. Clicks inside
                // them are consumed so they don't focus or activate controls
                // in panes underneath the modal.
                return;
            }
        }
        if let Some(hit) = self.url_hit_at(col, row) {
            match open_url(&hit.url) {
                Ok(()) => self.set_status(format!("opened {}", hit.url)),
                Err(e) => self.set_status(format!("open URL failed: {e}")),
            }
            return;
        }
        if self.handle_dynamic_ui_overlay_click(col, row).await {
            return;
        }
        if self
            .layout
            .modeline_approval_mode_hit
            .is_some_and(|hit| hit.contains(col, row))
        {
            self.cycle_approval_mode_silent().await;
            return;
        }
        // Matrix-rain horizontal reveal word: jump to the session that
        // produced it (issue #140). Checked before the pane hit-tests —
        // the rain panel is its own region, so this never shadows a real
        // list/view click.
        if let Some(hit) = self
            .matrix_reveal_hits
            .iter()
            .find(|h| h.contains(col, row))
            .cloned()
        {
            if self.sessions.iter().any(|s| s.id == hit.session_id) {
                self.focus = PaneFocus::List;
                self.select_session(hit.session_id);
            } else {
                self.set_status(format!(
                    "session for \u{201c}{}\u{201d} has ended",
                    hit.text
                ));
            }
            return;
        }
        // Clickable shortcut affordances (minibuffer hints, empty-state
        // onboarding shortcuts) dispatch their bound key action before
        // pane-level click handling.
        for hint in &self.layout.shortcut_hints {
            if row == hint.y && col >= hint.x_start && col < hint.x_end {
                let action = hint.action;
                self.run_action(action).await;
                return;
            }
        }
        if let Some(mb_area) = self.layout.minibuffer_area {
            if contains(mb_area, col, row) {
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
        if let Some(hit) = self
            .layout
            .main_window_areas
            .iter()
            .find(|hit| contains(hit.area, col, row))
            .copied()
        {
            let view = hit.area;
            self.focus_main_window(hit.id);
            if contains(view, col, row) {
                self.dynamic_ui_focused = None;
                if let Some((x_start, x_end, y)) = self.layout.browser_preview_close {
                    if row == y && col >= x_start && col < x_end {
                        if let Some(id) = self.selected_id() {
                            self.browser_previews.remove(&id);
                            self.layout.browser_preview_area = None;
                            self.layout.browser_preview_close = None;
                        }
                        return;
                    }
                }
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
                // Top-row session action button: 3-cell range at the
                // right edge of the top border. Click opens the dropdown.
                let (close_x_start, close_x_end, close_y) =
                    crate::ui::view_close_button_range(view);
                let (toggle_x_start, toggle_x_end, toggle_y) =
                    crate::ui::view_program_toggle_button_range(view);
                if self.selected_session().is_some()
                    && row == toggle_y
                    && col >= toggle_x_start
                    && col < toggle_x_end
                {
                    self.toggle_program_popup().await;
                    return;
                }
                if self.selected_id().is_some()
                    && row == close_y
                    && col >= close_x_start
                    && col < close_x_end
                {
                    if let Some(session_id) = self.selected_id() {
                        self.open_session_title_menu(session_id, view);
                    }
                    return;
                }
                if self.handle_dynamic_ui_overlay_click(col, row).await {
                    return;
                }
                self.dynamic_ui_focused = None;
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
        if self.program_popup.is_some() {
            // Remember caret + scroll so reopening restores them, matching the
            // toggle-close path.
            self.remember_program_view_state();
            self.program_popup = None;
            return;
        }
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
    /// - legacy `[bg]` / `[kill]` hit zones → `client.tool_action(...)`.
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
            // Legacy button-row check. Current tool blocks render
            // keyboard hints instead of buttons, but older block hit
            // geometry may still exist across a live upgrade.
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
        let active_window = Some(self.active_window_id);
        let next = adjusted_scrollback(self.scrollback_for_window(active_window), delta);
        self.set_scrollback_for_window(active_window, next);
        self.show_terminal_scrollbar();
    }

    /// Reveal the scrollback overlay for the window the user just scrolled (the
    /// active window). Keyed per window so siblings in a split layout — which
    /// are still at the bottom — don't flash a scrollbar too. Expired entries
    /// are pruned here so the map stays bounded to recently-scrolled windows.
    fn show_terminal_scrollbar(&mut self) {
        let now = Instant::now();
        self.terminal_scrollbar_visible_until
            .retain(|_, until| *until > now);
        self.terminal_scrollbar_visible_until
            .insert(self.active_window_id, now + TERMINAL_SCROLLBAR_TTL);
    }

    /// When the scrollback overlay should stay visible for `window_id`. `None`
    /// (zoomed / single-window render) falls back to the active window, mirroring
    /// `scrollback_for_window`.
    pub fn terminal_scrollbar_visible_until(&self, window_id: Option<u64>) -> Option<Instant> {
        let id = window_id.unwrap_or(self.active_window_id);
        self.terminal_scrollbar_visible_until.get(&id).copied()
    }

    fn mouse_scrollback_step(&self) -> i32 {
        let rows = self.active_pane_size().1.max(1) as i32;
        (rows / 4).clamp(6, 24)
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

    pub(crate) fn is_orchestrator_panel_open(&self) -> bool {
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
    pub fn active_pane_size(&self) -> (u16, u16) {
        self.window_pane_sizes
            .get(&self.active_window_id)
            .copied()
            .unwrap_or(self.terminal_pane_size)
    }

    /// The pane size a session is currently rendered at, if it occupies a
    /// visible window leaf. Distinct from [`Self::active_pane_size`]: in a
    /// split, a passive pane's session is rendered at *its* width, not the
    /// active pane's. Hydrating or resizing a specific session must use this —
    /// sizing a passive pane's parser to the active pane's width builds the
    /// vt100 grid at the wrong width, so an alt-screen harness (claude), whose
    /// content `set_size` never reflows, renders garbled until the pane is
    /// re-hydrated at its true width (which a focus-switch happens to force).
    pub fn session_pane_size(&self, id: &str) -> Option<(u16, u16)> {
        self.window_session_pane_sizes()
            .into_iter()
            .find(|(sid, _)| sid == id)
            .map(|(_, size)| size)
    }

    pub fn window_session_pane_sizes(&self) -> Vec<(String, (u16, u16))> {
        fn collect(
            node: &MainWindowTree,
            sizes: &HashMap<u64, (u16, u16)>,
            out: &mut Vec<(String, (u16, u16))>,
        ) {
            match node {
                MainWindowTree::Leaf { id, selection } => {
                    if let (Some(session_id), Some(size)) = (selection.session_id(), sizes.get(id))
                    {
                        if !out.iter().any(|(existing, _)| existing == session_id) {
                            out.push((session_id.to_string(), *size));
                        }
                    }
                }
                MainWindowTree::Split { first, second, .. } => {
                    collect(first, sizes, out);
                    collect(second, sizes, out);
                }
            }
        }
        let mut out = Vec::new();
        collect(&self.main_windows, &self.window_pane_sizes, &mut out);
        out
    }

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
        // In disconnected state, still allow standard keymap chords for
        // quitting and quick palette access. `/` commands are not
        // accepted here, but `C-x C-c` remains available.
        if !self.connected {
            let res = self.chord_state.handle(key, &self.keymap);
            self.chord_label = self.chord_state.label();
            match res {
                KeymapResult::Action(KeyAction::Quit) => {
                    self.should_quit = true;
                }
                KeymapResult::Action(action) => {
                    self.run_action(action).await;
                }
                KeymapResult::Pending(label) => {
                    self.chord_label = label;
                }
                KeymapResult::Unhandled => {}
            }
            return;
        }
        // The session-picker dialog is the topmost modal: while open it owns
        // every keystroke. This must sit above the program-popup gate below so
        // that a dialog opened from the program view's `@`→session path
        // captures input instead of leaking it into the program buffer.
        if self.session_picker_active() {
            self.handle_session_picker_key(key);
            return;
        }
        if self.tasks_popup.is_some() {
            if matches!(key.code, KeyCode::Esc) {
                self.tasks_popup = None;
                return;
            }
        }
        // The program captures keystrokes only while it is the topmost input
        // surface *and* the view pane holds focus. If a minibuffer/palette
        // overlay is open over it (e.g. `C-x x` opened the command palette or
        // the operator input), that overlay must capture input instead —
        // otherwise typed keys leak into the program buffer and the
        // palette/operator is unusable. The `C-x` chord that *opens* an overlay
        // still reaches the program global handler because no minibuffer exists
        // yet at that point; once the overlay is open the minibuffer block below
        // takes over.
        //
        // The `PaneFocus::View` gate is what lets `C-x o` hand control back to
        // the session list while the program stays visible in the view pane.
        // With focus on the list, Up/Down and `C-n`/`C-p` fall through to the
        // keymap and move the list selection instead of the program cursor.
        // Opening a program focuses the view (see `open_program_popup`), so the
        // common open-then-type flow is unchanged.
        if self
            .program_popup
            .as_ref()
            .is_some_and(|popup| !popup.terminal_focus)
            && self.minibuffer.is_none()
            && self.focus == PaneFocus::View
        {
            if self.handle_program_global_key(key).await {
                return;
            }
            self.handle_program_key(key).await;
            return;
        }
        // /remote-control modal: Esc closes the popup *and* the
        // orchestrator panel it was launched from, so a single Esc
        // returns the user to whichever session they had focused
        // before typing the slash. Without the orchestrator-close
        // step, the panel keeps routing every subsequent keystroke
        // to operator's PTY — the user reported "couldn't type prompt
        // from tui after enabling remote control" because of this.
        //
        // Non-Esc keys are *eaten* while the popup is visible — the
        // popup body is informational only (URL + QR), and falling
        // through to the underlying handler would silently route
        // typing into operator / a session under the modal.
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

        if self.handle_inline_dynamic_ui_key(key).await {
            return;
        }

        if self.layout.dynamic_ui_inline_hit.is_some() {
            return;
        }

        if self.try_dynamic_ui_action_key(key).await {
            return;
        }
        if self.try_dynamic_ui_scroll_key(key) {
            return;
        }

        if self.should_autofocus_view_from_list(key) {
            self.collapse_orchestrator_panel_on_focus_change();
            self.focus = PaneFocus::View;
        }

        // Emacs-style fast split-pane focus. Handled here — ahead of both the
        // PTY-capture forwarding and the chord dispatch below — so these keys
        // escape a focused child's PTY the same way `C-x o` does (it's a `C-x`
        // chord; these aren't, so they need an explicit intercept like the
        // PageUp/PageDown scrollback keys further down). Only fired when no
        // chord is in flight and the action actually has a target pane;
        // otherwise we fall through and the key keeps its normal meaning
        // (PTY input / list reorder / unbound).
        if self.chord_state.is_empty() {
            // `C-2`..`C-5` focus a pane directly (pane 1 = list, pane 2 = the
            // first split window, …). Terminals that don't deliver Ctrl+digit
            // (some legacy ones fold it onto Ctrl+@) simply never reach here.
            if key.modifiers == KeyModifiers::CONTROL {
                if let KeyCode::Char(c @ '2'..='5') = key.code {
                    let pane_index = c as usize - '1' as usize; // '2' -> 1 … '5' -> 4
                    if self.focus_pane_by_index(pane_index) {
                        self.chord_label.clear();
                        return;
                    }
                }
            }
            // `Shift+Arrow` moves focus to the spatially adjacent split window.
            // Scoped to an unzoomed, view-focused split layout so it only
            // shadows the child's / list's own Shift+Arrow when there's a real
            // multi-pane layout to navigate; a no-neighbor press is a no-op
            // (consumed) per the binding's contract.
            if key.modifiers == KeyModifiers::SHIFT
                && self.focus == PaneFocus::View
                && matches!(self.zoom, ZoomMode::None)
                && self.is_split_layout()
            {
                let dir = match key.code {
                    KeyCode::Up => Some(FocusDir::Up),
                    KeyCode::Down => Some(FocusDir::Down),
                    KeyCode::Left => Some(FocusDir::Left),
                    KeyCode::Right => Some(FocusDir::Right),
                    _ => None,
                };
                if let Some(dir) = dir {
                    self.focus_adjacent_window(dir);
                    self.chord_label.clear();
                    return;
                }
            }
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
            // PageUp/PageDown page the TUI scrollback even while the PTY is
            // captured (same effect as the `C-x [` / `C-x ]` chords) instead
            // of being forwarded to the child. Guarded to a plain press with
            // no chord in flight, so a mid-chord key still reaches the keymap.
            // Tradeoff: a full-screen program in the PTY (less, vim, man) no
            // longer receives bare PageUp/PageDown — scrollback wins.
            if self.chord_state.is_empty()
                && key.modifiers.is_empty()
                && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown)
            {
                let action = if matches!(key.code, KeyCode::PageUp) {
                    KeyAction::ScrollPageUp
                } else {
                    KeyAction::ScrollPageDown
                };
                self.run_action(action).await;
                return;
            }
            if self.chord_state.is_empty() && !is_ctrl_x {
                // Typing snaps the view back to live: it's confusing to
                // type "into the past" while reading scrollback.
                let active_window = Some(self.active_window_id);
                let was_scrolled = self.scrollback_for_window(active_window) != 0;
                self.set_scrollback_for_window(active_window, 0);
                if was_scrolled {
                    self.show_terminal_scrollbar();
                }
                if let Some(bytes) = encode_key_to_bytes(key) {
                    if let Some(id) = self.selected_id() {
                        self.queue_pty_input(id, bytes, "pty_input");
                    }
                }
                // The keystroke's visible effect arrives later as PTY
                // output, which triggers its own redraw. Painting now
                // just renders a stale frame — the dominant wasted
                // work when a key is held down (one render per repeat).
                // Skip it, unless we just snapped scrollback back to
                // live, which is a local display change with no output.
                if !was_scrolled {
                    self.skip_redraw_after_event = true;
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

    #[cfg(test)]
    fn ui_panels_for_test(
        &self,
        session_id: &str,
    ) -> Option<&HashMap<String, agentd_protocol::UiPanel>> {
        self.ui_panels.get(session_id)
    }

    fn global_action_while_inline(&mut self, key: KeyEvent) -> Option<KeyAction> {
        match self.chord_state.handle(key, &self.keymap) {
            KeymapResult::Action(
                action @ (KeyAction::NextSession
                | KeyAction::PrevSession
                | KeyAction::SwitchFocus
                | KeyAction::SplitWindowBelow
                | KeyAction::SplitWindowRight
                | KeyAction::DeleteWindow
                | KeyAction::DeleteOtherWindows
                | KeyAction::EnlargeWindow
                | KeyAction::EnlargeWindowHorizontally
                | KeyAction::ShrinkWindowHorizontally
                | KeyAction::FocusView
                | KeyAction::ToggleView
                | KeyAction::ToggleZoom
                | KeyAction::ToggleHelp
                | KeyAction::OpenProgram
                | KeyAction::ToggleProgramTerminalFocus
                | KeyAction::OpenCommandPalette
                | KeyAction::OpenSwitchSession
                | KeyAction::OpenNewSession
                | KeyAction::Refresh
                | KeyAction::Quit),
            ) => {
                self.chord_label.clear();
                Some(action)
            }
            KeymapResult::Action(_) => {
                self.chord_label.clear();
                None
            }
            KeymapResult::Pending(label) => {
                self.chord_label = label;
                None
            }
            KeymapResult::Unhandled => {
                self.chord_label.clear();
                None
            }
        }
    }

    fn should_autofocus_view_from_list(&self, key: KeyEvent) -> bool {
        // With a program visible in the view pane, the list-focused state is
        // purely for navigation: a stray letter must not auto-focus the view
        // (which would leak the keystroke into the shadowed PTY rather than the
        // program). Editing the program is an explicit `C-x o` away.
        if self.program_popup.is_some() {
            return false;
        }
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
                // `project` is a synthetic option that creates a project
                // instead of a session — surfaced in the same wizard for
                // discovery.
                names.push("project");
                let hint = names.join("|");
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("New [{hint}] (Tab completes): "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::NewSessionHarness,
                    error: None,
                });
            }
            OpenFork => {
                let Some(id) = self.selected_id() else {
                    self.set_status("fork: no session selected".to_string());
                    return;
                };
                if self.harnesses.is_empty() {
                    self.harnesses = self.client.harnesses().await.unwrap_or_default();
                }
                let names: Vec<&str> = self
                    .harnesses
                    .iter()
                    .filter(|h| h.available)
                    .map(|h| h.name.as_str())
                    .collect();
                let hint = names.join("|");
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("Fork → [{hint}] (Tab completes): "),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::ForkSessionHarness {
                        source_session_id: id,
                    },
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
                        prompt: "Rename project to: ".to_string(),
                        input: current,
                        cursor,
                        intent: MinibufferIntent::GroupRename { group_id: id },
                        error: None,
                    });
                }
                Selection::None => self.set_status("nothing selected".into()),
                // The "N archived" disclosure row has no name to rename.
                Selection::ArchivedRow(_) => {}
            },
            OpenDeleteConfirm => match self.selection.clone() {
                Selection::Session(id) => {
                    self.minibuffer = Some(Minibuffer {
                        prompt: format!(
                            "Session {}: [d/y] delete (drop transcript + worktree) / [a] archive (terminate, keep, hide) / [N] cancel: ",
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
                            "Delete project '{}'? (y = orphan members / type 'all' to delete sessions too / N = cancel): ",
                            name
                        ),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::GroupDeleteConfirm { group_id: id },
                        error: None,
                    });
                }
                Selection::None => {}
                // The "N archived" disclosure row cascade-deletes every
                // archived session it stands in for (each with the subagent
                // cascade applied daemon-side).
                Selection::ArchivedRow(section) => {
                    let ids = self.archived_sessions_in_section(&section);
                    if ids.is_empty() {
                        self.set_status("no archived sessions to delete".to_string());
                    } else {
                        let label = self.archive_section_label(&section);
                        self.minibuffer = Some(Minibuffer {
                            prompt: format!(
                                "Delete all {} archived session(s) in {}? (drops transcript + worktree) [y/N]: ",
                                ids.len(),
                                label
                            ),
                            input: String::new(),
                            cursor: 0,
                            intent: MinibufferIntent::ArchivedDeleteConfirm { section },
                            error: None,
                        });
                    }
                }
            },
            OpenProgram => {
                self.toggle_program_popup().await;
            }
            UndoProgram => {
                self.undo_program_edit();
            }
            SaveProgram => {
                self.save_program_popup().await;
            }
            RunProgram => {
                // Keyboard equivalent of the title-bar ▶ button and the
                // selection ▶ Run button: run just the highlighted selection
                // when one is active, otherwise run the whole program. No-op
                // (with a status hint) when no program surface is open.
                let selection = self
                    .program_popup
                    .as_ref()
                    .and_then(Self::selected_program_text);
                self.execute_program_popup(selection, None).await;
            }
            ToggleProgramTerminalFocus => {
                self.toggle_program_terminal_focus();
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
                self.open_minibuffer_for_command();
            }
            OpenSwitchSession => {
                self.open_session_picker(SessionPickerPurpose::Switch);
            }
            FocusView => {
                // Enter on an "N archived" disclosure row expands/collapses it
                // (it has no view to drill into).
                if let Some(section) = self.selection.archive_section().cloned() {
                    self.toggle_archive_section(&section);
                    return;
                }
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
                // zoomed (and focused). In normal layout it cycles
                // list plus all visible main windows.
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
                    ZoomMode::None => match self.focus {
                        PaneFocus::List => {
                            if let Some(id) = self.leaf_window_ids().first().copied() {
                                self.focus_main_window(id);
                            } else {
                                self.focus = PaneFocus::View;
                            }
                        }
                        PaneFocus::View => {
                            let ids = self.leaf_window_ids();
                            if let Some(pos) =
                                ids.iter().position(|id| *id == self.active_window_id)
                            {
                                if pos + 1 < ids.len() {
                                    self.focus_main_window(ids[pos + 1]);
                                } else {
                                    self.focus = PaneFocus::List;
                                }
                            } else {
                                self.focus = PaneFocus::List;
                            }
                        }
                    },
                }
                let label = match self.focus {
                    PaneFocus::List => "focus: list".to_string(),
                    PaneFocus::View => format!("focus: window {}", self.active_window_id),
                };
                self.set_status(label);
            }
            // `C-x <arrow>` — directional pane focus. The keyboard-reachable
            // counterpart to `Shift+<arrow>`, dispatched through the keymap so
            // it survives terminals that swallow `Shift+Up`/`Shift+Down` for
            // scrollback (where the bare `Shift+Arrow` intercept never fires).
            FocusWindowUp => self.focus_window_in_dir(FocusDir::Up),
            FocusWindowDown => self.focus_window_in_dir(FocusDir::Down),
            FocusWindowLeft => self.focus_window_in_dir(FocusDir::Left),
            FocusWindowRight => self.focus_window_in_dir(FocusDir::Right),
            SplitWindowBelow => self.split_active_window(WindowSplitDirection::Below),
            SplitWindowRight => self.split_active_window(WindowSplitDirection::Right),
            DeleteWindow => self.delete_active_window(),
            DeleteOtherWindows => self.delete_other_windows(),
            EnlargeWindow => self.resize_active_window(5, WindowSplitDirection::Below),
            EnlargeWindowHorizontally => self.resize_active_window(5, WindowSplitDirection::Right),
            ShrinkWindowHorizontally => self.resize_active_window(-5, WindowSplitDirection::Right),
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
                // Scope the toggle to the focused split only: `set_active_view`
                // records the new mode in `window_views` for the active window
                // so sibling panes keep their own transcript/terminal mode.
                let next = match (self.view, has_pty) {
                    (ViewMode::Chat, true) => {
                        // First time switching → bootstrap from replay snapshot.
                        if let Some(id) = self.selected_id() {
                            self.bootstrap_terminal(&id).await;
                        }
                        ViewMode::Terminal
                    }
                    _ => ViewMode::Chat,
                };
                self.set_active_view(next);
            }
            MoveSelectedUp => self.move_selected(true).await,
            MoveSelectedDown => self.move_selected(false).await,
            TogglePin => {
                self.toggle_pin_on_selection().await;
            }
            ExpandGroup => {
                if let Some(section) = self.selection.archive_section().cloned() {
                    self.set_archive_section_revealed(&section, true);
                } else if let Some(g) = self.selected_group() {
                    let id = g.id.clone();
                    if let Err(e) = self.client.set_project_collapsed(&id, false).await {
                        self.set_status(format!("expand failed: {e}"));
                    }
                } else if self.focus == PaneFocus::List {
                    if let Some(id) = self.selected_session_has_subagents() {
                        self.subagent_collapsed.remove(&id);
                    }
                }
            }
            CollapseGroup => {
                if let Some(section) = self.selection.archive_section().cloned() {
                    self.set_archive_section_revealed(&section, false);
                } else if let Some(g) = self.selected_group() {
                    let id = g.id.clone();
                    if let Err(e) = self.client.set_project_collapsed(&id, true).await {
                        self.set_status(format!("collapse failed: {e}"));
                    }
                } else if self.focus == PaneFocus::List {
                    if let Some(id) = self.selected_session_has_subagents() {
                        self.subagent_collapsed.insert(id);
                    }
                }
            }
            ScrollUp => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(1);
                } else if self.view == ViewMode::Chat {
                    self.adjust_chat_scroll(1);
                }
            }
            ScrollDown => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(-1);
                } else if self.view == ViewMode::Chat {
                    self.adjust_chat_scroll(-1);
                }
            }
            ScrollPageUp => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(10);
                } else if self.view == ViewMode::Chat {
                    self.adjust_chat_scroll(10);
                }
            }
            ScrollPageDown => {
                if self.can_scroll_pty_history() {
                    self.adjust_scrollback(-10);
                } else if self.view == ViewMode::Chat {
                    self.adjust_chat_scroll(-10);
                }
            }
            ScrollTop => {
                if self.can_scroll_pty_history() {
                    if self.is_orchestrator_panel_open() {
                        self.orchestrator_scrollback = SCROLLBACK_MAX;
                    } else {
                        let active_window = Some(self.active_window_id);
                        self.set_scrollback_for_window(active_window, SCROLLBACK_MAX);
                        self.show_terminal_scrollbar();
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
                        let active_window = Some(self.active_window_id);
                        self.set_scrollback_for_window(active_window, 0);
                        self.show_terminal_scrollbar();
                    }
                } else {
                    self.transcript_scroll = u16::MAX;
                }
            }
            ToggleHelp => {
                self.help_visible = !self.help_visible;
            }
            ToggleAutomode => {
                self.cycle_approval_mode().await;
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
            // Typing into operator snaps back to live output, matching the main
            // PTY pane's behavior.
            self.orchestrator_scrollback = 0;
            self.queue_pty_input(orch_id, bytes, "orchestrator pty_input");
        }
    }

    /// Execute a slash-style command (`zoom`, `new`, `quit`, ...) with
    /// no LLM involvement. Used both by the orchestrator panel (when
    /// input starts with `/`) and by the static palette (fallback when
    /// no orchestrator is present).
    pub(super) async fn run_slash_command(&mut self, cmd: &str) {
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
        let verb = verb.trim_start_matches('/');
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
            "fork" => self.run_action(KeyAction::OpenFork).await,
            "program" | "edit-program" => self.run_action(KeyAction::OpenProgram).await,
            "zoom" | "fullscreen" => self.run_action(KeyAction::ToggleZoom).await,
            "rain" | "matrix" | "matrix-rain" => {
                self.matrix_rain_hidden = !self.matrix_rain_hidden;
                self.set_status(format!(
                    "matrix rain {}",
                    if self.matrix_rain_hidden {
                        "collapsed"
                    } else {
                        "expanded"
                    }
                ));
            }
            "archived" | "archive" | "archives" => {
                self.toggle_archived_for_selection();
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
            "construct" => {
                // Subcommand dispatch:
                //
                //   /construct restart [binary path]
                //       → daemon.restart; exec the daemon's own binary,
                //         or the given one (e.g. a freshly-built worktree
                //         binary). The path is validated daemon-side.
                //
                // Other subcommands are reserved for future use
                // (e.g. `/construct info` to print build version). The
                // daemon.restart RPC will close the IPC connection
                // as the new process replaces the old; the TUI
                // observes that as a "daemon disconnected" status
                // and the user must re-run `construct` to reconnect
                // (auto-reconnect is follow-up work, see issue #90).
                let mut parts = arg.trim().splitn(2, char::is_whitespace);
                let sub = parts.next().unwrap_or("");
                let rest = parts.next().unwrap_or("").trim();
                match sub {
                    "restart" => {
                        let exe = (!rest.is_empty()).then(|| rest.to_string());
                        match self.client.daemon_restart(exe, false).await {
                            Ok(r) => self.set_status(format!(
                                "construct: restart requested (exe={}, pid={}) — reconnect when ready",
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
                                        "construct: restart in flight (socket closed) — reconnect when ready".to_string(),
                                    );
                                } else {
                                    self.set_status(format!("construct restart failed: {e}"));
                                }
                            }
                        }
                    }
                    "" => self.set_status("construct: subcommand required (e.g. `restart`)".into()),
                    other => self.set_status(format!(
                        "construct: unknown subcommand '{other}'; try `restart`"
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
    ui_panels: &mut HashMap<String, agentd_protocol::UiPanel>,
    is_headless: bool,
) {
    for ev in events {
        match &ev.event {
            // TaskStart is the PRIMARY block-creation event for
            // current smith sessions — it carries the explicit
            // `call_id` and the live `on_notification` handler
            // forwards it to `feed_task_start`. Without forwarding
            // it here too, a fresh TUI re-attaching to an existing
            // session sees no `ToolBlock` items in the replayed
            // history (the OSC 7700 backstop only fires for legacy
            // `pty.log` files; current smith doesn't write the
            // fences), `has_blocks` is false, and the user can no
            // longer see synthesized tool blocks at all — including
            // when scrolling. See
            // ` smith_tool_block_visible_after_bootstrap_via_task_start`.
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
            SessionEvent::ToolUse { tool, args, .. } => {
                // The TUI-dispatch tool (`tui`) is a slash-command
                // short-circuit, not a real tool block — skip it
                // just like the live notification handler does.
                if tool != agentd_protocol::TUI_DISPATCH_TOOL {
                    history.feed_tool_use(tool.clone(), summarize_tool_args(args));
                }
            }
            SessionEvent::ToolResult {
                tool,
                ok,
                output,
                call_id,
            } => {
                // Correlate by the explicit `call_id` when present; legacy
                // transcripts (call_id == None) still carry the id in the
                // `tool` field, so fall back to it.
                history.feed_tool_result(
                    call_id.as_deref().unwrap_or(tool),
                    *ok,
                    crate::pty_render::tool_output_preview_for_history(output),
                );
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
            // Browser previews are deliberately NOT reconstructed from
            // the transcript — they're ephemeral, live-only UI: the
            // overlay/wallpaper must never resurrect a stale thumbnail on
            // reconnect/restart or session switch.

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
            SessionEvent::UiPanel(panel) => {
                ui_panels.insert(panel.id.clone(), panel.clone());
            }
            SessionEvent::UiDelete { id } => {
                ui_panels.remove(id);
            }
            // Headless sessions carry their conversation as structured
            // Message / Reasoning events (no PTY), so fold the prose into
            // history to render it on reconnect. PTY-backed sessions keep
            // their prose in the PTY stream — skip to avoid double-render.
            SessionEvent::Message { role, text } if is_headless => {
                let kind = match role {
                    agentd_protocol::MessageRole::User => crate::pty_render::MessageKind::User,
                    _ => crate::pty_render::MessageKind::Assistant,
                };
                history.feed_message(kind, text);
            }
            SessionEvent::Reasoning { text } if is_headless => {
                history.feed_message(crate::pty_render::MessageKind::Reasoning, text);
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

fn session_switch_label(s: &SessionSummary) -> String {
    s.title
        .as_ref()
        .filter(|t| !t.trim().is_empty())
        .cloned()
        .unwrap_or_else(|| short_id(&s.id).to_string())
}

fn session_switch_haystack(s: &SessionSummary) -> String {
    format!(
        "{} {} {} {}",
        session_switch_label(s),
        s.id,
        short_id(&s.id),
        s.harness
    )
    .to_lowercase()
}

fn fuzzy_match(query: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|needle| chars.by_ref().any(|hay| hay == needle))
}

fn switch_session_match_score(s: &SessionSummary, query: &str) -> Option<i32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let label = session_switch_label(s).to_lowercase();
    let id = s.id.to_lowercase();
    let short = short_id(&s.id).to_lowercase();
    let harness = s.harness.to_lowercase();
    if label == q || id == q || short == q {
        Some(100)
    } else if label.starts_with(&q) {
        Some(90)
    } else if short.starts_with(&q) || id.starts_with(&q) {
        Some(85)
    } else if harness.starts_with(&q) {
        Some(75)
    } else if label.contains(&q) || id.contains(&q) || harness.contains(&q) {
        Some(65)
    } else if fuzzy_match(&q, &session_switch_haystack(s)) {
        Some(40)
    } else {
        None
    }
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
    // Reused per-row char buffer. The previous form indexed each cell with
    // `line.chars().nth(col)`, re-decoding the line from the start for every
    // column — O(width^2) UTF-8 decoding per row. Since this runs on every
    // frame the mouse hovers the view (hovered-URL hit test in `finish_frame`),
    // that quadratic dominated the render loop on wide terminals. Decode each
    // line once into `line_chars` and index it in O(1) instead.
    let mut line_chars: Vec<char> = Vec::new();
    for row in bounds.top()..bounds.bottom() {
        let Some(line) = frame_text.get(row as usize) else {
            continue;
        };
        line_chars.clear();
        line_chars.extend(line.chars());
        for col in bounds.left()..bounds.right() {
            let ch = line_chars.get(col as usize).copied().unwrap_or(' ');
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipboardCopyOutcome {
    Copied,
    Requested(Osc52Mode),
}

impl ClipboardCopyOutcome {
    fn status(self, chars: usize) -> String {
        match self {
            Self::Copied => format!("copied {chars} chars"),
            Self::Requested(mode) => format!(
                "sent copy request for {chars} chars via OSC 52 {}",
                mode.label()
            ),
        }
    }
}

fn copy_to_clipboard(text: &str) -> Result<ClipboardCopyOutcome> {
    // `pbcopy` writes to the clipboard of the host construct runs on; OSC 52
    // travels back through the controlling terminal to the clipboard of the
    // machine the user is actually sitting at. Over SSH those differ — pbcopy
    // would land on the remote host, and on a remote macOS host it *succeeds*
    // against the wrong pasteboard, so the OSC 52 fallback never fires and the
    // selection never reaches the user's clipboard. Prefer OSC 52 when we
    // detect a remote session; keep pbcopy first locally, where it's the
    // reliable path (e.g. Terminal.app honors no OSC 52).
    if is_remote_session() {
        let mode = osc52_mode();
        if copy_with_osc52_mode(text, mode).is_ok() {
            return Ok(ClipboardCopyOutcome::Requested(mode));
        }
        copy_with_pbcopy(text)?;
        return Ok(ClipboardCopyOutcome::Copied);
    }
    if copy_with_pbcopy(text).is_ok() {
        return Ok(ClipboardCopyOutcome::Copied);
    }
    let mode = osc52_mode();
    copy_with_osc52_mode(text, mode)?;
    Ok(ClipboardCopyOutcome::Requested(mode))
}

/// True when the process appears to be running inside an SSH session, where
/// local pasteboard tools (`pbcopy`) would target the remote host rather than
/// the machine the user is sitting at.
fn is_remote_session() -> bool {
    std::env::var_os("SSH_TTY").is_some_and(|v| !v.is_empty())
        || std::env::var_os("SSH_CONNECTION").is_some_and(|v| !v.is_empty())
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

fn read_from_clipboard() -> Result<String> {
    let output = Command::new("pbpaste").output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        anyhow::bail!("pbpaste exited with {}", output.status)
    }
}

/// Build the OSC 52 clipboard-write escape sequence for `text`: the terminal
/// that receives it copies the (base64-encoded) payload to the system
/// clipboard of the machine the terminal runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Osc52Mode {
    Direct,
    Tmux,
    Screen,
}

impl Osc52Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Tmux => "tmux",
            Self::Screen => "screen",
        }
    }
}

fn osc52_mode() -> Osc52Mode {
    osc52_mode_from_vars(
        std::env::var_os("CONSTRUCT_OSC52_MODE").as_deref(),
        std::env::var_os("TMUX").as_deref(),
        std::env::var_os("STY").as_deref(),
        std::env::var_os("TERM").as_deref(),
    )
}

fn osc52_mode_from_vars(
    override_mode: Option<&OsStr>,
    tmux: Option<&OsStr>,
    screen: Option<&OsStr>,
    term: Option<&OsStr>,
) -> Osc52Mode {
    match override_mode
        .and_then(|v| v.to_str())
        .map(|v| v.to_ascii_lowercase())
        .as_deref()
    {
        Some("tmux") => return Osc52Mode::Tmux,
        Some("screen") => return Osc52Mode::Screen,
        Some("direct") | Some("plain") => return Osc52Mode::Direct,
        _ => {}
    }
    if tmux.is_some_and(|v| !v.is_empty()) {
        Osc52Mode::Tmux
    } else if screen.is_some_and(|v| !v.is_empty()) {
        Osc52Mode::Screen
    } else if term
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.contains("tmux"))
    {
        Osc52Mode::Tmux
    } else if term
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.contains("screen"))
    {
        Osc52Mode::Screen
    } else {
        Osc52Mode::Direct
    }
}

fn osc52_direct_sequence(text: &str) -> String {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // Emit both standard OSC terminators. Most terminals accept BEL, but ST is
    // legal too and helps stricter terminal paths without changing the target
    // clipboard value.
    format!("\x1b]52;c;{encoded}\x07\x1b]52;c;{encoded}\x1b\\")
}

fn tmux_passthrough_sequence(sequence: &str) -> String {
    let escaped = sequence.replace('\x1b', "\x1b\x1b");
    format!("\x1bPtmux;{escaped}\x1b\\")
}

fn screen_passthrough_sequence(sequence: &str) -> String {
    format!("\x1bP{sequence}\x1b\\")
}

fn osc52_sequence_for_mode(text: &str, mode: Osc52Mode) -> String {
    let direct = osc52_direct_sequence(text);
    match mode {
        Osc52Mode::Direct => direct,
        // Emit both the multiplexer passthrough form and the plain OSC 52.
        // Different tmux/screen versions and configs accept different paths:
        // passthrough reaches the outer terminal when enabled, while the plain
        // sequence lets multiplexers with native OSC 52 clipboard support copy.
        Osc52Mode::Tmux => format!("{}{}", tmux_passthrough_sequence(&direct), direct),
        Osc52Mode::Screen => format!("{}{}", screen_passthrough_sequence(&direct), direct),
    }
}

fn copy_with_osc52_mode(text: &str, mode: Osc52Mode) -> Result<()> {
    let mut stdout = std::io::stdout();
    write!(stdout, "{}", osc52_sequence_for_mode(text, mode))?;
    stdout.flush()?;
    Ok(())
}

fn byte_pos(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn program_cursor_at_modal_point(
    app: Option<&App>,
    buffer: &str,
    modal: ratatui::layout::Rect,
    scroll_offset: usize,
    col: u16,
    row: u16,
) -> Option<usize> {
    let inner_x = modal
        .x
        .saturating_add(1)
        .saturating_add(PROGRAM_CONTENT_PADDING_X);
    let inner_y = modal
        .y
        .saturating_add(1)
        .saturating_add(PROGRAM_CONTENT_PADDING_Y);
    if row < inner_y {
        return None;
    }
    // Map the clicked cell to an absolute *visual* (word-wrapped) row/col: add
    // the scroll offset to reach the source row, then resolve it through the
    // renderer's wrap so a click on a wrapped continuation row lands on the right
    // offset instead of being treated as a whole logical line.
    let target_row = (row.saturating_sub(inner_y) as usize).saturating_add(scroll_offset);
    let target_col = col.saturating_sub(inner_x) as usize;
    let width = ui::program_modal_inner_width(modal);
    Some(program_normalize_program_cursor(
        buffer,
        ui::program_visual_to_cursor(app, buffer, target_row, target_col, width),
    ))
}

fn program_list_marker_content_start(raw_line: &str) -> Option<usize> {
    let trimmed = raw_line.trim();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        Some(raw_line.chars().take_while(|ch| ch.is_whitespace()).count() + 2)
    } else {
        None
    }
}

fn program_list_marker_cursor(buffer: &str, cursor: usize) -> Option<usize> {
    let mut line_start = 0usize;
    for (idx, ch) in buffer.chars().enumerate() {
        if idx >= cursor {
            break;
        }
        if ch == '\n' {
            line_start = idx + 1;
        }
    }
    let raw_line: String = buffer
        .chars()
        .skip(line_start)
        .take_while(|ch| *ch != '\n')
        .collect();
    program_list_marker_content_start(&raw_line).map(|offset| line_start + offset)
}

fn program_normalize_program_cursor(buffer: &str, cursor: usize) -> usize {
    program_list_marker_cursor(buffer, cursor)
        .map(|marker_start| marker_start.max(cursor))
        .unwrap_or(cursor)
}

fn program_smart_clip_query(popup: &ProgramPopup, trigger_start: usize) -> Option<String> {
    if popup.cursor <= trigger_start {
        return None;
    }
    let trigger = popup.buffer.chars().nth(trigger_start)?;
    if trigger != '@' {
        return None;
    }
    let mut query = String::new();
    for ch in popup
        .buffer
        .chars()
        .skip(trigger_start + 1)
        .take(popup.cursor.saturating_sub(trigger_start + 1))
    {
        if ch.is_whitespace() || matches!(ch, '{' | '}' | '[' | ']' | '(' | ')' | ',' | ';') {
            return None;
        }
        query.push(ch);
    }
    Some(query)
}

/// Relevance score of a non-empty, lowercased `query` against a candidate.
/// `label` is the primary field (session title / harness name); `haystack` is a
/// lowercased blob of every searchable field. Higher is better; `None` means no
/// match at all. Drives both the top relevance section (rank, take 5) and submenu
/// dimming (matched = `Some`).
pub(crate) fn program_clip_match_score(query: &str, label: &str, haystack: &str) -> Option<i32> {
    debug_assert!(!query.is_empty());
    let label = label.to_ascii_lowercase();
    if label == query {
        return Some(1000);
    }
    if label.starts_with(query) {
        return Some(900);
    }
    if label
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| !word.is_empty() && word.starts_with(query))
    {
        return Some(750);
    }
    if label.contains(query) {
        return Some(600);
    }
    if haystack.contains(query) {
        return Some(400);
    }
    // Loose fuzzy fallback, but only against the visible label — a subsequence
    // over the whole haystack matches almost anything for short queries (so it
    // would never dim a submenu item).
    if program_clip_is_subsequence(query, &label) {
        return Some(250);
    }
    None
}

/// Whether `needle`'s chars appear in `haystack` in order (gaps allowed) — the
/// loose fuzzy fallback used for ranking when no contiguous match exists.
fn program_clip_is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    'next: for nc in needle.chars() {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'next;
            }
        }
        return false;
    }
    true
}

fn program_search_matches(buffer: &str, query: &str) -> Vec<(usize, usize)> {
    if query.is_empty() {
        return Vec::new();
    }
    let query_chars = query.chars().count();
    let query_bytes = query.len();
    let mut out = Vec::new();
    let mut start_b = 0usize;
    while start_b <= buffer.len() {
        let Some(slice) = buffer.get(start_b..) else {
            break;
        };
        let Some(offset) = slice.find(query) else {
            break;
        };
        let match_start_b = start_b + offset;
        let match_end_b = match_start_b + query_bytes;
        let char_start = buffer[..match_start_b].chars().count();
        out.push((char_start, char_start + query_chars));
        start_b = match_end_b;
    }
    out
}

/// Add search matches for smart clips whose resolved label text contains `query`.
/// Each match spans the entire raw `@{...}` clip in the buffer so the chip gets
/// highlighted and the cursor navigates to it.  Clips that are already covered by
/// an existing raw-buffer match (e.g. the query happens to appear inside the clip
/// syntax itself) are skipped to avoid duplicate matches at overlapping ranges.
fn program_search_add_clip_label_matches(
    app: &App,
    buffer: &str,
    query: &str,
    matches: &mut Vec<(usize, usize)>,
) {
    let mut char_offset = 0usize;
    let mut byte_offset = 0usize;
    while byte_offset < buffer.len() {
        let rest = &buffer[byte_offset..];
        let Some(at_pos) = rest.find("@{") else { break };
        let before_bytes = &rest[..at_pos];
        let before_chars = before_bytes.chars().count();
        let clip_char_start = char_offset + before_chars;
        let after_marker = &rest[at_pos + 2..];
        let Some(end_pos) = after_marker.find('}') else {
            break;
        };
        let raw_clip = &after_marker[..end_pos];
        let raw_clip_chars = raw_clip.chars().count();
        let clip_char_end = clip_char_start + 2 + raw_clip_chars + 1;
        let already_covered = matches
            .iter()
            .any(|&(ms, me)| ms < clip_char_end && me > clip_char_start);
        if !already_covered {
            let (_, label) = crate::ui::program_smart_clip_label(Some(app), raw_clip);
            if label.contains(query) {
                matches.push((clip_char_start, clip_char_end));
            }
        }
        let full_clip_bytes = 2 + end_pos + 1;
        byte_offset += at_pos + full_clip_bytes;
        char_offset = clip_char_end;
    }
}

fn program_smart_clip_with_instance_id(clip: &str, buffer: &str) -> String {
    if program_smart_clip_instance_id(clip).is_some() {
        return clip.to_string();
    }
    let Some(body) = clip.strip_prefix("@{").and_then(|s| s.strip_suffix('}')) else {
        return clip.to_string();
    };
    format!(
        "@{{{} clip_id={}}}",
        body,
        program_next_smart_clip_id(buffer)
    )
}

/// Whether the popup holds edits the user actually made, as opposed to
/// differing from the last daemon sync only by smart-clip instance-id
/// normalization. Agent-written documents can carry clips without a
/// `clip_id=` (or with duplicate ids), so the normalized form of an untouched
/// buffer never equals the raw `saved_markdown`; comparing normalized to raw
/// would misread such a popup as dirty and silently skip every live agent
/// update until the program was hidden and reopened. Normalize both sides so
/// only real edits count (the web UI does the same — it stores and compares
/// normalized content).
fn program_popup_has_unsaved_edits(popup: &ProgramPopup) -> bool {
    program_normalize_smart_clip_instance_ids(&popup.buffer)
        != program_normalize_smart_clip_instance_ids(&popup.saved_markdown)
}

/// The char-offset span (in `old`/`new` coordinates respectively) where `old`
/// and `new` differ, via common-prefix/suffix trim — mirrors
/// `program_edit_overall_span` in crates/daemon/src/session.rs. Returns
/// `(prefix, old_end, new_end)`: chars before `prefix` are shared, chars from
/// `prefix` to `old_end` in `old` were replaced by chars from `prefix` to
/// `new_end` in `new`. `None` when `old == new` (no rebase needed).
fn program_document_diff_span(old: &str, new: &str) -> Option<(usize, usize, usize)> {
    if old == new {
        return None;
    }
    let old_chars: Vec<char> = old.chars().collect();
    let new_chars: Vec<char> = new.chars().collect();
    let mut prefix = 0usize;
    while prefix < old_chars.len()
        && prefix < new_chars.len()
        && old_chars[prefix] == new_chars[prefix]
    {
        prefix += 1;
    }
    let mut old_end = old_chars.len();
    let mut new_end = new_chars.len();
    while old_end > prefix
        && new_end > prefix
        && old_chars[old_end - 1] == new_chars[new_end - 1]
    {
        old_end -= 1;
        new_end -= 1;
    }
    Some((prefix, old_end, new_end))
}

/// Rebase a char offset in the old document through to the new document,
/// given the diff span from [`program_document_diff_span`]. Positions at or
/// before the changed span's start are unchanged; positions at or after the
/// old span's end shift by the length delta; positions inside the changed
/// span clamp to the new span's end.
fn program_rebase_position(pos: usize, span: (usize, usize, usize)) -> usize {
    let (prefix, old_end, new_end) = span;
    if pos <= prefix {
        pos
    } else if pos >= old_end {
        let delta = new_end as isize - old_end as isize;
        (pos as isize + delta).max(prefix as isize) as usize
    } else {
        new_end
    }
}

fn program_normalize_smart_clip_instance_ids(markdown: &str) -> String {
    let ranges = program_smart_clip_ranges(markdown);
    if ranges.is_empty() {
        return markdown.to_string();
    }

    let mut max = 0usize;
    for range in &ranges {
        let start_b = byte_pos(markdown, range.start + 2);
        let end_b = byte_pos(markdown, range.end.saturating_sub(1));
        if let Some(id) = program_smart_clip_instance_id(&markdown[start_b..end_b]) {
            if let Some(num) = id
                .strip_prefix("clip_")
                .and_then(|s| s.parse::<usize>().ok())
            {
                max = max.max(num);
            }
        }
    }

    let mut used = HashSet::new();
    let mut next = max + 1;
    let mut normalized = String::with_capacity(markdown.len());
    let mut last_b = 0usize;
    for range in ranges {
        let clip_start_b = byte_pos(markdown, range.start);
        let body_start_b = byte_pos(markdown, range.start + 2);
        let body_end_b = byte_pos(markdown, range.end.saturating_sub(1));
        let clip_end_b = byte_pos(markdown, range.end);
        normalized.push_str(&markdown[last_b..clip_start_b]);

        let body = &markdown[body_start_b..body_end_b];
        let existing = program_smart_clip_instance_id(body).map(str::to_string);
        if existing.as_ref().is_some_and(|id| used.insert(id.clone())) {
            normalized.push_str(&markdown[clip_start_b..clip_end_b]);
        } else {
            let id = loop {
                let candidate = format!("clip_{next}");
                next += 1;
                if used.insert(candidate.clone()) {
                    break candidate;
                }
            };
            let body_without_id = program_smart_clip_body_without_instance_id(body);
            normalized.push_str("@{");
            normalized.push_str(&body_without_id);
            if !body_without_id.is_empty() {
                normalized.push(' ');
            }
            normalized.push_str("clip_id=");
            normalized.push_str(&id);
            normalized.push('}');
        }
        last_b = clip_end_b;
    }
    normalized.push_str(&markdown[last_b..]);
    normalized
}

fn program_next_smart_clip_id(buffer: &str) -> String {
    let mut max = 0usize;
    for range in program_smart_clip_ranges(buffer) {
        let start_b = byte_pos(buffer, range.start + 2);
        let end_b = byte_pos(buffer, range.end.saturating_sub(1));
        if let Some(id) = program_smart_clip_instance_id(&buffer[start_b..end_b]) {
            if let Some(num) = id
                .strip_prefix("clip_")
                .and_then(|s| s.parse::<usize>().ok())
            {
                max = max.max(num);
            }
        }
    }
    format!("clip_{}", max + 1)
}

fn program_smart_clip_instance_id(raw_clip: &str) -> Option<&str> {
    raw_clip.split_whitespace().find_map(|part| {
        part.strip_prefix("clip_id=")
            .filter(|value| !value.is_empty())
    })
}

fn program_smart_clip_body_without_instance_id(raw_clip: &str) -> String {
    raw_clip
        .split_whitespace()
        .filter(|part| !part.starts_with("clip_id="))
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProgramSmartClipRange {
    start: usize,
    end: usize,
}

fn program_cursor_left(buffer: &str, cursor: usize) -> usize {
    let cursor = program_normalize_program_cursor(buffer, cursor);
    if let Some(marker_start) = program_list_marker_cursor(buffer, cursor) {
        if cursor <= marker_start {
            return marker_start;
        }
    }
    program_normalize_program_cursor(
        buffer,
        program_smart_clip_range_before_or_containing(buffer, cursor)
            .map(|range| range.start)
            .unwrap_or_else(|| cursor.saturating_sub(1)),
    )
}

fn program_cursor_right(buffer: &str, cursor: usize) -> usize {
    let len = buffer.chars().count();
    let cursor = program_normalize_program_cursor(buffer, cursor);
    program_normalize_program_cursor(
        buffer,
        program_smart_clip_range_at_or_containing(buffer, cursor)
            .map(|range| range.end)
            .unwrap_or_else(|| (cursor + 1).min(len)),
    )
}

fn program_smart_clip_range_at_or_containing(
    buffer: &str,
    cursor: usize,
) -> Option<ProgramSmartClipRange> {
    program_smart_clip_ranges(buffer)
        .into_iter()
        .find(|range| cursor >= range.start && cursor < range.end)
}

fn program_smart_clip_range_before_or_containing(
    buffer: &str,
    cursor: usize,
) -> Option<ProgramSmartClipRange> {
    program_smart_clip_ranges(buffer)
        .into_iter()
        .find(|range| cursor > range.start && cursor <= range.end)
}

fn program_smart_clip_ranges(buffer: &str) -> Vec<ProgramSmartClipRange> {
    let chars: Vec<char> = buffer.chars().collect();
    let mut ranges = Vec::new();
    let mut idx = 0usize;
    while idx + 1 < chars.len() {
        if chars[idx] != '@' || chars[idx + 1] != '{' {
            idx += 1;
            continue;
        }
        let mut end = idx + 2;
        while end < chars.len() && chars[end] != '}' {
            end += 1;
        }
        if end < chars.len() {
            ranges.push(ProgramSmartClipRange {
                start: idx,
                end: end + 1,
            });
            idx = end + 1;
        } else {
            idx += 2;
        }
    }
    ranges
}

fn program_popup_from_document(
    program: agentd_protocol::ProgramDocument,
    blocks: Vec<agentd_protocol::ProgramBlockView>,
    now: Instant,
) -> ProgramPopup {
    let markdown = program.markdown.clone();
    ProgramPopup {
        program,
        buffer: markdown.clone(),
        saved_markdown: markdown,
        blocks,
        undo_stack: Vec::new(),
        cursor: 0,
        preferred_col: None,
        selection: None,
        smart_clip: None,
        search: None,
        revealed_at: now,
        hide_after: now + Duration::from_secs(365 * 24 * 60 * 60),
        closing: false,
        scroll_offset: 0,
        cover_percent: PROGRAM_COVER_PERCENT_DEFAULT,
        terminal_focus: false,
        slide_from: 0.0,
        slide_changed_at: None,
    }
}

/// Resolve a char offset into the buffer to its `(line index, column)` where
/// column is the char offset within that line and `'\n'` counts as one char.
/// `lines` must be the buffer split on `'\n'` (so the count is preserved).
fn program_offset_to_line_col(lines: &[String], offset: usize) -> (usize, usize) {
    let mut consumed = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if offset <= consumed + len {
            return (i, offset - consumed);
        }
        consumed += len + 1; // + the trailing newline
    }
    let last = lines.len().saturating_sub(1);
    (
        last,
        lines.get(last).map(|l| l.chars().count()).unwrap_or(0),
    )
}

/// Inverse of [`program_offset_to_line_col`]: the char offset of `(line, col)`.
fn program_line_col_to_offset(lines: &[String], line: usize, col: usize) -> usize {
    let mut offset = 0usize;
    for l in lines.iter().take(line) {
        offset += l.chars().count() + 1;
    }
    offset + col
}

fn program_line_start(s: &str, cursor: usize) -> usize {
    let mut line_start = 0usize;
    for (idx, ch) in s.chars().enumerate() {
        if idx >= cursor {
            break;
        }
        if ch == '\n' {
            line_start = idx + 1;
        }
    }
    if let Some(marker_start) = program_list_marker_cursor(s, cursor) {
        if marker_start >= line_start {
            return marker_start;
        }
    }
    line_start
}

fn program_line_end(s: &str, cursor: usize) -> usize {
    for (idx, ch) in s.chars().enumerate().skip(cursor) {
        if ch == '\n' {
            return idx;
        }
    }
    s.chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    /// Spec 0065 agent presence: the local receipt clock must renew only when
    /// the daemon's `updated_at_ms` genuinely advances, never on a rebase
    /// that leaves it unchanged (GAP D — a rebase must not re-trigger the
    /// reveal).
    #[test]
    fn program_agent_reveal_receipt_update_bumps_on_new_stamp_holds_on_rebase() {
        let t0 = Instant::now();
        let first = program_agent_reveal_receipt_update(None, 1_000, t0);
        assert_eq!(
            first,
            (1_000, t0),
            "first sighting always records a receipt"
        );

        let t1 = t0 + Duration::from_millis(50);
        let rebased = program_agent_reveal_receipt_update(Some(&first), 1_000, t1);
        assert_eq!(
            rebased, first,
            "unchanged updated_at_ms (a rebase) must not renew the receipt"
        );

        let t2 = t1 + Duration::from_millis(50);
        let renewed = program_agent_reveal_receipt_update(Some(&rebased), 1_200, t2);
        assert_eq!(
            renewed,
            (1_200, t2),
            "an advanced updated_at_ms (a genuine new write) must renew the receipt"
        );
    }

    #[test]
    fn osc52_sequence_wraps_base64_in_clipboard_escape() {
        // ESC ] 52 ; c ; <base64> with both legal OSC terminators:
        // BEL and ST. base64("hi") == "aGk=".
        assert_eq!(
            osc52_sequence_for_mode("hi", Osc52Mode::Direct),
            "\x1b]52;c;aGk=\u{7}\x1b]52;c;aGk=\x1b\\"
        );
        assert_eq!(
            osc52_sequence_for_mode("", Osc52Mode::Direct),
            "\x1b]52;c;\u{7}\x1b]52;c;\x1b\\"
        );
    }

    #[test]
    fn osc52_sequence_wraps_for_terminal_multiplexers() {
        let direct = "\x1b]52;c;aGk=\u{7}\x1b]52;c;aGk=\x1b\\";
        assert_eq!(
            osc52_sequence_for_mode("hi", Osc52Mode::Tmux),
            format!("{}{}", tmux_passthrough_sequence(direct), direct)
        );
        assert_eq!(
            osc52_sequence_for_mode("hi", Osc52Mode::Screen),
            format!("{}{}", screen_passthrough_sequence(direct), direct)
        );
    }

    #[test]
    fn osc52_mode_prefers_tmux_then_screen() {
        assert_eq!(
            osc52_mode_from_vars(None, None, None, Some(OsStr::new("xterm-256color"))),
            Osc52Mode::Direct
        );
        assert_eq!(
            osc52_mode_from_vars(None, Some(OsStr::new("tmux")), None, None),
            Osc52Mode::Tmux
        );
        assert_eq!(
            osc52_mode_from_vars(None, None, Some(OsStr::new("screen")), None),
            Osc52Mode::Screen
        );
        assert_eq!(
            osc52_mode_from_vars(
                None,
                Some(OsStr::new("tmux")),
                Some(OsStr::new("screen")),
                None
            ),
            Osc52Mode::Tmux
        );
    }

    #[test]
    fn osc52_mode_uses_term_for_multiplexers_hidden_by_ssh() {
        assert_eq!(
            osc52_mode_from_vars(None, None, None, Some(OsStr::new("tmux-256color"))),
            Osc52Mode::Tmux
        );
        assert_eq!(
            osc52_mode_from_vars(None, None, None, Some(OsStr::new("screen-256color"))),
            Osc52Mode::Screen
        );
    }

    #[test]
    fn osc52_mode_can_be_forced_for_hidden_terminal_paths() {
        assert_eq!(
            osc52_mode_from_vars(
                Some(OsStr::new("tmux")),
                None,
                None,
                Some(OsStr::new("xterm-256color"))
            ),
            Osc52Mode::Tmux
        );
        assert_eq!(
            osc52_mode_from_vars(
                Some(OsStr::new("direct")),
                Some(OsStr::new("tmux")),
                None,
                Some(OsStr::new("tmux-256color"))
            ),
            Osc52Mode::Direct
        );
    }

    #[test]
    fn clipboard_copy_outcome_status_distinguishes_requests() {
        assert_eq!(ClipboardCopyOutcome::Copied.status(7), "copied 7 chars");
        assert_eq!(
            ClipboardCopyOutcome::Requested(Osc52Mode::Direct).status(7),
            "sent copy request for 7 chars via OSC 52 direct"
        );
        assert_eq!(
            ClipboardCopyOutcome::Requested(Osc52Mode::Tmux).status(7),
            "sent copy request for 7 chars via OSC 52 tmux"
        );
    }

    #[test]
    fn operator_monolog_text_filters_noise() {
        assert_eq!(operator_monolog_text(""), None);
        assert_eq!(operator_monolog_text("   "), None);
        assert_eq!(operator_monolog_text("noted"), None);
        assert_eq!(operator_monolog_text("  Noted.  "), None);
        assert_eq!(
            operator_monolog_text("  'run using smith' is waiting at the trust prompt.  ")
                .as_deref(),
            Some("'run using smith' is waiting at the trust prompt.")
        );
    }

    /// Regression guard for the input-priority optimization (#2).
    ///
    /// Under heavy background PTY output `notifications.recv()` is
    /// almost always ready, so an unbiased `select!` services that
    /// feed work about as often as input — adding keystroke→render
    /// latency in the focused session (the "cursor hangs then jumps
    /// 3-5 chars" report). The fix biases the loop toward input:
    /// `biased;` with the input arm polled before the notification
    /// arm.
    ///
    /// That benefit is a tokio scheduling property with no stable
    /// runtime probe — the latency delta lives in a narrow, noisy,
    /// hardware-dependent load window (see the `multi_session_latency`
    /// benchmark in crates/e2e, which reports it but can't gate on
    /// it). So assert the structural invariant that produces the
    /// behavior instead: drop `biased;` or reorder the arms and this
    /// fails.
    #[test]
    fn run_loop_select_biases_input_ahead_of_notifications() {
        let src = include_str!("app.rs");
        let select_at = src
            .find("tokio::select! {")
            .expect("run_loop should contain a tokio::select!");
        // Bound the search to the run_loop region so the arm strings
        // quoted in this very test (far below) can't be matched.
        let body = &src[select_at..(select_at + 8000).min(src.len())];
        let biased = body
            .find("biased;")
            .expect("the event-loop select! must be `biased;` so a ready keystroke wins ties");
        let input = body.find("ev = input_stream.next()").expect("input arm");
        let notif = body
            .find("notif = notifications.recv()")
            .expect("notification arm");
        assert!(biased < input, "`biased;` must precede the input arm");
        assert!(
            input < notif,
            "input arm must be polled before the notification-drain arm \
             (bias toward input under background feed load)"
        );
    }

    /// Regression guard for frame starvation under many active sessions.
    ///
    /// Coalescing daemon notifications is useful: one codex/claude terminal
    /// redraw can arrive as several PTY chunks, and drawing after every chunk
    /// looks like a replay cascade. A count-only drain, though, can spend a
    /// whole animation frame budget feeding background PTY bytes before the
    /// loop paints again. The drain therefore has both a count cap and a small
    /// elapsed-time budget.
    #[test]
    fn notification_drain_stops_on_count_or_time_budget() {
        let start = Instant::now();
        assert!(should_continue_notification_drain(0, start, start));
        assert!(should_continue_notification_drain(
            MAX_NOTIFICATION_DRAIN - 1,
            start,
            start + NOTIFICATION_DRAIN_BUDGET / 2
        ));
        assert!(!should_continue_notification_drain(
            MAX_NOTIFICATION_DRAIN,
            start,
            start
        ));
        assert!(!should_continue_notification_drain(
            1,
            start,
            start + NOTIFICATION_DRAIN_BUDGET
        ));
    }

    #[test]
    fn run_loop_notification_drain_uses_time_budget() {
        let src = include_str!("app.rs");
        let recv_at = src
            .find("notif = notifications.recv()")
            .expect("notification arm");
        let body = &src[recv_at..(recv_at + 2500).min(src.len())];
        assert!(
            body.contains("should_continue_notification_drain"),
            "notification drain must check both count and elapsed time"
        );
        assert!(
            body.contains("drain_started"),
            "notification drain must measure elapsed time from the batch start"
        );
    }

    #[test]
    fn pty_skip_redraw_path_skips_housekeeping_before_select() {
        let src = include_str!("app.rs");
        let skip_at = src
            .find("let skip_draw = std::mem::take(&mut app.skip_redraw_after_event);")
            .expect("run_loop should consume skip_redraw_after_event");
        let select_at = src[skip_at..]
            .find("tokio::select! {")
            .map(|idx| skip_at + idx)
            .expect("run_loop should select after draw/maintenance");
        let body = &src[skip_at..select_at];
        let guard_at = body
            .find("if !skip_draw {")
            .expect("maintenance must be gated by skip_draw");
        let hydration_at = body
            .find("app.selected_needs_hydration()")
            .expect("selected hydration maintenance should remain in run_loop");
        let resize_at = body
            .find("app.active_pane_size()")
            .expect("resize maintenance should remain in run_loop");
        assert!(
            guard_at < hydration_at && guard_at < resize_at,
            "PTY passthrough skip-redraw should return to select! before \
             hydration/resize housekeeping can delay PTY echo handling"
        );
    }

    fn test_layout() -> LayoutSnapshot {
        LayoutSnapshot {
            list_area: Some(Rect::new(0, 0, 20, 10)),
            view_area: Some(Rect::new(20, 0, 80, 20)),
            main_window_areas: vec![WindowPaneHit {
                id: 1,
                area: Rect::new(20, 0, 80, 20),
                inner_area: Rect::new(21, 1, 78, 18),
            }],
            main_window_dividers: Vec::new(),
            pin_strip_area: Some(Rect::new(20, 20, 80, 8)),
            matrix_rain_area: None,
            minibuffer_area: Some(Rect::new(0, 29, 100, 4)),
            last_chat_areas: std::collections::HashMap::new(),
            modeline_approval_mode_hit: None,
            list_row_count: 0,
            list_items_area: None,
            list_scroll_offset: 0,
            shortcut_hints: Vec::new(),
            minibuffer_harness_hits: Vec::new(),
            modal_area: None,
            program_title_run_hit: None,
            program_title_toggle_hit: None,
            program_title_close_hit: None,
            program_selection_run_hit: None,
            program_inner_area: None,
            program_base_area: None,
            program_resize_hit: None,
            program_smart_clip_anchor: None,
            program_clip_hits: Vec::new(),
            program_template_hits: Vec::new(),
            browser_preview_area: None,
            browser_preview_close: None,
            terminal_scrollbar: None,
            dynamic_ui_action_hits: Vec::new(),
            dynamic_ui_url_hits: Vec::new(),
            dynamic_ui_widget_hits: Vec::new(),
            dynamic_ui_panel_close_hits: Vec::new(),
            dynamic_ui_inline_hit: None,
            matrix_operator_loop_hit: None,
            matrix_operator_title_hit: None,
            matrix_widget_hits: Vec::new(),
            dynamic_ui_trigger: None,
            dynamic_ui_triggers: Vec::new(),
            dynamic_ui_popover_area: None,
            dynamic_ui_dropdown_area: None,
            dynamic_ui_scroll_metrics: None,
        }
    }

    fn test_app(client: Arc<Client>, sessions: Vec<SessionSummary>) -> App {
        let now = Instant::now();
        let (pty_input_tx, pty_input_errors) = spawn_pty_input_pump(client.clone());
        App {
            client,
            last_reported_view: None,
            sessions,
            groups: Vec::new(),
            selection: Selection::Session("s1".into()),
            focus: PaneFocus::View,
            main_windows: MainWindowTree::single(1, Selection::Session("s1".into())),
            active_window_id: 1,
            next_window_id: 2,
            subagent_collapsed: HashSet::new(),
            transcript: Vec::new(),
            transcript_session: None,
            transcript_scroll: 0,
            minibuffer: None,
            harnesses: Vec::new(),
            program_templates: Vec::new(),
            program_templates_tx: mpsc::unbounded_channel().0,
            theme: crate::theme::Theme::default(),
            help_visible: false,
            profile: Profile::Emacs,
            keymap: keymap::default_for(Profile::Emacs),
            chord_state: ChordState::default(),
            chord_label: String::new(),
            status: None,
            update_notice: None,
            last_diff: None,
            should_quit: false,
            connected: true,
            remote_clients: 0,
            view: ViewMode::Terminal,
            histories: HashMap::new(),
            terminal_replayed_sessions_this_frame: HashSet::new(),
            block_hits: HashMap::new(),
            matrix_reveal_hits: Vec::new(),
            orchestrator_desired_size: None,
            terminal_pane_size: (80, 24),
            window_pane_sizes: HashMap::new(),
            zoom: ZoomMode::None,
            list_scroll_offset: 0,
            view_scrollback: 0,
            window_scrollback: HashMap::new(),
            window_views: HashMap::new(),
            terminal_scrollbar_visible_until: HashMap::new(),
            skip_redraw_after_event: false,
            notification_dirtied_view: true,
            hydrating_sessions: HashSet::new(),
            orchestrator_scrollback: 0,
            operator_monolog: None,
            operator_utterance: String::new(),
            orchestrator_panel_h: None,
            resizing_orchestrator_panel: None,
            dragging_terminal_scrollbar: None,
            pty_activity: HashMap::new(),
            start_instant: now,
            layout: LayoutSnapshot::default(),
            session_title_menu: None,
            mouse_pos: None,
            mouse_capture_enabled: true,
            orchestrator_id: None,
            list_panel_w: LIST_PANEL_W_DEFAULT,
            resizing_list: None,
            pin_strip_h: None,
            resizing_pin_strip: None,
            matrix_rain_h: None,
            resizing_matrix_rain: None,
            resizing_main_window: None,
            resizing_program_popup: None,
            list_collapsed: false,
            tasks_popup: None,
            session_picker: None,
            program_popup: None,
            program_popups: HashMap::new(),
            program_view_memory: HashMap::new(),
            program_runs: HashMap::new(),
            program_run_dispatch: HashMap::new(),
            program_settle_flourishes: HashMap::new(),
            program_collaborators: HashMap::new(),
            program_agent_reveal_receipts: HashMap::new(),
            own_program_client_id: None,
            program_clipboard: None,
            remote_control_popup: None,
            remote_control_task: None,
            editor_states: HashMap::new(),
            agent_statuses: HashMap::new(),
            pending_tool_approvals: HashMap::new(),
            browser_previews: HashMap::new(),
            ui_panels: HashMap::new(),
            dynamic_ui_popover_open: None,
            dynamic_ui_selected: HashSet::new(),
            dynamic_ui_temporary_until: HashMap::new(),
            dynamic_ui_hover: None,
            dynamic_ui_focused: None,
            dynamic_ui_scroll_offsets: HashMap::new(),
            image_resize_cache: Vec::new(),
            session_transitions: HashMap::new(),
            pin_transitions: HashMap::new(),
            matrix_rain: crate::matrix_rain::MatrixRain::default(),
            matrix_rain_intensity: 0.0,
            matrix_rain_intensity_updated_at: now,
            matrix_rain_foreground_epoch: now,
            matrix_rain_active_drops: HashMap::new(),
            matrix_widget_pinned: None,
            matrix_widget_hover: None,
            matrix_rain_hidden: false,
            show_archived_ungrouped: false,
            show_archived_groups: HashSet::new(),
            show_archived_subagents: HashSet::new(),
            hide_pane_side_borders: true,
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
            parent_session_id: None,
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
        }
    }

    fn program_popup_for_test(session_id: &str, markdown: &str, cursor: usize) -> ProgramPopup {
        let now = Instant::now();
        ProgramPopup {
            program: agentd_protocol::ProgramDocument {
                session_id: session_id.to_string(),
                markdown: markdown.to_string(),
                version: 1,
                updated_at_ms: 0,
                template_id: None,
            },
            buffer: markdown.to_string(),
            saved_markdown: markdown.to_string(),
            blocks: Vec::new(),
            undo_stack: Vec::new(),
            cursor,
            preferred_col: None,
            selection: None,
            smart_clip: None,
            search: None,
            revealed_at: now,
            hide_after: now + Duration::from_secs(60),
            closing: false,
            scroll_offset: 0,
            cover_percent: PROGRAM_COVER_PERCENT_DEFAULT,
            terminal_focus: false,
            slide_from: 0.0,
            slide_changed_at: None,
        }
    }

    /// Mock daemon for tests exercising Program open/edit/save end to end:
    /// replies to `program.get` / `program.update` with the session id
    /// echoed from params, and to `session.transcript` with an empty
    /// transcript. Everything else gets a `null` result (fine for
    /// fire-and-forget calls whose errors are swallowed by the caller).
    async fn program_flow_mock_daemon(
    ) -> (Arc<Client>, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
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
                        let req: Value = serde_json::from_str(&line).expect("json request");
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let params = req.get("params").cloned().unwrap_or(Value::Null);
                        let session_id = params
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or("s1")
                            .to_string();
                        let result = match method.as_str() {
                            ipc_method::PROGRAM_GET => {
                                let markdown = params
                                    .get("markdown")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                serde_json::json!({ "program": {
                                    "session_id": session_id,
                                    "markdown": markdown,
                                    "version": 1,
                                    "updated_at_ms": 0,
                                    "template_id": null,
                                }})
                            }
                            ipc_method::PROGRAM_UPDATE => {
                                let markdown = params
                                    .get("markdown")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                serde_json::json!({ "program": {
                                    "session_id": session_id,
                                    "markdown": markdown,
                                    "version": 2,
                                    "updated_at_ms": 0,
                                    "template_id": null,
                                }})
                            }
                            ipc_method::SESSION_TRANSCRIPT => {
                                serde_json::json!({ "events": [], "total": 0 })
                            }
                            _ => Value::Null,
                        };
                        let resp =
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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
        let client = Client::connect(&sock).await.expect("client connects");
        (client, dir, server)
    }

    // End-to-end reproduction of the reported repro: open Program on a
    // session, edit and save; split the window onto a second session listed
    // just above it; delete that second session so `focus_neighbor_of`
    // reassigns its pane to the neighbor — both panes now show the *first*
    // session, which still has a Program open. Drives the real
    // `open_program_popup` / `select_session` / `split_active_window` /
    // `on_session_deleted` methods (not hand-built App state) and checks
    // Program-popup bookkeeping stays consistent through the convergence:
    // no duplicate stashed entry, and the popup is restored to the session
    // both panes now share. Render-cost characteristics of this exact
    // shape are covered by
    // `two_split_panes_same_session_reuses_cached_size_instead_of_thrashing`.
    #[tokio::test]
    async fn two_windows_converging_on_one_session_with_program_open() {
        let (client, _dir, _server) = program_flow_mock_daemon().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.created_at = chrono::Utc::now() - chrono::Duration::seconds(10);
        let mut app = test_app(client, vec![s1]);
        app.selection = Selection::Session("s1".into());
        app.active_window_id = 1;
        app.main_windows = MainWindowTree::single(1, Selection::Session("s1".into()));

        // 1. Open Program on s1, edit, and save.
        app.open_program_popup().await;
        assert!(app.program_popup.is_some(), "program should have opened");
        app.program_popup.as_mut().unwrap().buffer = "# Notes\n\nedited body\n".to_string();
        assert!(app.save_program_popup().await, "save should succeed");
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            app.program_popup.as_ref().unwrap().saved_markdown,
            "buffer should be clean (not dirty) after save"
        );

        // 2. Create session s2, positioned above s1 in the list (newer
        // created_at, matching the reported repro's list ordering), then
        // split and switch the new window to show it.
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.created_at = chrono::Utc::now();
        app.sessions.push(s2);
        assert_eq!(
            app.list_items()
                .iter()
                .filter_map(|it| match it {
                    ListItem::Session { summary, .. } => Some(summary.id.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["s2".to_string(), "s1".to_string()],
            "s2 must sort above s1 in the list for this repro"
        );
        app.split_active_window(WindowSplitDirection::Right);
        app.select_session("s2".into());
        app.sync_active_window_selection();
        assert_eq!(app.main_windows.visible_session_ids(), vec!["s1", "s2"]);
        // s1's program is now stashed (focus moved to s2's window).
        assert!(app.program_popup.is_none());
        assert!(app.program_popups.contains_key("s1"));

        // 3. Delete s2. Its window's selection is reassigned to its list
        // neighbor (s1) — both windows now show s1.
        app.on_session_deleted("s2").await;
        assert_eq!(
            app.main_windows.visible_session_ids(),
            vec!["s1", "s1"],
            "both windows should now show s1"
        );

        // Render a few frames to drive the post-convergence bookkeeping
        // (`sync_program_popup_with_selection`) to steady state.
        let backend = ratatui::backend::TestBackend::new(160, 45);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        for _ in 0..3 {
            term.draw(|f| crate::ui::render(f, &mut app)).expect("draw");
        }
        assert!(
            app.program_popup.is_some(),
            "program_popup should have been restored to s1 after convergence"
        );
        assert!(
            app.program_popups.is_empty(),
            "program_popups should not retain a duplicate stashed entry for s1"
        );
    }

    // Regression: deleting a session must drop its Program-related state, not
    // just its terminal/history state. `program_popups` in particular is
    // scanned in full (sorted, cloned ids) by `open_program_session_ids()`
    // once per visible split pane on every frame — left uncleaned, opening
    // and deleting sessions with a Program view over a long-running TUI
    // session grows that map without bound, and the per-frame cost grows
    // with it, multiplied by however many split panes are open. That matches
    // reports of the TUI getting laggy after "delete a session from a split,
    // then create a new split": more splits pay the growing cost more times
    // per frame.
    #[tokio::test]
    async fn deleting_a_session_clears_its_program_state() {
        let (mut app, _dir, _server) = two_session_app().await;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        // s1's program is the active popup; s2's is stashed (not focused).
        app.program_popup = Some(program_popup_for_test("s1", "# Rule\n\nbody\n", 0));
        app.program_popups.insert(
            "s2".into(),
            program_popup_for_test("s2", "# Rule\n\nother\n", 0),
        );
        app.program_runs.insert(
            "s1".into(),
            ProgramRun {
                started_at: Instant::now(),
                pending: ["s1-block".to_string()].into_iter().collect(),
                pending_tooltips: HashMap::new(),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(30),
                first_output_seen: false,
                stage: agentd_protocol::ProgramRunStage::Delivered,
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 1,
            },
        );
        app.program_view_memory.insert(
            "s1".into(),
            ProgramViewMemory {
                cursor: 3,
                preferred_col: None,
                scroll_offset: 0,
                cover_percent: PROGRAM_COVER_PERCENT_DEFAULT,
            },
        );
        app.program_collaborators.insert(
            "client-1".into(),
            agentd_protocol::ProgramCursor {
                client_id: "client-1".into(),
                session_id: "s1".into(),
                label: "Web".into(),
                kind: "web".into(),
                cursor: 0,
                selection_anchor: None,
                selection_head: None,
                version: None,
                color_index: 0,
                updated_at_ms: 0,
                active: true,
            },
        );

        app.on_session_deleted("s1").await;

        assert!(
            app.program_popup.is_none(),
            "the active popup for the deleted session must not linger"
        );
        assert!(!app.program_popups.contains_key("s1"));
        assert!(!app.program_runs.contains_key("s1"));
        assert!(!app.program_view_memory.contains_key("s1"));
        assert!(
            !app.program_collaborators
                .values()
                .any(|c| c.session_id == "s1"),
            "collaborator cursors for the deleted session must be dropped"
        );
        // s2's program state is untouched — only s1's is gone.
        assert!(app.program_popups.contains_key("s2"));
    }

    // Concrete reproduction reported after the above fix shipped: a session
    // reassignment (the neighbor a deleted/archived session's window falls
    // back to — see `focus_neighbor_of`) can leave TWO split panes showing
    // the SAME session. `ItemHistory` caches one parser sized to whichever
    // width it was last replayed at; two panes at two different widths
    // alternate that width every single frame. For a plain PTY session
    // that's a bounded-tail rebuild (cheap); for a tool-block session
    // (smith/claude/codex — anything with a `ToolBlock` item) `replay_full`
    // takes the `rebuild_from = Some(0)` path on any cols change, replaying
    // every item from scratch — measured non-linear, multiple *seconds* for
    // just a couple thousand accumulated lines when cols alternate every
    // call, vs. instant when cols stay stable. Same failure family as
    // `pin_tile_reuses_cached_size_to_avoid_split_thrash`
    // (crates/cli/src/pty_render.rs), which already guards the
    // split+pin-strip case — just never guarded two ordinary split panes.
    // Regression for the fix: `render_terminal_for_window` now reuses the
    // first pane's cached size for any later pane rendering an
    // already-replayed-this-frame session, so cols never actually alternate.
    #[tokio::test]
    async fn two_split_panes_same_session_reuses_cached_size_instead_of_thrashing() {
        let mut history = crate::pty_render::ItemHistory::new();
        // A tool block forces the `replay_full` path (see `needs_synth` in
        // `ItemHistory::replay`) — the one that fully rebuilds on any cols
        // change, not just the bounded-tail `replay_cached` path plain PTY
        // sessions use.
        history.feed_tool_use("shell".into(), "ls".into());
        history.feed_pty(b"\x1b]7700;open;call=cX\x07x\x1b]7700;close;call=cX\x07");
        history.feed_tool_result("cX", true, "done".into());
        for i in 0..2_000u32 {
            history
                .feed_pty(format!("\x1b[33mline {i} of accumulated chat \x1b[0m\r\n").as_bytes());
        }
        let (mut app, _dir, _server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.has_pty = true;
        app.sessions = vec![s1];
        app.histories.insert("s1".into(), history);
        // Deliberately unequal split so the two panes compute different
        // widths — the thrash trigger. `render_main_windows` renders `first`
        // (window 1) then `second` (window 2) every frame, so window 1 is
        // always the "first replay this frame" and window 2 the repeat.
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 30,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());

        let backend = ratatui::backend::TestBackend::new(160, 45);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        // Warm up: first frame legitimately pays for at least one parser
        // build per distinct width.
        term.draw(|f| crate::ui::render(f, &mut app)).expect("draw");

        let start = Instant::now();
        for _ in 0..30 {
            term.draw(|f| crate::ui::render(f, &mut app)).expect("draw");
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed.as_millis() < 1_000,
            "30 frames of two split panes on one tool-block session took \
             {elapsed:?} — thrash reintroduced? (should reuse the first \
             pane's cached parser size instead of rebuilding per pane). \
             Without the fix this took multiple seconds even for a couple \
             thousand accumulated lines."
        );
    }

    #[tokio::test]
    async fn program_scroll_offset_follows_cursor_down_and_back() {
        let (mut app, _dir, _server) = empty_app().await;
        // Twenty short, non-wrapping lines rendered into a 5-row-tall viewport.
        let markdown = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.program_popup = Some(program_popup_for_test("s1", &markdown, 0));
        app.layout.program_inner_area = Some(ratatui::layout::Rect::new(0, 0, 40, 5));

        // (3) Short content / cursor near the top keeps the offset pinned to 0.
        app.follow_program_scroll();
        assert_eq!(app.program_popup.as_ref().unwrap().scroll_offset, 0);

        // (1) Cursor jumps to the final line: the window scrolls so the cursor
        // (visual row 19) stays inside the 5-row viewport.
        app.program_popup.as_mut().unwrap().cursor = markdown.chars().count();
        app.follow_program_scroll();
        let offset = app.program_popup.as_ref().unwrap().scroll_offset;
        assert!(
            offset > 0,
            "offset should advance below the fold, got {offset}"
        );
        assert!(
            (offset..offset + 5).contains(&19),
            "cursor row 19 must be visible within [{offset}, {})",
            offset + 5
        );

        // (2) Cursor returns to the top: the window snaps back to offset 0.
        app.program_popup.as_mut().unwrap().cursor = 0;
        app.follow_program_scroll();
        assert_eq!(app.program_popup.as_ref().unwrap().scroll_offset, 0);
    }

    #[tokio::test]
    async fn program_hide_then_show_restores_cursor_and_scroll() {
        let (mut app, _dir, server) = empty_app().await;
        let markdown = (0..20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");

        // Active program with the user parked partway down: a non-zero caret,
        // a remembered preferred column, and a freely-scrolled viewport.
        let mut popup = program_popup_for_test("s1", &markdown, 42);
        popup.scroll_offset = 7;
        popup.preferred_col = Some(3);
        app.program_popup = Some(popup);

        // Hide: capture the position, then drop the popup the way the close
        // animation eventually does.
        app.remember_program_view_state();
        app.program_popup = None;
        assert!(
            app.program_view_memory.contains_key("s1"),
            "hiding must remember the program's caret + scroll"
        );

        // Show: a fresh popup is rebuilt from the daemon document (caret 0,
        // scroll 0); restore must put the user back where they were.
        let mut reopened = program_popup_for_test("s1", &markdown, 0);
        assert_eq!(reopened.cursor, 0);
        assert_eq!(reopened.scroll_offset, 0);
        app.restore_program_view_state(&mut reopened);
        assert_eq!(reopened.cursor, 42, "caret survives hide→show");
        assert_eq!(reopened.scroll_offset, 7, "scroll survives hide→show");
        assert_eq!(reopened.preferred_col, Some(3), "preferred column survives");
        assert!(
            !app.program_view_memory.contains_key("s1"),
            "restoring consumes the remembered position"
        );

        server.abort();
    }

    #[tokio::test]
    async fn program_show_clamps_restored_cursor_to_shrunk_buffer() {
        let (mut app, _dir, server) = empty_app().await;

        // Hidden with the caret deep in a long document.
        let mut popup = program_popup_for_test("s1", "a very long original line", 24);
        popup.scroll_offset = 4;
        app.program_popup = Some(popup);
        app.remember_program_view_state();
        app.program_popup = None;

        // The document shrank on the daemon while hidden; the restored caret is
        // clamped to the new buffer rather than pointing past its end.
        let mut reopened = program_popup_for_test("s1", "short", 0);
        app.restore_program_view_state(&mut reopened);
        assert_eq!(reopened.cursor, "short".chars().count());

        server.abort();
    }

    #[tokio::test]
    async fn program_clip_click_resolves_and_focuses_session() {
        let (mut app, _dir, _server) = empty_app().await;
        let s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());

        // Pretend the last program render captured a clip for s2 at row 3, cols 4..16.
        app.layout.program_clip_hits = vec![ProgramClipHit {
            col_start: 4,
            col_end: 16,
            row: 3,
            session_id: "s2".into(),
        }];

        // A cell inside the chip resolves to its session; outside it does not.
        assert_eq!(app.program_clip_session_at(10, 3), Some("s2".to_string()));
        assert_eq!(app.program_clip_session_at(2, 3), None);
        assert_eq!(app.program_clip_session_at(10, 4), None);

        // Resolving + focusing (what the click handler does) selects that session.
        let target = app.program_clip_session_at(10, 3).expect("clip resolves");
        app.focus = PaneFocus::List;
        app.select_session(target);
        assert_eq!(app.selection.session_id(), Some("s2"));
    }

    #[test]
    fn program_template_hit_contains_spans_box_rows() {
        let hit = ProgramTemplateHit {
            col_start: 4,
            col_end: 18,
            row_start: 3,
            row_end: 5,
            template_id: "tasks".into(),
            markdown: "# Todo\n".into(),
        };
        // Inside the box (any of its three rows) hits; the edges and outside miss.
        assert!(hit.contains(4, 3));
        assert!(hit.contains(10, 4));
        assert!(hit.contains(17, 5));
        assert!(!hit.contains(18, 4)); // col_end is exclusive
        assert!(!hit.contains(10, 6)); // below the box
        assert!(!hit.contains(3, 4)); // left of the box
    }

    #[tokio::test]
    async fn program_template_button_fills_empty_buffer() {
        let (mut app, _dir, _server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "", 0));

        app.apply_program_template("tasks".into(), "# Todo\n\n# Done\n".into());

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "# Todo\n\n# Done\n");
        // Cursor lands at the end of the inserted template.
        assert_eq!(popup.cursor, "# Todo\n\n# Done\n".chars().count());
        // The template id is stamped onto the document for persistence.
        assert_eq!(popup.program.template_id.as_deref(), Some("tasks"));
        // The prior (empty) state is recorded so the fill can be undone.
        assert_eq!(popup.undo_stack.len(), 1);

        app.undo_program_edit();
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "");
    }

    /// Clicking a clip that points at a session with no navigable list row —
    /// the canonical case being a subagent, which renders only as a child of
    /// its parent (and never at all when the parent is the hidden
    /// orchestrator) — must switch to it *persistently*. Two prior bugs made
    /// it flicker back to the program: (1) the click never synced the active
    /// window pane, so the main view kept rendering the old session, and
    /// (2) the next `refresh_sessions → ensure_selection_valid` reverted the
    /// selection because the subagent isn't in `list_items()`, which also
    /// popped the stashed program back open.
    #[tokio::test]
    async fn program_clip_click_to_subagent_persists_across_refresh() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, _server) = empty_app().await;
        let s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        // The orchestrator (the fleet dispatcher whose program holds the clips)
        // is hidden from the list, so its subagent children never get a row.
        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch".into();
        let mut sub = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        sub.id = "sub1".into();
        sub.parent_session_id = Some("orch".into());
        app.sessions = vec![s1, orch, sub];
        app.orchestrator_id = Some("orch".into());
        // The program-owner session is selected and its program is open; the
        // active window pane points at it (test_app's initial leaf is s1).
        app.selection = Selection::Session("s1".into());
        app.sync_active_window_selection();
        app.program_popup = Some(program_popup_for_test("s1", "see @{session:sub1}", 0));
        // The subagent isn't reachable through the list at all.
        assert!(
            !app.list_items()
                .iter()
                .any(|it| it.matches(&Selection::Session("sub1".into()))),
            "subagent must not have a navigable list row"
        );

        // Geometry the last program render would have captured: a modal area
        // and a clip hit for the subagent.
        app.layout.modal_area = Some(ratatui::layout::Rect::new(0, 0, 40, 10));
        app.layout.program_clip_hits = vec![ProgramClipHit {
            col_start: 4,
            col_end: 16,
            row: 3,
            session_id: "sub1".into(),
        }];

        // Click the clip.
        let consumed = app
            .handle_program_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 10,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::empty(),
            })
            .await;
        assert!(
            consumed,
            "clip click is handled by the program mouse router"
        );
        assert_eq!(app.selection.session_id(), Some("sub1"));
        // The active window pane drives the main view; it must point at the
        // target, or the click wouldn't actually reveal the session.
        assert_eq!(
            app.selection_for_window(app.active_window_id)
                .and_then(|s| s.session_id().map(str::to_string)),
            Some("sub1".to_string()),
            "active window pane must follow the clicked clip"
        );

        // A session-list refresh re-validates the selection. The subagent has
        // no list row, but it still exists — the selection must survive.
        app.ensure_selection_valid();
        assert_eq!(
            app.selection.session_id(),
            Some("sub1"),
            "subagent clip selection must persist across a session refresh"
        );
    }

    #[tokio::test]
    async fn program_c_l_centers_cursor_row_in_viewport() {
        let (mut app, _dir, _server) = empty_app().await;
        // 30 short lines into a 7-row viewport.
        let markdown = (0..30)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.program_popup = Some(program_popup_for_test("s1", &markdown, 0));
        app.layout.program_inner_area = Some(ratatui::layout::Rect::new(0, 0, 40, 7));

        // Start near top: center should be near 0 (clamped).
        app.center_program_cursor();
        let off0 = app.program_popup.as_ref().unwrap().scroll_offset;
        assert!(
            off0 <= 3,
            "near-top center should keep offset small, got {off0}"
        );

        // Move cursor far down (visual row ~29).
        let total_chars = markdown.chars().count();
        app.program_popup.as_mut().unwrap().cursor = total_chars;
        app.center_program_cursor();
        let off = app.program_popup.as_ref().unwrap().scroll_offset;
        let cursor_row =
            crate::ui::program_cursor_visual_row(Some(&app), &markdown, total_chars, 40);
        // Cursor row should be roughly centered in the 7-row window.
        // With half=3, we expect offset ~ cursor_row - 3, cursor visible in middle.
        assert!(
            (cursor_row >= off + 2) && (cursor_row < off + 7),
            "after C-l center, cursor row {cursor_row} should be inside viewport [{off}, {})",
            off + 7
        );
        // And not at the very top or bottom of the window for a mid-buffer cursor.
        assert!(
            off > 5,
            "offset should have advanced for deep cursor, got {off}"
        );
    }

    #[tokio::test]
    async fn program_open_state_is_preserved_per_session() {
        let (mut app, _dir, server) = empty_app().await;
        let s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "draft", 3));

        app.selection = Selection::Session("s2".into());
        app.sync_program_popup_with_selection();
        assert!(app.program_popup.is_none());
        assert!(app.program_popups.contains_key("s1"));

        app.selection = Selection::Session("s1".into());
        app.sync_program_popup_with_selection();
        let popup = app.program_popup.as_ref().expect("s1 program restored");
        assert_eq!(popup.buffer, "draft");
        assert_eq!(popup.cursor, 3);
        assert!(!app.program_popups.contains_key("s1"));
        server.abort();
    }

    #[tokio::test]
    async fn program_selection_cut_and_insert_replaces_selection() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.begin_program_selection();
        app.move_program_cursor(3);
        app.cut_program_selection();
        assert_eq!(app.program_clipboard.as_deref(), Some("cde"));
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "abf");
        app.insert_program_text("XY");
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "abXYf");
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_g_does_not_close_popup() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await;

        assert!(app.program_popup.is_some());
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "draft");
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_g_clears_active_selection() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.begin_program_selection();
        app.move_program_cursor(3);
        assert!(
            app.program_popup.as_ref().unwrap().selection.is_some(),
            "selection should be active before C-g"
        );

        app.handle_program_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert!(popup.selection.is_none(), "C-g should clear the selection");
        // Cancelling the mark must not mutate the buffer or move text around.
        assert_eq!(popup.buffer, "abcdef");
        assert_eq!(
            app.status.as_ref().map(|(status, _)| status.as_str()),
            Some("program selection canceled"),
            "C-g should replace the stale selection-started status"
        );
        assert_eq!(app.program_clipboard, None);
        server.abort();
    }

    #[tokio::test]
    async fn program_shift_arrow_starts_keyboard_selection() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));

        app.handle_program_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 3);
        assert_eq!(App::program_selection_range(popup), Some((2, 3)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("c"));

        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.handle_program_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 1);
        assert_eq!(App::program_selection_range(popup), Some((1, 2)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("b"));
        server.abort();
    }

    #[tokio::test]
    async fn program_shift_click_extends_selection_to_clicked_point() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 1));
        // inner content origin = (modal.x + 1 + pad, modal.y + 1 + pad) = (2, 2);
        // inner width = 9 - 2 border - 2 pad = 5, so "abcdef" paints on one row.
        let modal = Rect::new(0, 0, 9, 20);
        app.layout.modal_area = Some(modal);

        // Shift-click past the cursor (no prior selection): extends from the
        // pre-click cursor (1) to the clicked offset (4), like Shift+Arrow.
        app.handle_program_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 6,
            row: 2,
            modifiers: KeyModifiers::SHIFT,
        })
        .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 4);
        assert_eq!(App::program_selection_range(popup), Some((1, 4)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("bcd"));

        // A second shift-click keeps the original anchor (1) and moves only
        // the head, rather than restarting the selection at the new click.
        app.handle_program_mouse(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::SHIFT,
        })
        .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("a"));

        // Releasing the mouse after a shift-click commits the selection (like
        // a drag) instead of clearing it the way a plain click-release would.
        app.handle_program_mouse(&MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 2,
            row: 2,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));
        assert_eq!(app.program_clipboard.as_deref(), Some("a"));
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_space_extends_selection_with_emacs_motion() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abc\ndef", 1));
        app.layout.program_inner_area = Some(Rect::new(0, 0, 20, 5));

        app.handle_program_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 2);
        assert_eq!(App::program_selection_range(popup), Some((1, 2)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("b"));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 1);
        assert!(popup.selection.is_some());
        assert_eq!(App::program_selection_range(popup), None);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 5);
        assert_eq!(App::program_selection_range(popup), Some((1, 5)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("bc\nd"));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 1);
        assert!(popup.selection.is_some());
        assert_eq!(App::program_selection_range(popup), None);

        // C-e / C-a (end-of-line / beginning-of-line) must extend the mark
        // too, not just the char/line motions above.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 3);
        assert_eq!(App::program_selection_range(popup), Some((1, 3)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("bc"));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("a"));
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_space_extends_selection_with_raw_control_bytes() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abc\ndef", 1));
        app.layout.program_inner_area = Some(Rect::new(0, 0, 20, 5));

        // Some terminal paths deliver Emacs control keys as raw ASCII control
        // bytes with no CONTROL modifier. Program selection must handle those
        // the same way as normalized Ctrl+letter events.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('\0'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x06'), KeyModifiers::NONE))
            .await; // C-f

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 2);
        assert_eq!(App::program_selection_range(popup), Some((1, 2)));
        assert_eq!(App::selected_program_text(popup).as_deref(), Some("b"));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x05'), KeyModifiers::NONE))
            .await; // C-e
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 3);
        assert_eq!(App::program_selection_range(popup), Some((1, 3)));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x01'), KeyModifiers::NONE))
            .await; // C-a
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x0e'), KeyModifiers::NONE))
            .await; // C-n
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 4);
        assert_eq!(App::program_selection_range(popup), Some((1, 4)));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x10'), KeyModifiers::NONE))
            .await; // C-p
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x02'), KeyModifiers::NONE))
            .await; // C-b at start stays put and keeps the mark alive.
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert!(popup.selection.is_some());

        app.handle_program_key(KeyEvent::new(KeyCode::Char('\x07'), KeyModifiers::NONE))
            .await; // C-g
        let popup = app.program_popup.as_ref().unwrap();
        assert!(popup.selection.is_none(), "raw C-g should cancel selection");
        assert_eq!(popup.buffer, "abc\ndef");
        assert_eq!(
            app.status.as_ref().map(|(status, _)| status.as_str()),
            Some("program selection canceled")
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_space_accepts_terminal_aliases() {
        for mark_char in ['@', '\0'] {
            let (mut app, _dir, server) = empty_app().await;
            app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));

            app.handle_program_key(KeyEvent::new(
                KeyCode::Char(mark_char),
                KeyModifiers::CONTROL,
            ))
            .await;

            let popup = app.program_popup.as_ref().unwrap();
            let selection = popup.selection.as_ref().expect("selection mark");
            assert_eq!(selection.anchor, 2);
            assert_eq!(selection.head, 2);
            server.abort();
        }
    }

    /// The daemon broadcast that echoes our own cursor publish back at us,
    /// exactly as a live session sees after every keystroke.
    fn own_program_cursor_echo(app: &App) -> agentd_protocol::ProgramCursor {
        let popup = app.program_popup.as_ref().unwrap();
        agentd_protocol::ProgramCursor {
            session_id: popup.program.session_id.clone(),
            client_id: "c7".into(),
            label: "TUI".into(),
            kind: "tui".into(),
            cursor: popup.cursor,
            selection_anchor: popup.selection.as_ref().map(|s| s.anchor),
            selection_head: popup.selection.as_ref().map(|s| s.head),
            version: Some(popup.program.version),
            color_index: 0,
            updated_at_ms: 0,
            active: true,
        }
    }

    #[tokio::test]
    async fn program_ctrl_space_mark_survives_own_cursor_echo() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abc\ndef", 1));
        app.layout.program_inner_area = Some(Rect::new(0, 0, 20, 5));
        app.own_program_client_id = Some("c7".to_string());

        // C-Space publishes the fresh zero-width mark and the daemon
        // broadcasts it straight back. That echo used to fall into the
        // "no selection" arm (anchor == head) and drop the mark, so in a
        // live session C-f/C-b/C-p/C-n/C-a/C-e after C-Space only moved
        // the cursor — even though handler-only tests passed.
        app.handle_program_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        let selection = popup.selection.as_ref().expect("mark survives own echo");
        assert_eq!((selection.anchor, selection.head), (1, 1));

        // C-f extends by one char; the echo after it must keep the range.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((1, 2)));

        // C-e extends to end of line.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((1, 3)));

        // C-n extends a line down and must keep the sticky visual column
        // across the echo (the echo used to reset `preferred_col`).
        app.handle_program_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((1, 7)));
        assert!(
            popup.preferred_col.is_some(),
            "own echo must not reset the C-n/C-p sticky column"
        );

        // C-p extends back up.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((1, 3)));

        // C-a extends to start of line.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));

        // C-b at buffer start stays put and keeps the mark alive.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .await;
        app.on_program_cursor(own_program_cursor_echo(&app));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 0);
        assert_eq!(App::program_selection_range(popup), Some((0, 1)));
        server.abort();
    }

    #[tokio::test]
    async fn program_own_cursor_rebase_keeps_zero_width_mark() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abc\ndef", 1));
        app.layout.program_inner_area = Some(Rect::new(0, 0, 20, 5));
        app.own_program_client_id = Some("c7".to_string());

        app.handle_program_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::CONTROL))
            .await;

        // Another client's edit rebased our cursor daemon-side: same
        // zero-width mark shape, shifted offsets. The rebase must land and
        // the mark must stay alive at the new position.
        let mut rebased = own_program_cursor_echo(&app);
        rebased.cursor = 5;
        rebased.selection_anchor = Some(5);
        rebased.selection_head = Some(5);
        app.on_program_cursor(rebased);
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 5);
        let selection = popup.selection.as_ref().expect("rebased mark stays alive");
        assert_eq!((selection.anchor, selection.head), (5, 5));

        // The next motion extends from the rebased mark.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(App::program_selection_range(popup), Some((5, 6)));
        server.abort();
    }

    #[tokio::test]
    async fn program_shift_arrow_selection_wins_over_split_focus_shortcut() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(0, 0, 40, 10),
                inner_area: Rect::new(1, 1, 38, 8),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(40, 0, 40, 10),
                inner_area: Rect::new(41, 1, 38, 8),
            },
        ];
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 1;
        app.focus = PaneFocus::View;
        app.zoom = ZoomMode::None;

        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT))
            .await;

        assert_eq!(
            app.active_window_id, 1,
            "Program should consume Shift+Right before split focus navigation"
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 3);
        assert_eq!(App::program_selection_range(popup), Some((2, 3)));
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_g_dismisses_smart_clip_picker() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        // Typing the trigger opens the inline smart-clip picker.
        app.insert_program_text("@");
        assert!(app.program_smart_clip_active(), "typing @ opens the picker");

        app.handle_program_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await;

        assert!(!app.program_smart_clip_active(), "C-g dismisses the picker");
        assert!(
            app.program_popup.is_some(),
            "C-g must not close the program"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_meta_w_copies_selection_without_mutating() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.begin_program_selection();
        app.move_program_cursor(3);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT))
            .await;

        // M-w is kill-ring-save: it copies but never deletes.
        assert_eq!(app.program_clipboard.as_deref(), Some("cde"));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "abcdef", "M-w must not mutate the buffer");
        assert!(popup.selection.is_none(), "M-w deactivates the selection");
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_c_copies_selection_like_meta_w() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.begin_program_selection();
        app.move_program_cursor(3);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(app.program_clipboard.as_deref(), Some("cde"));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "abcdef", "C-c must not mutate the buffer");
        assert!(popup.selection.is_none(), "C-c deactivates the selection");
        server.abort();
    }

    #[tokio::test]
    async fn program_super_c_copies_selection_like_meta_w() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));
        app.begin_program_selection();
        app.move_program_cursor(3);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::SUPER))
            .await;

        assert_eq!(app.program_clipboard.as_deref(), Some("cde"));
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "abcdef", "Cmd-C must not mutate the buffer");
        assert!(popup.selection.is_none(), "Cmd-C deactivates the selection");
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_slash_undo_reverts_last_edit() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 5));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "draft!");

        app.handle_program_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "draft");
        server.abort();
    }

    #[tokio::test]
    async fn program_c_x_u_undo_reverts_last_edit() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 5));

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "with no edits, C-x u is currently a no-op"
        );

        app.handle_program_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "draft!");

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "C-x u should undo the previous program edit"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_c_x_u_undo_reverts_multiple_edits_in_order() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 5));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft!?",
            "both edits should be appended"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft!",
            "first C-x u should undo only the most recent edit"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "second C-x u should undo the next edit"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_copy_with_no_selection_is_noop() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdef", 2));

        // No active selection: M-w and C-c must not panic, must not copy, and
        // must leave the buffer untouched.
        app.handle_program_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::ALT))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(app.program_clipboard, None, "nothing should be copied");
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "abcdef");
        server.abort();
    }

    #[tokio::test]
    async fn program_esc_does_not_close_popup() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));

        app.handle_program_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;

        // Esc is not a program-hide affordance — the surface stays open and
        // its buffer is untouched. Show/hide is C-x Space only.
        assert!(app.program_popup.is_some());
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "draft");
        server.abort();
    }

    #[tokio::test]
    async fn program_esc_still_cancels_smart_clip_picker() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        // Type the smart-clip trigger so the inline picker is active.
        app.insert_program_text("@");
        assert!(
            app.program_smart_clip_active(),
            "typing @ should open the smart-clip picker"
        );

        app.handle_program_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;

        // Esc dismisses just the picker; the program surface remains open.
        assert!(
            !app.program_smart_clip_active(),
            "Esc should cancel the picker"
        );
        assert!(
            app.program_popup.is_some(),
            "Esc must not close the program"
        );
        server.abort();
    }

    /// Regression for the program/palette input-routing bug: with the
    /// program open, `C-x x` opens the command palette over it, and the
    /// keys typed afterwards must fill the palette input — not leak into
    /// the program buffer underneath. The top-level dispatch used to route
    /// every key to the program whenever `program_popup` was `Some`, so the
    /// palette was unusable while a program was focused.
    #[tokio::test]
    async fn program_open_command_palette_captures_typed_chars() {
        let (mut app, _dir, server) = empty_app().await;
        // No orchestrator session → `C-x x` opens the M-x command palette.
        app.orchestrator_id = None;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));

        // `C-x x` while the program is focused opens the command palette.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert!(
            matches!(
                app.minibuffer.as_ref().map(|m| &m.intent),
                Some(MinibufferIntent::CommandPalette)
            ),
            "C-x x must open the command palette over the program"
        );

        // Subsequent keystrokes must land in the palette input...
        app.on_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE))
            .await;

        assert_eq!(
            app.minibuffer.as_ref().map(|m| m.input.as_str()),
            Some("hi"),
            "typing must fill the command palette input"
        );
        // ...and must NOT leak into the program buffer underneath.
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "typing into the palette must not insert into the program"
        );
        server.abort();
    }

    /// Same routing precedence for the operator/orchestrator panel: when
    /// `C-x x` opens the persistent orchestrator input over a focused
    /// program, keys must go to the orchestrator (its PTY), not the program.
    #[tokio::test]
    async fn program_open_orchestrator_panel_captures_typed_chars() {
        let (mut app, _dir, server) = empty_app().await;
        // An orchestrator session → `C-x x` opens the operator input panel.
        app.orchestrator_id = Some("orch".to_string());
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert!(
            matches!(
                app.minibuffer.as_ref().map(|m| &m.intent),
                Some(MinibufferIntent::Orchestrator)
            ),
            "C-x x must open the orchestrator panel over the program"
        );

        // A plain keystroke is forwarded to the orchestrator's PTY, which
        // snaps its scrollback back to live — an observable side effect of
        // the orchestrator path running instead of the program path.
        app.orchestrator_scrollback = 7;
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.orchestrator_scrollback, 0,
            "typing must route to the orchestrator PTY (snaps scrollback to live)"
        );
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "typing into the orchestrator must not insert into the program"
        );
        server.abort();
    }

    /// Counterpart guard: with NO minibuffer/palette overlay open, the
    /// program keeps capturing keystrokes as before.
    #[tokio::test]
    async fn program_typing_with_no_overlay_still_inserts() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 5));
        assert!(app.minibuffer.is_none(), "precondition: no overlay open");

        app.on_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE))
            .await;

        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft!",
            "with no overlay, a typed char inserts into the program"
        );
        assert!(app.minibuffer.is_none());
        server.abort();
    }

    /// Regression: with a program visible in the view pane, switching focus to
    /// the session list (`C-x o`) must let Up/Down and `C-n`/`C-p` move the
    /// list selection. The top-level dispatch used to route every keystroke to
    /// the program whenever `program_popup` was `Some`, ignoring focus — so
    /// list navigation appeared dead while a program was open.
    #[tokio::test]
    async fn program_open_list_focus_navigates_session_list() {
        let (mut app, _dir, server) = two_session_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        app.focus = PaneFocus::List;
        assert_eq!(app.selection.session_id(), Some("s1"));

        // Down -> NextSession moves the list selection, not the program cursor.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.selection.session_id(),
            Some("s2"),
            "Down must move the list selection while the list is focused"
        );

        // Up -> PrevSession moves back.
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selection.session_id(), Some("s1"));

        // `C-n` / `C-p` are the explicit next/prev-session bindings and behave
        // the same way.
        app.on_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.selection.session_id(), Some("s2"));
        app.on_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.selection.session_id(), Some("s1"));

        server.abort();
    }

    /// Counterpart guard: when the program itself holds focus, arrow keys still
    /// drive the program cursor and never touch the list selection. (Vertical
    /// movement needs a rendered viewport, so this exercises the horizontal
    /// Right key, which routes through the same focus-gated dispatch.)
    #[tokio::test]
    async fn program_open_view_focus_moves_program_cursor() {
        let (mut app, _dir, server) = two_session_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "ab\ncd\nef", 0));
        app.focus = PaneFocus::View;
        assert_eq!(app.selection.session_id(), Some("s1"));

        // Right advances the program cursor one character, leaving the list
        // selection untouched.
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            1,
            "Right moves the program cursor while the view is focused"
        );
        assert_eq!(
            app.selection.session_id(),
            Some("s1"),
            "moving the program cursor must not change the list selection"
        );

        server.abort();
    }

    /// Regression: clicking the program body reclaims keyboard focus for the
    /// view pane. The reported bug was: click the session list (focus → List),
    /// then click back on the visible program — the caret moved but `focus`
    /// stayed `List`, so the `on_key` routing gate kept sending keystrokes to
    /// the list and typing into the program silently did nothing.
    #[tokio::test]
    async fn program_body_click_reclaims_view_focus() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, server) = two_session_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "ab\ncd\nef", 0));
        app.layout.modal_area = Some(ratatui::layout::Rect::new(0, 0, 40, 10));
        // Precondition the bug hit: the session list had grabbed keyboard focus
        // while the program stayed visible in the view pane.
        app.focus = PaneFocus::List;

        let consumed = app
            .handle_program_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 3,
                modifiers: crossterm::event::KeyModifiers::empty(),
            })
            .await;

        assert!(
            consumed,
            "a click inside the program modal is handled by the program mouse router"
        );
        assert_eq!(
            app.focus,
            PaneFocus::View,
            "clicking the program body must reclaim view focus so typing reaches it"
        );

        // End-to-end: with focus reclaimed, keystrokes now drive the program
        // cursor rather than the list selection. (Cursor reset to 0 so the
        // assertion is independent of where the click's caret landed.)
        app.program_popup.as_mut().unwrap().cursor = 0;
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            1,
            "after the click, Right moves the program cursor (input routed to the program)"
        );
        assert_eq!(
            app.selection.session_id(),
            Some("s1"),
            "typing into the reclaimed program must not move the list selection"
        );

        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_s_starts_program_search() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        app.program_popup.as_mut().unwrap().buffer = "draft changed".to_string();

        let handled = tokio::time::timeout(
            Duration::from_millis(100),
            app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
        )
        .await;

        assert!(handled.is_ok(), "raw C-s should not call program save");
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "draft changed");
        assert_eq!(popup.saved_markdown, "draft");
        assert!(popup.search.is_some(), "C-s starts program search");
        assert_eq!(popup.search.as_ref().unwrap().query, "");
        assert!(popup.search.as_ref().unwrap().matches.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn program_incremental_search_navigates_matches() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta alpha", 0));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search active");
        assert_eq!(search.query, "alpha");
        assert_eq!(search.matches, vec![(0, 5), (11, 16)]);
        assert_eq!(search.selected, 0);
        assert_eq!(popup.cursor, 0);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search still active");
        assert_eq!(search.selected, 1);
        assert_eq!(popup.cursor, 11);

        app.handle_program_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search still active");
        assert_eq!(search.selected, 0);
        assert_eq!(popup.cursor, 0);

        app.handle_program_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.program_popup.as_ref().unwrap().search.is_none());
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 0);
        server.abort();
    }

    #[tokio::test]
    async fn program_incremental_search_starts_at_or_after_anchor_cursor() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "one alpha two alpha", 10));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search active");
        assert_eq!(search.query, "alpha");
        assert_eq!(search.matches, vec![(4, 9), (14, 19)]);
        assert_eq!(search.selected, 1);
        assert_eq!(popup.cursor, 14);
        server.abort();
    }

    #[tokio::test]
    async fn program_incremental_search_cancel_restores_anchor_cursor() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha alpha", 6));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;

        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            6,
            "during search the first match >= anchor is selected (cursor moves)"
        );
        app.handle_program_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL))
            .await;
        let popup = app.program_popup.as_ref().unwrap();
        assert!(popup.search.is_none(), "C-g exits search");
        assert_eq!(popup.cursor, 6);
        server.abort();
    }

    #[tokio::test]
    async fn program_search_finds_smart_clip_by_label() {
        // Buffer: "hello @{session:stest} world"
        // Session "stest" has title "my-task" and harness "shell", state Done.
        // Searching "my-task" should find the clip (label "✓ my-task · shell")
        // even though "my-task" does not appear in the raw buffer text.
        //
        // "@{" starts at char 6; body "session:stest" is 13 chars; "}" at char 21.
        // Clip char range: [6, 22) = 6..(6+2+13+1)=22.
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "stest".into();
        session.title = Some("my-task".into());
        session.harness = "shell".into();
        session.state = agentd_protocol::SessionState::Done;
        app.sessions = vec![session];
        app.program_popup = Some(program_popup_for_test(
            "s1",
            "hello @{session:stest} world",
            0,
        ));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        for ch in "my-task".chars() {
            app.handle_program_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .await;
        }

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search active");
        assert_eq!(search.query, "my-task");
        assert!(
            !search.matches.is_empty(),
            "smart clip label 'my-task' should produce a search match"
        );
        let (match_start, match_end) = search.matches[search.selected];
        assert_eq!(match_start, 6, "match should start at the '@' of the clip");
        assert_eq!(match_end, 22, "match should end after the '}}' of the clip");
        assert_eq!(popup.cursor, 6, "cursor should navigate to the clip");
        server.abort();
    }

    #[tokio::test]
    async fn program_search_highlights_raw_text_match_inside_smart_clip() {
        // Buffer: "hello @{session:stest} world"
        // Searching "stest" — it appears literally inside the raw clip body.
        // The raw-buffer match must be found and its start must sit inside the
        // clip range so the rendering overlap check will highlight the chip.
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "stest".into();
        app.sessions = vec![session];
        app.program_popup = Some(program_popup_for_test(
            "s1",
            "hello @{session:stest} world",
            0,
        ));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        for ch in "stest".chars() {
            app.handle_program_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .await;
        }

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search active");
        assert_eq!(search.query, "stest");
        assert!(
            !search.matches.is_empty(),
            "raw text 'stest' inside clip should produce a search match"
        );
        // "stest" starts at char 16 inside "@{session:stest}": h(0)e(1)l(2)l(3)o(4) (5)@(6){(7)s(8)...(16)s(16)t(17)e(18)s(19)t(20)
        // Clip range is [6, 22); match at (16, 21) is inside the clip — overlap check covers it.
        let (match_start, _) = search.matches[0];
        assert!(
            match_start >= 6 && match_start < 22,
            "raw match start {match_start} should fall inside the clip range [6, 22)"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_incremental_search_paste_extends_query_not_buffer() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta alpha", 6));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.on_paste("alpha".to_string()).await;

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search remains active");
        assert_eq!(popup.buffer, "alpha beta alpha");
        assert_eq!(search.query, "alpha");
        assert_eq!(search.matches, vec![(0, 5), (11, 16)]);
        assert_eq!(search.selected, 1);
        assert_eq!(popup.cursor, 11);
        server.abort();
    }

    #[tokio::test]
    async fn program_tab_indents_list_item() {
        let (mut app, _dir, server) = empty_app().await;
        // Cursor at the end of "- item" (char offset 6).
        app.program_popup = Some(program_popup_for_test("s1", "- item", 6));

        app.handle_program_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "  - item", "Tab adds one indent level");
        // The cursor rides along with the text it was on (still end of line).
        assert_eq!(popup.cursor, 8);
        server.abort();
    }

    #[tokio::test]
    async fn program_shift_tab_outdents_list_item() {
        let (mut app, _dir, server) = empty_app().await;
        // Cursor at the end of "  - item" (char offset 8).
        app.program_popup = Some(program_popup_for_test("s1", "  - item", 8));

        app.handle_program_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "- item", "Shift-Tab removes one indent level");
        assert_eq!(popup.cursor, 6);
        server.abort();
    }

    #[tokio::test]
    async fn program_shift_tab_at_top_level_is_noop() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "- item", 3));

        app.handle_program_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.buffer, "- item",
            "outdent at column 0 clamps to a no-op"
        );
        assert_eq!(
            popup.cursor, 3,
            "cursor is undisturbed by a clamped outdent"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_tab_indents_multi_line_selection() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "- one\n- two", 0));
        // Select from the start of the buffer through the end of the second
        // list line so both lines fall inside the selection.
        app.begin_program_selection();
        app.move_program_cursor(11);

        app.handle_program_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.buffer, "  - one\n  - two",
            "Tab indents every selected list line"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_nested_list_item_renders_more_indented() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "- parent\n  - child", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let text = rendered_text(term.backend().buffer());

        let parent_line = text
            .lines()
            .find(|l| l.contains("parent"))
            .expect("parent list item rendered");
        let child_line = text
            .lines()
            .find(|l| l.contains("child"))
            .expect("nested list item rendered");
        let bullet_col = |line: &str| line.find('•').expect("bullet glyph rendered");
        assert!(
            bullet_col(child_line) > bullet_col(parent_line),
            "nested child bullet must render more indented than its parent:\n  parent={parent_line:?}\n  child={child_line:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_render_registers_run_affordance_hits() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.begin_program_selection();
        app.move_program_cursor(5);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let text = rendered_text(term.backend().buffer());

        assert!(text.contains("▶"), "title run icon should render: {text:?}");
        assert!(
            text.contains("▣"),
            "program mode toggle should render: {text:?}"
        );
        assert!(
            !text.contains("<program>"),
            "title should no longer render a literal program label: {text:?}"
        );
        assert!(
            text.contains("Run"),
            "selection run menu should render: {text:?}"
        );
        assert!(app.layout.program_title_run_hit.is_some());
        assert!(app.layout.program_title_toggle_hit.is_some());
        assert!(app.layout.program_selection_run_hit.is_some());
        server.abort();
    }

    #[tokio::test]
    async fn program_remote_cursor_does_not_replace_underlying_character_and_labels_are_tagged() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let cursor = "\n".chars().count() + "alp".chars().count();
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.program_collaborators.insert(
            "peer-1".to_string(),
            agentd_protocol::ProgramCursor {
                session_id: "s1".to_string(),
                client_id: "peer-1".to_string(),
                label: "Peer".to_string(),
                kind: "tui".to_string(),
                cursor,
                selection_anchor: None,
                selection_head: None,
                version: Some(1),
                color_index: 2,
                updated_at_ms: chrono::Utc::now().timestamp_millis(),
                active: true,
            },
        );

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            cursor,
            inner.width as usize,
        );
        let x = inner.x + col as u16;
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cursor_cell = buffer.cell((x, y)).expect("remote cursor cell");
        assert_eq!(
            cursor_cell.symbol(),
            "h",
            "remote cursor must style the target cell without replacing its glyph"
        );
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "remote cursor should underline the target cell"
        );
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD),
            "remote cursor should emphasize the target cell"
        );

        let label_cell = buffer
            .cell((x.saturating_add(1), y - 1))
            .expect("remote cursor label cell");
        assert_eq!(label_cell.symbol(), "P");
        assert_eq!(label_cell.style().bg, Some(ratatui::style::Color::Yellow));
        assert_eq!(label_cell.style().fg, Some(app.theme.highlight_fg));

        let after_short_label_cell = buffer
            .cell((x.saturating_add(5), y - 1))
            .expect("cell after short remote cursor label");
        assert_ne!(
            after_short_label_cell.style().bg,
            Some(ratatui::style::Color::Yellow),
            "short remote cursor labels should only highlight their actual text width"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_remote_cursor_hides_after_inactivity_timeout() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let cursor = "\n".chars().count() + "alp".chars().count();
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.program_collaborators.insert(
            "peer-1".to_string(),
            agentd_protocol::ProgramCursor {
                session_id: "s1".to_string(),
                client_id: "peer-1".to_string(),
                label: "Peer".to_string(),
                kind: "tui".to_string(),
                cursor,
                selection_anchor: None,
                selection_head: None,
                version: Some(1),
                color_index: 2,
                updated_at_ms: chrono::Utc::now()
                    .timestamp_millis()
                    .saturating_sub(PROGRAM_COLLAB_CURSOR_TTL_MS + 1),
                active: true,
            },
        );

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            cursor,
            inner.width as usize,
        );
        let x = inner.x + col as u16;
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cursor_cell = buffer.cell((x, y)).expect("remote cursor cell");
        assert_eq!(
            cursor_cell.symbol(),
            "h",
            "stale remote cursor should leave the target glyph plain"
        );
        assert!(
            !cursor_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "stale remote cursor should not underline the target cell"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_remote_cursor_label_truncates_to_capped_highlight_width() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let cursor = "\n".chars().count() + "alp".chars().count();
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.program_collaborators.insert(
            "peer-1".to_string(),
            agentd_protocol::ProgramCursor {
                session_id: "s1".to_string(),
                client_id: "peer-1".to_string(),
                label: "PeerCollaborator".to_string(),
                kind: "tui".to_string(),
                cursor,
                selection_anchor: None,
                selection_head: None,
                version: Some(1),
                color_index: 2,
                updated_at_ms: chrono::Utc::now().timestamp_millis(),
                active: true,
            },
        );

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            cursor,
            inner.width as usize,
        );
        let x = inner.x + col as u16;
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let label_x = x.saturating_add(1);
        let rendered_label: String = (0..12)
            .map(|dx| {
                buffer
                    .cell((label_x.saturating_add(dx), y - 1))
                    .expect("remote cursor label cell")
                    .symbol()
            })
            .collect();

        assert_eq!(rendered_label, "PeerCollabo…");
        for dx in 0..12 {
            let cell = buffer
                .cell((label_x.saturating_add(dx), y - 1))
                .expect("highlighted remote cursor label cell");
            assert_eq!(
                cell.style().bg,
                Some(ratatui::style::Color::Yellow),
                "ellipsized label text should be highlighted at column {dx}"
            );
        }
        let after_label_cell = buffer
            .cell((label_x.saturating_add(12), y - 1))
            .expect("cell after ellipsized remote cursor label");
        assert_ne!(
            after_label_cell.style().bg,
            Some(ratatui::style::Color::Yellow),
            "remote cursor label highlight should stop at the ellipsized text width"
        );
        server.abort();
    }

    /// Spec 0065 agent presence: an agent cursor (`kind == "agent"`) must be
    /// styled distinctly from a human TUI/web peer's cursor (italic instead
    /// of underline) without hiding the target glyph.
    #[tokio::test]
    async fn program_remote_agent_cursor_uses_italic_instead_of_underline() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let cursor = "\n".chars().count() + "alp".chars().count();
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.program_collaborators.insert(
            "agent-1".to_string(),
            agentd_protocol::ProgramCursor {
                session_id: "s1".to_string(),
                client_id: "agent-1".to_string(),
                label: "claude".to_string(),
                kind: "agent".to_string(),
                cursor,
                selection_anchor: None,
                selection_head: None,
                version: Some(1),
                color_index: 2,
                updated_at_ms: chrono::Utc::now().timestamp_millis(),
                active: true,
            },
        );

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            cursor,
            inner.width as usize,
        );
        let x = inner.x + col as u16;
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cursor_cell = buffer.cell((x, y)).expect("agent cursor cell");
        assert_eq!(
            cursor_cell.symbol(),
            "h",
            "agent cursor must style the target cell without replacing its glyph"
        );
        assert!(
            cursor_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC),
            "agent cursor should italicize the target cell"
        );
        assert!(
            !cursor_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::UNDERLINED),
            "agent cursor should not use the human-peer underline"
        );

        let label_cell = buffer
            .cell((x.saturating_add(1), y - 1))
            .expect("agent cursor label cell");
        assert_eq!(label_cell.symbol(), "c", "labeled with the agent's harness");
        assert!(
            label_cell
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::ITALIC),
            "agent cursor label should also be italicized"
        );
        server.abort();
    }

    /// Spec 0065 agent presence: a fresh agent edit's span (carried in
    /// `selection_anchor`/`selection_head`) briefly tints the program body
    /// instead of repainting it instantly.
    #[tokio::test]
    async fn program_remote_agent_cursor_reveals_edited_span_briefly() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        // "alpha" spans char offsets [1, 6); the point cursor sits at the end
        // of that span (offset 6, the following space). Routed through
        // `on_program_cursor` (rather than a raw `program_collaborators`
        // insert) so it also records the client-receipt freshness the reveal
        // gate now keys off (GAP D) — the daemon's `updated_at_ms` alone no
        // longer drives it.
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 6,
            selection_anchor: Some(1),
            selection_head: Some(6),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: true,
        });
        // The reveal now sweeps in over `PROGRAM_AGENT_REVEAL_MS` (GAP D
        // typewriter effect) rather than tinting the whole span the instant
        // it's received, so observe it partway through the window — the
        // leading part of "alpha" should already be tinted, but the sweep
        // shouldn't have caught up to its tail yet. Half the window leaves
        // generous margin on both sides against test-execution jitter.
        if let Some(entry) = app.program_agent_reveal_receipts.get_mut("agent-1") {
            entry.1 = Instant::now()
                - Duration::from_millis((crate::ui::PROGRAM_AGENT_REVEAL_MS / 2) as u64);
        }

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, start_col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            1,
            inner.width as usize,
        );
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let leading_cell = buffer
            .cell((inner.x + start_col as u16, y))
            .expect("revealed cell");
        assert_eq!(
            leading_cell.style().bg,
            Some(app.theme.inactive_highlight_bg),
            "fresh agent edit should briefly tint the leading edge of its span"
        );
        let trailing_col = start_col + "alpha".chars().count() - 1;
        let trailing_cell = buffer
            .cell((inner.x + trailing_col as u16, y))
            .expect("unswept trailing cell");
        assert_ne!(
            trailing_cell.style().bg,
            Some(app.theme.inactive_highlight_bg),
            "the sweep should not yet have reached the tail of the span at the window's midpoint"
        );
        server.abort();
    }

    /// Spec 0065 agent presence (GAP D typewriter sweep): right when an agent
    /// edit's cursor is received, none of its span should be tinted yet —
    /// the reveal sweeps in over `PROGRAM_AGENT_REVEAL_MS` rather than
    /// painting the whole span instantly.
    #[tokio::test]
    async fn program_remote_agent_cursor_reveal_starts_unswept() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 6,
            selection_anchor: Some(1),
            selection_head: Some(6),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: true,
        });

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, start_col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            1,
            inner.width as usize,
        );
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cell = buffer
            .cell((inner.x + start_col as u16, y))
            .expect("unswept cell");
        assert_ne!(
            cell.style().bg,
            Some(app.theme.inactive_highlight_bg),
            "the sweep should not have tinted anything at (essentially) zero elapsed time"
        );
        server.abort();
    }

    /// Spec 0065 agent presence (GAP D): the reveal must key off the local
    /// receipt clock, not the daemon's `updated_at_ms` — broadcast transit
    /// and the render tick can eat most or all of a short reveal window
    /// before the first paint, so a cursor whose daemon stamp already reads
    /// as stale must still reveal if this is the first time it's been seen.
    #[tokio::test]
    async fn program_remote_agent_cursor_reveal_is_receipt_not_daemon_stamp_gated() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        // Daemon stamp is already well past the reveal window — under the old
        // daemon-stamp gate this would never reveal. Received live just now,
        // it must reveal anyway.
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 6,
            selection_anchor: Some(1),
            selection_head: Some(6),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now()
                .timestamp_millis()
                .saturating_sub(crate::ui::PROGRAM_AGENT_REVEAL_MS * 10),
            active: true,
        });
        // As above: observe partway through the reveal window (still driven
        // by the fresh local receipt `on_program_cursor` just recorded, not
        // by the ancient `updated_at_ms` above) rather than the unswept
        // instant right after the notification arrives.
        if let Some(entry) = app.program_agent_reveal_receipts.get_mut("agent-1") {
            entry.1 = Instant::now()
                - Duration::from_millis((crate::ui::PROGRAM_AGENT_REVEAL_MS / 2) as u64);
        }

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, start_col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            1,
            inner.width as usize,
        );
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cell = buffer
            .cell((inner.x + start_col as u16, y))
            .expect("revealed cell");
        assert_eq!(
            cell.style().bg,
            Some(app.theme.inactive_highlight_bg),
            "a cursor received live just now must reveal even with a stale daemon stamp"
        );
        server.abort();
    }

    /// Spec 0065 agent presence (GAP D): rebasing an agent cursor through an
    /// edit it did not author must not renew its reveal freshness — the
    /// daemon leaves `updated_at_ms` unchanged on a rebase, and the local
    /// receipt clock must honor that rather than treating every notification
    /// as a fresh write.
    #[tokio::test]
    async fn program_remote_agent_cursor_rebase_does_not_retrigger_reveal() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        let updated_at_ms = chrono::Utc::now().timestamp_millis();
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 6,
            selection_anchor: Some(1),
            selection_head: Some(6),
            version: Some(1),
            color_index: 2,
            updated_at_ms,
            active: true,
        });
        // Age the receipt out past the reveal window, then rebase (same
        // `updated_at_ms`, shifted offsets) — mirroring what the daemon sends
        // when someone else's edit lands after the agent's own.
        if let Some(entry) = app.program_agent_reveal_receipts.get_mut("agent-1") {
            entry.1 = Instant::now()
                - Duration::from_millis((crate::ui::PROGRAM_AGENT_REVEAL_MS + 1) as u64);
        }
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 7,
            selection_anchor: Some(2),
            selection_head: Some(7),
            version: Some(1),
            color_index: 2,
            updated_at_ms,
            active: true,
        });

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, start_col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            2,
            inner.width as usize,
        );
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        let cell = buffer
            .cell((inner.x + start_col as u16, y))
            .expect("rebased cell");
        assert_ne!(
            cell.style().bg,
            Some(app.theme.inactive_highlight_bg),
            "a rebase carrying the same updated_at_ms must not re-trigger the reveal"
        );
        server.abort();
    }

    /// Spec 0065 agent presence: the reveal highlight is brief — once it has
    /// aged past its short window it must not still be tinting the buffer,
    /// even though the point cursor + label remain until the normal
    /// one-minute presence TTL.
    #[tokio::test]
    async fn program_remote_agent_cursor_reveal_fades_after_its_window() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "\nalpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        // Establish the cursor + its (fresh) local receipt via the real
        // notification path, then backdate the receipt directly — the reveal
        // now fades on the client's own receipt clock, not the daemon's
        // `updated_at_ms`, so that's what must age out here.
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 6,
            selection_anchor: Some(1),
            selection_head: Some(6),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: true,
        });
        if let Some(entry) = app.program_agent_reveal_receipts.get_mut("agent-1") {
            entry.1 = Instant::now()
                - Duration::from_millis((crate::ui::PROGRAM_AGENT_REVEAL_MS + 1) as u64);
        }

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let inner = app.layout.program_inner_area.expect("program inner area");
        let popup = app.program_popup.as_ref().expect("program popup");
        let (visual_row, start_col) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            &popup.buffer,
            1,
            inner.width as usize,
        );
        let y = inner.y + visual_row as u16;
        let buffer = term.backend().buffer();
        for col in start_col..start_col + "alpha".chars().count() {
            let cell = buffer
                .cell((inner.x + col as u16, y))
                .expect("no-longer-revealed cell");
            assert_ne!(
                cell.style().bg,
                Some(app.theme.inactive_highlight_bg),
                "reveal highlight should no longer tint column {col} once past its window"
            );
        }
        server.abort();
    }

    /// GAP E (spec 0065 agent presence): an agent edit landing below the
    /// current viewport is invisible by construction — the cursor and reveal
    /// both paint at the edit's own location — so the bottom border should
    /// grow a plain-language "agent editing ↓" indicator pointing at it.
    #[tokio::test]
    async fn program_agent_edge_indicator_points_down_for_activity_below_viewport() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let markdown = (1..=200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let end_cursor = markdown.chars().count();
        app.program_popup = Some(program_popup_for_test("s1", &markdown, end_cursor));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
            popup.scroll_offset = 0;
        }
        // The agent's own edit landed at the very end of a long document; the
        // popup is still scrolled to the top, so it's off the bottom.
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: end_cursor,
            selection_anchor: Some(end_cursor.saturating_sub(4)),
            selection_head: Some(end_cursor),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: true,
        });

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let rect = app.layout.modal_area.expect("program modal area");
        let buffer = term.backend().buffer();
        let bottom_y = rect.y + rect.height - 1;
        let text: String = (rect.x..rect.x + rect.width)
            .filter_map(|x| buffer.cell((x, bottom_y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            text.contains("agent editing") && text.contains('\u{2193}'),
            "bottom border should show a down-pointing edge indicator, got: {text:?}"
        );
        server.abort();
    }

    /// GAP E (spec 0065 agent presence): the mirror case — activity above a
    /// scrolled-down viewport points "up" on the top border instead, and
    /// must do so without displacing the close button's fixed hit-test
    /// position (`view_close_button_range` assumes it's always the pane's
    /// very corner).
    #[tokio::test]
    async fn program_agent_edge_indicator_points_up_for_activity_above_viewport() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        let markdown = (1..=200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.program_popup = Some(program_popup_for_test("s1", &markdown, 5));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
            // Scrolled far down; clamped to the real max inside the renderer.
            popup.scroll_offset = 500;
        }
        // The agent's edit landed near the very top of the document, off the
        // top of the scrolled-down viewport.
        app.on_program_cursor(agentd_protocol::ProgramCursor {
            session_id: "s1".to_string(),
            client_id: "agent-1".to_string(),
            label: "claude".to_string(),
            kind: "agent".to_string(),
            cursor: 5,
            selection_anchor: Some(0),
            selection_head: Some(5),
            version: Some(1),
            color_index: 2,
            updated_at_ms: chrono::Utc::now().timestamp_millis(),
            active: true,
        });

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let rect = app.layout.modal_area.expect("program modal area");
        let buffer = term.backend().buffer();
        let top_y = rect.y;
        let text: String = (rect.x..rect.x + rect.width)
            .filter_map(|x| buffer.cell((x, top_y)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            text.contains("agent editing") && text.contains('\u{2191}'),
            "top border should show an up-pointing edge indicator, got: {text:?}"
        );

        let (close_x_start, close_x_end, close_y) = crate::ui::view_close_button_range(rect);
        assert_eq!(
            close_y, rect.y,
            "the close button's hit-test row must still be the top border"
        );
        let close_cell = buffer
            .cell((close_x_start, close_y))
            .expect("close button cell");
        assert!(
            "☰x".contains(close_cell.symbol()),
            "the close/session-actions glyph must still paint inside its fixed hit-test range \
             ({close_x_start}, {close_x_end}), unmoved by the new edge-indicator title; got {:?}",
            close_cell.symbol()
        );
        server.abort();
    }

    #[test]
    fn program_selection_block_ids_include_touched_full_blocks() {
        let mut popup = program_popup_for_test("s1", "- alpha beta\n\n- gamma", 0);
        popup.selection = Some(ProgramSelection {
            anchor: 2,
            head: 7,
            dragged: false,
        });

        let ids = App::selected_program_block_ids(&popup).expect("selected block ids");

        assert!(
            ids.contains(&agentd_protocol::program_block_id("- alpha beta")),
            "a partial text selection should shimmer its enclosing block"
        );
        assert!(
            !ids.contains(&agentd_protocol::program_block_id("- gamma")),
            "untouched blocks should not shimmer for a selection run"
        );
    }

    /// The widget indicator's leading "─" stitches the square into the title
    /// bar's top border, so it must carry the *program* border color (accent_alt),
    /// not the session view's green `pane_border_style`. Regression: it painted
    /// a green dash on the program's accent border.
    #[tokio::test]
    async fn program_widget_icon_dash_matches_program_border_color() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.ui_panels.insert(
            "s1".into(),
            HashMap::from([(
                "w1".to_string(),
                agentd_protocol::UiPanel {
                    id: "w1".into(),
                    source: Some("w1.md".into()),
                    title: Some("widget".into()),
                    created_at_ms: 1,
                    placement: agentd_protocol::UiPlacement::Sticky,
                    markdown: "# widget".into(),
                },
            )]),
        );
        app.program_popup = Some(program_popup_for_test("s1", "# program\n\nbody", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        // The widget title actually painted (otherwise this test is vacuous).
        assert!(
            !app.layout.dynamic_ui_widget_hits.is_empty(),
            "widget title square should register a hit"
        );

        let modal = app.layout.modal_area.expect("program modal area");
        let accent = app.theme.accent_alt;
        let buf = term.backend().buffer();
        let y = modal.y;
        // Every "─" on the program's top border — including the indicator's
        // leading dash — must use the accent border color, never the green
        // session-view border.
        let mismatched: Vec<u16> = (modal.x..modal.x + modal.width)
            .filter(|&x| buf.cell((x, y)).map(|c| c.symbol()) == Some("─"))
            .filter(|&x| buf.cell((x, y)).and_then(|c| c.style().fg) != Some(accent))
            .collect();
        assert!(
            mismatched.is_empty(),
            "program top-border dashes must match the accent border color; mismatched cols: {mismatched:?}"
        );
        server.abort();
    }

    /// Hovering/pinning a session's sticky widget while its program is open must
    /// reveal the widget body *on top of* the program. Regression: the program's
    /// own `Clear` wiped the widget the session view had drawn underneath, so the
    /// widget was never visible while the program was shown.
    #[tokio::test]
    async fn program_reveals_pinned_widget_over_program() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.ui_panels.insert(
            "s1".into(),
            HashMap::from([(
                "w1".to_string(),
                agentd_protocol::UiPanel {
                    id: "w1".into(),
                    source: Some("w1.md".into()),
                    title: Some("ZZWIDGET".into()),
                    created_at_ms: 1,
                    placement: agentd_protocol::UiPlacement::Sticky,
                    markdown: "# ZZWIDGET\n\nbody text".into(),
                },
            )]),
        );
        // Pin the widget so it is visible without simulating a hover.
        app.dynamic_ui_selected
            .insert(("s1".to_string(), "w1".to_string()));
        app.program_popup = Some(program_popup_for_test("s1", "# program\n\nbody", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let text = rendered_text(term.backend().buffer());

        assert!(
            text.contains("ZZWIDGET"),
            "pinned widget body should render on top of the program: {text:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_defaults_to_roll_down_with_terminal_visible() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "# program\n\nbody", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }

        let backend = ratatui::backend::TestBackend::new(120, 45);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let base = app.layout.program_base_area.expect("program base area");
        let modal = app.layout.modal_area.expect("program modal area");
        assert_eq!(modal.y, base.y);
        assert_eq!(modal.width, base.width);
        assert!(
            modal.height < base.height,
            "default Program should leave terminal visible: modal={modal:?} base={base:?}"
        );
        assert_eq!(
            app.layout.program_resize_hit,
            Some(Rect::new(
                modal.x,
                modal.y + modal.height - 1,
                modal.width,
                1
            )),
            "Program bottom border should be the resize hit row"
        );
        assert!(
            (modal.height as i32 - ((base.height as i32 * 67 + 50) / 100)).abs() <= 1,
            "default Program coverage should be about two thirds: modal={modal:?} base={base:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_bottom_border_drag_resizes_coverage() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        app.layout.program_base_area = Some(Rect::new(20, 0, 80, 30));
        app.layout.modal_area = Some(Rect::new(20, 0, 80, 20));
        app.layout.program_resize_hit = Some(Rect::new(20, 19, 80, 1));

        assert!(
            app.handle_program_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 40,
                row: 19,
                modifiers: KeyModifiers::NONE,
            })
            .await,
            "resize border should consume mouse down"
        );
        assert!(app.resizing_program_popup.is_some());

        app.handle_program_mouse(&MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 40,
            row: 28,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert_eq!(app.program_popup.as_ref().unwrap().cover_percent, 97);

        app.handle_program_mouse(&MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 40,
            row: 8,
            modifiers: KeyModifiers::NONE,
        })
        .await;
        assert!(app.resizing_program_popup.is_none());
        assert_eq!(app.program_popup.as_ref().unwrap().cover_percent, 30);
        server.abort();
    }

    #[tokio::test]
    async fn clicking_exposed_terminal_focuses_terminal_without_hiding_program() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        app.focus = PaneFocus::View;
        app.layout.program_base_area = Some(Rect::new(20, 0, 80, 30));
        app.layout.modal_area = Some(Rect::new(20, 0, 80, 20));

        let consumed = app
            .handle_program_mouse(&MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 40,
                row: 24,
                modifiers: KeyModifiers::NONE,
            })
            .await;

        assert!(!consumed, "exposed terminal click should fall through");
        let popup = app.program_popup.as_ref().expect("Program remains visible");
        assert!(popup.terminal_focus);
        assert!(
            popup.slide_changed_at.is_some(),
            "focus flip should start the slide animation"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().buffer,
            "draft",
            "terminal-focused keys must not edit Program Markdown"
        );
        server.abort();
    }

    #[tokio::test]
    async fn c_x_ctrl_o_toggles_between_program_and_terminal_focus() {
        let (mut app, _dir, server) = captured_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));
        app.focus = PaneFocus::View;

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .await;

        let popup = app.program_popup.as_ref().expect("program remains open");
        assert!(
            popup.terminal_focus,
            "C-x C-o from Program focus should focus the terminal"
        );
        assert_eq!(app.focus, PaneFocus::View);
        assert_eq!(
            app.status.as_ref().map(|(message, _)| message.as_str()),
            Some("focus: session terminal")
        );
        assert!(
            popup.slide_changed_at.is_some(),
            "terminal focus should start the slide animation"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL))
            .await;

        let popup = app.program_popup.as_ref().expect("program remains open");
        assert!(
            !popup.terminal_focus,
            "C-x C-o from terminal focus should refocus Program"
        );
        assert_eq!(app.focus, PaneFocus::View);
        assert_eq!(
            app.status.as_ref().map(|(message, _)| message.as_str()),
            Some("focus: program")
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_terminal_focus_slide_animates_and_reverses_mid_flight() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 0));

        // Before any focus flip, the fraction sits at the anchored endpoint.
        assert_eq!(
            app.program_popup
                .as_ref()
                .unwrap()
                .slide_fraction(Instant::now()),
            0.0
        );

        app.set_program_terminal_focus(true);
        let changed_at = app
            .program_popup
            .as_ref()
            .unwrap()
            .slide_changed_at
            .expect("focus flip records its instant");
        assert!(
            app.program_popup
                .as_ref()
                .unwrap()
                .slide_fraction(changed_at)
                < 0.01,
            "the slide starts anchored, not snapped right"
        );
        let half = app.program_popup.as_ref().unwrap().slide_fraction(
            changed_at + Duration::from_millis(PROGRAM_REVEAL_MS / 2),
        );
        assert!(
            (half - 0.5).abs() < 0.05,
            "halfway through the popup is mid-slide, got {half}"
        );
        assert_eq!(
            app.program_popup.as_ref().unwrap().slide_fraction(
                changed_at + Duration::from_millis(PROGRAM_REVEAL_MS),
            ),
            1.0,
            "the slide settles fully slid"
        );

        // A redundant flip must not restart the animation.
        app.set_program_terminal_focus(true);
        assert_eq!(
            app.program_popup.as_ref().unwrap().slide_changed_at,
            Some(changed_at)
        );

        // Reversing mid-flight resumes from the in-flight fraction instead of
        // snapping to an endpoint: simulate a slide-right that started half a
        // reveal ago, then hand focus back to the Program.
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.slide_from = 0.0;
            popup.slide_changed_at =
                Some(Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS / 2));
        }
        app.set_program_terminal_focus(false);
        assert!(
            (app.program_popup.as_ref().unwrap().slide_from - 0.5).abs() < 0.1,
            "reversal should start near the mid-flight position, got {}",
            app.program_popup.as_ref().unwrap().slide_from
        );
        let done = app
            .program_popup
            .as_ref()
            .unwrap()
            .slide_changed_at
            .expect("reversal records instant")
            + Duration::from_millis(PROGRAM_REVEAL_MS);
        assert_eq!(
            app.program_popup.as_ref().unwrap().slide_fraction(done),
            0.0,
            "the reversed slide settles back at the pane's left edge"
        );
        server.abort();
    }

    /// The Run button now lives in the LEFT cluster of the program title bar,
    /// between the session name and the ` * modified` marker — not pinned to the
    /// right side any more.
    #[tokio::test]
    async fn program_title_run_button_sits_in_left_cluster() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
            // Diverge the buffer from the saved markdown so the ` * modified`
            // marker renders to the right of the Run button.
            popup.buffer = "alpha beta".into();
        }

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let buf = term.backend().buffer();
        let modal = app.layout.modal_area.expect("modal area");

        let run = app
            .layout
            .program_title_run_hit
            .expect("run hit registered");
        assert_eq!(run.2, modal.y, "run sits on the title border row");
        assert!(
            run.0 < modal.x + modal.width / 2,
            "run {run:?} must sit in the left cluster, not the right side"
        );

        // Locate the ▶ glyph and the dirty-marker `*` on the title row.
        let col_of = |needle: &str| -> Option<u16> {
            (modal.x..modal.x + modal.width)
                .find(|&x| buf.cell((x, modal.y)).map(|c| c.symbol()) == Some(needle))
        };
        let run_col = col_of("▶").expect("run glyph renders on the title row");
        let star_col = col_of("*").expect("dirty marker renders on the title row");
        assert!(
            run.0 <= run_col && run_col < run.1,
            "run glyph (col {run_col}) paints inside its hit range {run:?}"
        );
        assert!(
            run_col < star_col,
            "Run ▶ (col {run_col}) must sit left of the ` * modified` marker (col {star_col})"
        );
        server.abort();
    }

    /// The program session-actions button reuses the session chat view's
    /// geometry — the same `view_close_button_range` slot — but as of #556 it
    /// paints in the program border color (`accent_alt`) so the ☰ reads as part
    /// of the program frame, not in the shared session-view `matrix_close` hue.
    #[tokio::test]
    async fn program_title_actions_reuse_session_geometry_in_program_border_color() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        // Program border color (#556): `program_border_style` paints the frame in
        // `accent_alt`, and the render path passes that hue through to the ☰.
        let program_border_color = app.theme.accent_alt;

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let buf = term.backend().buffer();
        let modal = app.layout.modal_area.expect("modal area");

        assert_eq!(
            app.layout.program_title_close_hit,
            Some(crate::ui::view_close_button_range(modal)),
            "program actions must reuse the session-view action geometry"
        );
        let (cx, ce, cy) = crate::ui::view_close_button_range(modal);
        let glyph_cell = (cx..ce)
            .filter_map(|x| buf.cell((x, cy)))
            .find(|cell| cell.symbol() == "☰")
            .expect("hamburger glyph paints in its range");
        assert_eq!(
            glyph_cell.style().fg,
            Some(program_border_color),
            "program action glyph should use the program border color (#556)"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_run_button_spins_while_running() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }

        app.start_program_run("s1", "alpha beta", false, "");

        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let run = app
            .layout
            .program_title_run_hit
            .expect("run hit registered");
        let run_glyph = term
            .backend()
            .buffer()
            .cell((run.0.saturating_add(1), run.2))
            .map(|c| c.symbol().to_string())
            .unwrap_or_default();
        assert_eq!(run_glyph, app.spinner_frame().to_string());
        server.abort();
    }

    #[tokio::test]
    async fn program_run_button_spins_on_rerun_when_shimmer_already_exists() {
        // Pressing Run while a shimmer block already exists for this session
        // (an older run whose first output has already been seen, so the
        // button had stopped pulsing) must still restart the pulse for the
        // fresh press rather than staying dark.
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));
        {
            let popup = app.program_popup.as_mut().unwrap();
            popup.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        }
        app.program_runs.insert(
            "s1".to_string(),
            ProgramRun {
                started_at: Instant::now() - Duration::from_secs(5),
                pending: HashSet::new(),
                pending_tooltips: HashMap::new(),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(60),
                first_output_seen: true,
                stage: agentd_protocol::ProgramRunStage::FirstOutput,
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 0,
            },
        );

        app.start_program_run("s1", "alpha beta", false, "alpha beta");

        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let run = app
            .layout
            .program_title_run_hit
            .expect("run hit registered");
        let run_glyph = term
            .backend()
            .buffer()
            .cell((run.0.saturating_add(1), run.2))
            .map(|c| c.symbol().to_string())
            .unwrap_or_default();
        assert_eq!(run_glyph, app.spinner_frame().to_string());
        server.abort();
    }

    #[tokio::test]
    async fn program_clip_chip_color_reflects_live_session_status() {
        let (mut app, _dir, server) = empty_app().await;
        let mut running = summary_with_kind(agentd_protocol::SessionKind::User);
        running.id = "s-run".into();
        running.state = agentd_protocol::SessionState::Running;
        let mut errored = summary_with_kind(agentd_protocol::SessionKind::User);
        errored.id = "s-err".into();
        errored.state = agentd_protocol::SessionState::Errored;
        app.sessions = vec![running, errored];
        app.selection = Selection::Session("s-run".into());
        app.program_popup = Some(program_popup_for_test(
            "s-run",
            "@{session:s-run} @{session:s-err} @{session:s-ghost}",
            0,
        ));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);

        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let hits = app.layout.program_clip_hits.clone();
        assert_eq!(hits.len(), 3, "expected three session clip hits: {hits:?}");
        let buf = term.backend().buffer();
        // Sample a couple of cells into the chip rather than `col_start`
        // itself: the buffer's text cursor sits at offset 0 (the popup's
        // `cursor` field), which paints its own block-cursor cell over the
        // chip's leading padding space and isn't what this test covers.
        let style_of = |id: &str| {
            let hit = hits
                .iter()
                .find(|h| h.session_id == id)
                .unwrap_or_else(|| panic!("no clip hit for {id}"));
            buf.cell((hit.col_start + 2, hit.row)).expect("cell").style()
        };

        let running_style = style_of("s-run");
        let errored_style = style_of("s-err");
        let ghost_style = style_of("s-ghost");
        assert_eq!(
            running_style.bg,
            Some(app.theme.success),
            "a running worker's chip should read as success-colored"
        );
        assert_eq!(
            errored_style.bg,
            Some(app.theme.danger),
            "a dying worker's chip should turn danger-colored"
        );
        assert_eq!(
            ghost_style.bg,
            Some(app.theme.muted),
            "an unresolved session id should render muted, not the old fixed accent color"
        );
        assert!(
            ghost_style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT),
            "a missing session's chip should render struck-through"
        );
        assert!(
            !errored_style
                .add_modifier
                .contains(ratatui::style::Modifier::CROSSED_OUT),
            "a known-but-errored session is not struck through, only recolored"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_clip_hover_shows_missing_session_tooltip() {
        let (mut app, _dir, server) = empty_app().await;
        let s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        app.sessions = vec![s1];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "talk @{session:ghost}", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);

        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let hit = app
            .layout
            .program_clip_hits
            .first()
            .cloned()
            .expect("clip hit for ghost session");
        app.mouse_pos = Some((hit.col_start, hit.row));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("hover tooltip should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("session deleted"),
            "hovering a dead session clip with no preview should degrade to the \
             plain-language status instead of showing nothing: {text}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_session_hover_card_shows_session_output() {
        use crate::pty_render::ItemHistory;

        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s2.id = "s2".into();
        s2.title = Some("Worker".into());
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "talk @{session:s2}", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);

        let mut history = ItemHistory::new();
        history.feed_pty(b"SESS_PREVIEW_MARKER\nsecond line");
        app.histories.insert("s2".into(), history);

        let backend = ratatui::backend::TestBackend::new(120, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let hit = app
            .layout
            .program_clip_hits
            .first()
            .cloned()
            .expect("clip hit for s2");
        app.mouse_pos = Some((hit.col_start, hit.row));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program hover card should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("SESS_PREVIEW_"),
            "session output preview should include session marker"
        );
        assert!(
            !text.contains("session output"),
            "preview card should not include the old hover label"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_session_hover_card_works_on_unfocused_split_program() {
        use crate::pty_render::ItemHistory;

        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s3 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s2.id = "s2".into();
        s3.id = "s3".into();
        s3.title = Some("Worker".into());
        app.sessions = vec![s1, s2, s3];
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 2;
        app.selection = Selection::Session("s2".into());
        let mut inactive_program = program_popup_for_test("s1", "talk @{session:s3}", 0);
        inactive_program.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        app.program_popups.insert("s1".into(), inactive_program);

        let mut history = ItemHistory::new();
        history.feed_pty(b"UNFOCUSED_SPLIT_PREVIEW\nsecond line");
        app.histories.insert("s3".into(), history);

        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let pane = app
            .layout
            .main_window_areas
            .iter()
            .find(|hit| hit.id == 1)
            .expect("inactive split pane")
            .area;
        let inner = pane.inner(ratatui::layout::Margin {
            horizontal: 1 + PROGRAM_CONTENT_PADDING_X,
            vertical: 1 + PROGRAM_CONTENT_PADDING_Y,
        });
        let popup = app.program_popups.get("s1").expect("stashed s1 program");
        let hit = crate::ui::program_session_clip_hits(Some(&app), &popup.buffer, 0, inner)
            .into_iter()
            .find(|hit| hit.session_id == "s3")
            .expect("inactive program clip hit");
        app.mouse_pos = Some((hit.col_start, hit.row));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("inactive program hover card should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("UNFOCUSED_SPLIT_"),
            "hovering a clip in an unfocused split program should show the session tail"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_shimmer_tooltip_works_on_unfocused_split_program() {
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s2.id = "s2".into();
        app.sessions = vec![s1, s2];
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 2;
        app.selection = Selection::Session("s2".into());
        let markdown = "running shimmer";
        let mut inactive_program = program_popup_for_test("s1", markdown, 0);
        inactive_program.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        app.program_popups.insert("s1".into(), inactive_program);

        let block_id = agentd_protocol::program_block_id(markdown);
        app.program_runs.insert(
            "s1".into(),
            ProgramRun {
                started_at: Instant::now(),
                pending: HashSet::from([block_id.clone()]),
                pending_tooltips: HashMap::from([(block_id, "Still running".into())]),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(60),
                first_output_seen: true,
                stage: agentd_protocol::ProgramRunStage::FirstOutput,
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 1,
            },
        );

        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let pane = app
            .layout
            .main_window_areas
            .iter()
            .find(|hit| hit.id == 1)
            .expect("inactive split pane")
            .area;
        let inner = pane.inner(ratatui::layout::Margin {
            horizontal: 1 + PROGRAM_CONTENT_PADDING_X,
            vertical: 1 + PROGRAM_CONTENT_PADDING_Y,
        });
        app.mouse_pos = Some((inner.x, inner.y));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("inactive program hover tooltip should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("Still running"),
            "hovering shimmer in an unfocused split program should show its tooltip"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_shimmer_hover_shows_status_text_not_session_preview() {
        use crate::pty_render::ItemHistory;

        // Hovering a shimmering block's prose (away from its clip chip) shows
        // only the block's concise status tooltip. The rolled-down Program view
        // can expose the terminal directly; session previews remain exclusive
        // to explicit session clip chips.
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s3 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s2.id = "s2".into();
        s3.id = "s3".into();
        app.sessions = vec![s1, s2, s3];
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 2;
        app.selection = Selection::Session("s2".into());

        let markdown = "Building the PR @{session:s3}";
        let mut inactive_program = program_popup_for_test("s1", markdown, 0);
        inactive_program.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        app.program_popups.insert("s1".into(), inactive_program);

        let mut dispatcher_history = ItemHistory::new();
        dispatcher_history.feed_pty(b"SHIMMER_TERMINAL_PREVIEW\nsecond line");
        app.histories.insert("s1".into(), dispatcher_history);
        let mut worker_history = ItemHistory::new();
        worker_history.feed_pty(b"WORKER_TERMINAL_OUTPUT\nsecond line");
        app.histories.insert("s3".into(), worker_history);

        let block_id = agentd_protocol::program_block_spans(markdown)
            .into_iter()
            .next()
            .expect("one block")
            .id;
        app.program_runs.insert(
            "s1".into(),
            ProgramRun {
                started_at: Instant::now(),
                pending: HashSet::from([block_id.clone()]),
                pending_tooltips: HashMap::from([(block_id, "Building PR".into())]),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(60),
                first_output_seen: true,
                stage: agentd_protocol::ProgramRunStage::FirstOutput,
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 1,
            },
        );

        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let pane = app
            .layout
            .main_window_areas
            .iter()
            .find(|hit| hit.id == 1)
            .expect("inactive split pane")
            .area;
        let inner = pane.inner(ratatui::layout::Margin {
            horizontal: 1 + PROGRAM_CONTENT_PADDING_X,
            vertical: 1 + PROGRAM_CONTENT_PADDING_Y,
        });
        // Hover the block's leading text cell — away from the trailing clip chip —
        // so the shimmer hover, not the clip hover, owns the tooltip.
        app.mouse_pos = Some((inner.x, inner.y));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("shimmer hover tooltip should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("Building PR"),
            "hovering shimmering prose should show the block status tooltip"
        );
        assert!(
            !text.contains("SHIMMER_TERMINAL_PREVIEW"),
            "shimmer hover must not show the dispatching session terminal tail"
        );
        assert!(
            !text.contains("WORKER_TERMINAL_OUTPUT"),
            "shimmer hover must not show the worker session it names"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_shimmer_status_tooltip_persists_while_pointer_stays_still() {
        // The shimmer-text tooltip must persist for as long as the pointer stays
        // over the shimmering block — matching the clip-chip hover — rather than
        // self-dismissing once the pointer has been briefly still.
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        let mut s3 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s3.id = "s3".into();
        app.sessions = vec![s1, s3];
        app.selection = Selection::Session("s1".into());

        let markdown = "Building the PR @{session:s3}";
        let mut program = program_popup_for_test("s1", markdown, 0);
        program.revealed_at = Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        app.program_popup = Some(program);

        let block_id = agentd_protocol::program_block_spans(markdown)
            .into_iter()
            .next()
            .expect("one block")
            .id;
        app.program_runs.insert(
            "s1".into(),
            ProgramRun {
                started_at: Instant::now(),
                pending: HashSet::from([block_id.clone()]),
                pending_tooltips: HashMap::from([(block_id, "Building PR".into())]),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(60),
                first_output_seen: true,
                stage: agentd_protocol::ProgramRunStage::FirstOutput,
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 1,
            },
        );

        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let modal = app.layout.modal_area.expect("modal area");
        let inner = modal.inner(ratatui::layout::Margin {
            horizontal: 1 + PROGRAM_CONTENT_PADDING_X,
            vertical: 1 + PROGRAM_CONTENT_PADDING_Y,
        });
        app.mouse_pos = Some((inner.x, inner.y));

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("shimmer hover tooltip should render");
        assert!(
            rendered_text(term.backend().buffer()).contains("Building PR"),
            "hovering a shimmering block should show its status tooltip"
        );

        // The pointer hasn't moved since the last render, but it also hasn't
        // left the block — real wall-clock time passing alone must not hide
        // the tooltip.
        tokio::time::sleep(Duration::from_millis(1_200)).await;

        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("shimmer hover tooltip should still render");
        assert!(
            rendered_text(term.backend().buffer()).contains("Building PR"),
            "the tooltip must persist for as long as the pointer stays over the block, \
             not self-dismiss once it has been briefly still"
        );
        server.abort();
    }

    /// Seed `app` with one selected session that owns a single sticky widget,
    /// plus a fully-revealed program popup over it. Returns the keep-alive dir
    /// and mock-daemon handle.
    async fn program_with_widget_app() -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use agentd_protocol::{UiPanel, UiPlacement};
        let (mut app, dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.ui_panels.insert(
            "s1".into(),
            HashMap::from([(
                "w1".into(),
                UiPanel {
                    id: "w1".into(),
                    source: Some("w1.md".into()),
                    title: Some("W1".into()),
                    created_at_ms: 10,
                    placement: UiPlacement::Sticky,
                    markdown: "# W1".into(),
                },
            )]),
        );
        app.program_popup = Some(program_popup_for_test("s1", "alpha", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        (app, dir, server)
    }

    #[tokio::test]
    async fn program_title_renders_action_button_and_widget_icons() {
        let (mut app, _dir, server) = program_with_widget_app().await;

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let buf = term.backend().buffer();
        let modal = app.layout.modal_area.expect("modal area");

        // The actions button reuses the chat-view geometry, and its hamburger
        // glyph paints inside that range.
        let close = app
            .layout
            .program_title_close_hit
            .expect("actions hit registered");
        assert_eq!(
            close,
            crate::ui::view_close_button_range(modal),
            "program actions must reuse the session-view action geometry"
        );
        let close_text: String = (close.0..close.1)
            .filter_map(|x| buf.cell((x, close.2)).map(|c| c.symbol().to_string()))
            .collect();
        assert!(
            close_text.contains('☰'),
            "actions button glyph should paint within its hit range: {close_text:?}"
        );

        // The sticky widget registers a title-bar indicator via the SHARED
        // `render_session_widget_title` helper, so it lands in
        // `dynamic_ui_widget_hits` — the same list the pane title bar uses. The
        // session view and the program both render it at identical geometry.
        let widget_hits: Vec<_> = app
            .layout
            .dynamic_ui_widget_hits
            .iter()
            .filter(|h| h.panel_id == "w1")
            .cloned()
            .collect();
        assert!(
            !widget_hits.is_empty(),
            "the sticky widget should register a title-bar indicator"
        );
        let w = &widget_hits[0];
        assert!(
            widget_hits
                .iter()
                .all(|h| h.start_col == w.start_col && h.row == w.row),
            "every registration of the widget icon must share one geometry: {widget_hits:?}"
        );
        // The □/■ indicator paints on the title row exactly at the registered
        // hit cell: ratatui's right-aligned title placement lands the glyph on
        // the same column the hover/click hitbox covers — the same as the
        // session-view pane title bar. (Aiming one cell off used to be required;
        // see `dynamic_ui_trigger_range`.)
        let painted_at = |x: u16| {
            buf.cell((x, w.row))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default()
        };
        assert!(
            painted_at(w.start_col) == "□" || painted_at(w.start_col) == "■",
            "the widget indicator glyph must paint exactly on its hit cell {w:?}: {:?}",
            (w.start_col.saturating_sub(2)..w.start_col + 2)
                .map(painted_at)
                .collect::<Vec<_>>()
        );

        // Run now lives in the LEFT cluster (left of the widget icon), and the
        // widget icon sits left of the rightmost actions button.
        let run = app
            .layout
            .program_title_run_hit
            .expect("run hit registered");
        assert!(
            run.1 <= w.start_col,
            "run {run:?} (left cluster) should sit left of the widget icon at {}",
            w.start_col
        );
        assert!(
            w.end_col <= close.0,
            "widget icon (..{}) should sit left of actions {close:?}",
            w.end_col
        );
        assert_eq!(
            close.1,
            modal.x + modal.width - 1,
            "actions should be the rightmost control: {close:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_title_action_button_click_opens_session_menu() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, server) = program_with_widget_app().await;

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let close = app
            .layout
            .program_title_close_hit
            .expect("actions hit registered");

        // Clicking the hamburger opens the same session actions menu as the
        // normal session view; it no longer dismisses the program.
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.0 + 1,
            row: close.2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        assert!(
            app.program_popup.as_ref().is_some_and(|p| !p.closing),
            "clicking the actions button should leave the program open"
        );
        assert!(
            app.session_title_menu
                .as_ref()
                .is_some_and(|menu| menu.session_id == "s1"),
            "clicking the actions button should open the session menu"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_title_widget_icon_click_toggles_pin() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, server) = program_with_widget_app().await;

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let hit = app
            .layout
            .dynamic_ui_widget_hits
            .iter()
            .find(|h| h.panel_id == "w1")
            .cloned()
            .expect("widget hit registered");
        assert!(!app.dynamic_ui_panel_pinned("s1", "w1"));

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.start_col,
            row: hit.row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        assert!(
            app.dynamic_ui_panel_pinned("s1", "w1"),
            "clicking the title-bar widget indicator should pin the widget"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_title_run_button_click_runs_program() {
        use agentd_protocol::ipc_method;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let _ = seen_tx.send((method.clone(), params.clone()));
                let program = serde_json::json!({
                    "session_id": "s1",
                    "markdown": "alpha",
                    "version": 2,
                    "updated_at_ms": 0,
                    "template_id": null,
                });
                let result = match method.as_str() {
                    ipc_method::PROGRAM_EXECUTE => {
                        serde_json::json!({ "program": program, "prompt": "sent", "active_run": null })
                    }
                    ipc_method::PROGRAM_UPDATE => serde_json::json!({ "program": program }),
                    _ => Value::Null,
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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
        let mut app = test_app(client, Vec::new());
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");

        let run = app
            .layout
            .program_title_run_hit
            .expect("run hit registered");
        let close = app
            .layout
            .program_title_close_hit
            .expect("close hit registered");
        // Run sits left of the close button (the rightmost control).
        assert!(
            run.1 <= close.0,
            "run {run:?} should sit left of close {close:?}"
        );

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: run.0 + 1,
            row: run.2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        let (method, params) = seen_rx.recv().await.expect("program execute request");
        assert_eq!(
            method,
            ipc_method::PROGRAM_EXECUTE,
            "clicking the title Run button should execute the program"
        );
        assert!(
            params.get("selection").map(Value::is_null).unwrap_or(true),
            "the title Run button runs the whole program, not a selection: {params:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_selection_run_click_clears_menu_and_runs_selection() {
        use agentd_protocol::ipc_method;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let _ = seen_tx.send((method.clone(), params.clone()));
                let program = serde_json::json!({
                    "session_id": "s1",
                    "markdown": "alpha beta",
                    "version": 1,
                    "updated_at_ms": 0,
                    "template_id": null,
                });
                let result = match method.as_str() {
                    ipc_method::PROGRAM_EXECUTE => {
                        serde_json::json!({ "program": program, "prompt": "sent", "active_run": null })
                    }
                    _ => Value::Null,
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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
        let mut app = test_app(client, Vec::new());
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);
        app.begin_program_selection();
        app.move_program_cursor(5);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let run = app
            .layout
            .program_selection_run_hit
            .expect("selection run hit registered");

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: run.0,
            row: run.2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        assert!(
            app.program_popup
                .as_ref()
                .is_some_and(|popup| popup.selection.is_none()),
            "selection Run click should clear selection so the context menu disappears"
        );
        assert!(
            app.layout.program_selection_run_hit.is_none(),
            "selection Run hitbox should clear with the context menu"
        );
        let (method, params) = seen_rx.recv().await.expect("program execute request");
        assert_eq!(method, ipc_method::PROGRAM_EXECUTE);
        assert_eq!(
            params.get("selection").and_then(Value::as_str),
            Some("alpha")
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_selection_run_click_preserves_other_pending_shimmer() {
        // Regression: clicking "Run" on a fresh selection while other blocks
        // are already shimmering from an earlier run must keep that shimmer
        // and optimistically add the newly-run block, not replace the whole
        // pending set with just the new selection.
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        // No accept loop: the click handler awaits the program/execute round
        // trip, which we deliberately never let complete so a single poll
        // observes only the synchronous optimistic shimmer update.
        let _listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let client = Client::connect(&sock).await.expect("client connects");

        let mut app = test_app(client, Vec::new());
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.id = "s1".into();
        app.sessions = vec![session];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "alpha\n\nbeta\n", 0));
        app.program_popup.as_mut().unwrap().revealed_at =
            Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS);

        // "beta" is already shimmering from an earlier, separate run.
        app.start_program_run("s1", "alpha\n\nbeta\n", false, "");
        app.program_runs.get_mut("s1").expect("run").pending =
            HashSet::from([agentd_protocol::program_block_id("beta")]);

        app.begin_program_selection();
        app.move_program_cursor(5); // selects "alpha"

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("program should render");
        let run = app
            .layout
            .program_selection_run_hit
            .expect("selection run hit registered");

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: run.0,
            row: run.2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };
        let fut = app.handle_program_mouse(&click);
        assert!(
            fut.now_or_never().is_none(),
            "click handling should still be awaiting the daemon response"
        );

        let pending = &app.program_runs["s1"].pending;
        assert!(
            pending.contains(&agentd_protocol::program_block_id("beta")),
            "clicking Run on a new selection must not clear another block's existing shimmer"
        );
        assert!(
            pending.contains(&agentd_protocol::program_block_id("alpha")),
            "the freshly-run selection should be optimistically shimmered too"
        );
    }

    #[tokio::test]
    async fn program_run_for_orchestrator_does_not_panic_and_submit_does_not_clear_other_shimmers()
    {
        // TDD for the reported panic (screenshot 2026-06-27 9.43.19),
        // "Program click 'run' on smith session does type the prompt, but not submit",
        // and "On smith session program, when submit the prompt, all shimmer animation clears".
        // Orchestrator (smith minibuffer) sessions must not participate in normal
        // program run shimmer, and activity on them must not wipe User/Subagent shimmers.
        let (mut app, _dir, server) = empty_app().await;

        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch1".into();
        let mut user = summary_with_kind(agentd_protocol::SessionKind::User);
        user.id = "user1".into();
        app.sessions = vec![orch.clone(), user.clone()];
        app.selection = Selection::Session("orch1".into());

        // Exercise the run start path that could panic or set bad state for orchestrator.
        let body = "# do the task\nImplement the thing".to_string();
        app.start_program_run("orch1", &body, false, "");

        // Normal user session program run must still be tracked.
        app.start_program_run("user1", "# user program task", false, "");
        assert!(
            app.program_runs.contains_key("user1"),
            "user program run must be tracked"
        );

        // Simulate orchestrator submit / state update clearing only its own entry
        // (the unconditional removes in on_program_state / result paths).
        app.program_runs.remove("orch1");
        assert!(
            app.program_runs.contains_key("user1"),
            "user shimmer must survive orchestrator activity/submit"
        );

        // Orchestrator should not have left a popup in normal flows.
        if let Some(p) = app.program_popup.as_ref() {
            assert_ne!(p.saved_markdown, "should not be here for orchestrator");
        }

        server.abort();
    }

    #[tokio::test]
    async fn session_title_program_toggle_opens_program_from_chat_mode() {
        use agentd_protocol::ipc_method;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let program = serde_json::json!({
                    "session_id": "s1",
                    "markdown": "draft",
                    "version": 1,
                    "updated_at_ms": 0,
                    "template_id": null,
                });
                let result = match method.as_str() {
                    ipc_method::PROGRAM_GET => serde_json::json!({ "program": program }),
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
        let client = Client::connect(&sock).await.expect("client connects");
        let mut app = test_app(
            client,
            vec![summary_with_kind(agentd_protocol::SessionKind::User)],
        );

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("session title should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("●"),
            "chat mode should keep the status glyph: {text:?}"
        );
        let view = app.layout.view_area.expect("view area");
        let (x_start, _x_end, y) = crate::ui::view_program_toggle_button_range(view);

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        assert!(
            app.program_popup.is_some(),
            "clicking the title toggle should open program"
        );
        server.abort();
    }

    // Regression: in a `Below` (stacked) split the resize divider's grab zone is
    // two rows tall — the upper pane's bottom border *and* the lower pane's top
    // border. That lower row is the lower pane's title bar, where its program
    // status-glyph toggle sits. The mouse-down hit-test used to start a window
    // resize and swallow the click, so "show program" silently failed on every
    // non-top split pane (hide kept working because the active program's own
    // mouse handler intercepts that click earlier).
    #[tokio::test]
    async fn split_below_nontop_toggle_opens_program() {
        use agentd_protocol::ipc_method;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                // Echo back the requested session so the opened popup is keyed to
                // whichever pane was clicked.
                let session_id = req
                    .get("params")
                    .and_then(|p| p.get("session_id"))
                    .and_then(Value::as_str)
                    .unwrap_or("s2")
                    .to_string();
                let program = serde_json::json!({
                    "session_id": session_id,
                    "markdown": "draft",
                    "version": 1,
                    "updated_at_ms": 0,
                    "template_id": null,
                });
                let result = match method.as_str() {
                    ipc_method::PROGRAM_GET => serde_json::json!({ "program": program }),
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
        let client = Client::connect(&sock).await.expect("client connects");
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.has_pty = true;
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.has_pty = true;
        let mut app = test_app(client, vec![s1, s2]);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Below,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.focus = PaneFocus::View;

        // Render so the real split geometry — pane areas *and* the resize
        // divider — is captured into the layout, just like a live frame.
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("split should render");

        let bottom = app
            .layout
            .main_window_areas
            .iter()
            .find(|h| h.id == 2)
            .copied()
            .expect("bottom split pane registered");
        let (x_start, _x_end, y) = crate::ui::view_program_toggle_button_range(bottom.area);

        // Precondition: the toggle row really does sit inside a resize divider —
        // otherwise this test would not exercise the bug it guards against.
        assert!(
            app.layout
                .main_window_dividers
                .iter()
                .any(|d| App::rect_contains(d.area, x_start, y)),
            "expected the bottom pane's toggle row to overlap the split divider"
        );

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        assert!(
            app.resizing_main_window.is_none(),
            "clicking the toggle glyph must not start a window resize"
        );

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        assert_eq!(
            app.active_window_id, 2,
            "clicking the bottom pane focuses it"
        );
        let popup = app
            .program_popup
            .as_ref()
            .expect("showing the program should open a popup on the non-top pane");
        assert_eq!(
            popup.program.session_id, "s2",
            "the clicked pane's own program opened"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_execute_selection_saves_then_runs_selected_text() {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let _ = seen_tx.send((method.clone(), params.clone()));
                let program = serde_json::json!({
                    "session_id": "s1",
                    "markdown": params
                        .get("markdown")
                        .and_then(Value::as_str)
                        .unwrap_or("alpha beta"),
                    "version": 2,
                    "updated_at_ms": 0,
                    "template_id": null,
                });
                let result = match method.as_str() {
                    ipc_method::PROGRAM_UPDATE => serde_json::json!({ "program": program }),
                    ipc_method::PROGRAM_EXECUTE => {
                        serde_json::json!({ "program": program, "prompt": "sent" })
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
        let client = Client::connect(&sock).await.expect("client connects");
        let mut app = test_app(client, Vec::new());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));
        app.program_popup.as_mut().unwrap().buffer = "alpha beta changed".to_string();

        assert!(
            app.execute_program_popup(Some("beta".to_string()), None)
                .await
        );

        let (first_method, first_params) = seen_rx.recv().await.expect("program update request");
        let (second_method, second_params) = seen_rx.recv().await.expect("program execute request");
        assert_eq!(first_method, ipc_method::PROGRAM_UPDATE);
        assert_eq!(
            first_params.get("markdown").and_then(Value::as_str),
            Some("alpha beta changed")
        );
        assert_eq!(second_method, ipc_method::PROGRAM_EXECUTE);
        assert_eq!(
            second_params.get("selection").and_then(Value::as_str),
            Some("beta")
        );
        assert_eq!(
            second_params.get("base_version").and_then(Value::as_u64),
            Some(2)
        );
        server.abort();
    }

    /// Mock daemon for the Run overlap/idempotency guard tests below: accepts
    /// a single connection, echoes `program.update`/`program.execute` bodies
    /// back at an incrementing version, and reports every method name it
    /// receives on the returned channel so tests can assert exactly how many
    /// `program.execute` requests actually went out.
    async fn program_run_dispatch_mock_daemon(
        session_id: &str,
    ) -> (
        Arc<Client>,
        tempfile::TempDir,
        tokio::task::JoinHandle<()>,
        mpsc::UnboundedReceiver<String>,
    ) {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (methods_tx, methods_rx) = mpsc::unbounded_channel::<String>();
        let session_id = session_id.to_string();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            let mut version = 1u64;
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let _ = methods_tx.send(method.clone());
                let result = match method.as_str() {
                    ipc_method::PROGRAM_UPDATE | ipc_method::PROGRAM_EXECUTE => {
                        version += 1;
                        let markdown = params
                            .get("markdown")
                            .and_then(Value::as_str)
                            .or_else(|| params.get("selection").and_then(Value::as_str))
                            .unwrap_or_default()
                            .to_string();
                        let program = serde_json::json!({
                            "session_id": session_id,
                            "markdown": markdown,
                            "version": version,
                            "updated_at_ms": 0,
                            "template_id": null,
                        });
                        if method == ipc_method::PROGRAM_EXECUTE {
                            serde_json::json!({ "program": program, "prompt": "run" })
                        } else {
                            serde_json::json!({ "program": program })
                        }
                    }
                    _ => Value::Null,
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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
        (client, dir, server, methods_rx)
    }

    /// Drain every method name the mock daemon has recorded so far. Safe to
    /// call right after an awaited `execute_program_popup` call: its RPCs
    /// have already completed (and so already reported themselves) by the
    /// time the call returns.
    fn drain_methods(rx: &mut mpsc::UnboundedReceiver<String>) -> Vec<String> {
        let mut methods = Vec::new();
        while let Ok(method) = rx.try_recv() {
            methods.push(method);
        }
        methods
    }

    #[tokio::test]
    async fn program_run_double_dispatch_is_coalesced_into_one_execute() {
        // Two immediate identical Run gestures (double `C-x C-r`, a
        // double-clicked Run button) must not dispatch two execute turns to
        // the owning agent — the second is coalesced into the first's
        // dispatch and only sets a status message (spec 0042 consequence).
        use agentd_protocol::ipc_method;

        let (client, _dir, server, mut methods) = program_run_dispatch_mock_daemon("s1").await;
        let mut app = test_app(client, Vec::new());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));

        assert!(
            app.execute_program_popup(None, None).await,
            "first run dispatches"
        );
        assert!(
            !app
                .execute_program_popup(None, None)
                .await,
            "identical immediate re-Run must be suppressed"
        );
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(msg, _)| msg.contains("already dispatched")),
            "status should explain the suppression, got: {:?}",
            app.status
        );

        let methods = drain_methods(&mut methods);
        assert_eq!(
            methods.iter().filter(|m| *m == ipc_method::PROGRAM_EXECUTE).count(),
            1,
            "exactly one program.execute request, got: {methods:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_run_different_body_dispatches_again() {
        // A full re-Run whose body changed (the user edited the program
        // between the two Runs) must not be suppressed by the dedup guard.
        use agentd_protocol::ipc_method;

        let (client, _dir, server, mut methods) = program_run_dispatch_mock_daemon("s1").await;
        let mut app = test_app(client, Vec::new());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));

        assert!(app.execute_program_popup(None, None).await);
        app.program_popup.as_mut().unwrap().buffer = "alpha beta changed".to_string();
        assert!(
            app.execute_program_popup(None, None).await,
            "a re-Run with a changed body must dispatch again"
        );

        let methods = drain_methods(&mut methods);
        assert_eq!(
            methods.iter().filter(|m| *m == ipc_method::PROGRAM_EXECUTE).count(),
            2,
            "both dispatches should have sent program.execute, got: {methods:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_run_repeat_after_debounce_window_dispatches() {
        // Once the dedup window has elapsed, an identical Run is a
        // deliberate re-Run (spec 0042 intentionally supports this) and must
        // dispatch again.
        use agentd_protocol::ipc_method;

        let (client, _dir, server, mut methods) = program_run_dispatch_mock_daemon("s1").await;
        let mut app = test_app(client, Vec::new());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));

        assert!(app.execute_program_popup(None, None).await);
        // Simulate the debounce window having already elapsed by backdating
        // its recorded instant, matching this file's existing convention for
        // testing time-based expiry without sleeping (see e.g. `revealed_at`
        // in the popup-slide tests above).
        for state in app.program_run_dispatch.values_mut() {
            if let ProgramRunDispatchState::Dispatched(at) = state {
                *at = Instant::now() - Duration::from_millis(PROGRAM_RUN_DEDUP_WINDOW_MS + 50);
            }
        }
        assert!(
            app.execute_program_popup(None, None).await,
            "an identical re-Run after the debounce window must dispatch"
        );

        let methods = drain_methods(&mut methods);
        assert_eq!(
            methods.iter().filter(|m| *m == ipc_method::PROGRAM_EXECUTE).count(),
            2,
            "both dispatches should have sent program.execute, got: {methods:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_run_selection_not_suppressed_by_just_dispatched_full_run() {
        // Spec 0042 lets a selection Run proceed while a full run is in
        // flight elsewhere in the program; the dedup guard must not treat
        // that as a duplicate of the full run that was just dispatched.
        use agentd_protocol::ipc_method;

        let (client, _dir, server, mut methods) = program_run_dispatch_mock_daemon("s1").await;
        let mut app = test_app(client, Vec::new());
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));

        assert!(
            app.execute_program_popup(None, None).await,
            "full run dispatches"
        );
        assert!(
            app.execute_program_popup(Some("beta".to_string()), None)
                .await,
            "a selection run must not be suppressed by a just-dispatched full run"
        );

        let methods = drain_methods(&mut methods);
        assert_eq!(
            methods.iter().filter(|m| *m == ipc_method::PROGRAM_EXECUTE).count(),
            2,
            "both the full run and the selection run should have dispatched, got: {methods:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_adopts_latest_when_clean() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "# Todo\n- a\n", 0));
        // The owning agent edited the program on the daemon.
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "# Todo\n- a\n- agent added\n".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.program.version, 2);
        assert_eq!(popup.buffer, "# Todo\n- a\n- agent added\n");
        assert_eq!(popup.saved_markdown, "# Todo\n- a\n- agent added\n");
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_adopts_update_when_untouched_clip_lacks_id() {
        // Regression: an agent-written document can carry a smart clip without
        // a clip_id (the daemon stores it as-is). The popup's buffer and
        // saved_markdown are both that raw content, but its *normalized* form
        // differs — an untouched popup must not read as dirty, or every live
        // agent update is skipped until the program is hidden and reopened.
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test(
            "s1",
            "# In progress\n- task @{session:sub1}\n",
            0,
        ));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "# In progress\n\n# Done\n- task @{session:sub1}\n".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.program.version, 2);
        assert_eq!(popup.buffer, "# In progress\n\n# Done\n- task @{session:sub1}\n");
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_preserves_real_edits_when_clip_lacks_id() {
        // Real unsaved edits must still be preserved even when the last synced
        // content is not in normalized form.
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test(
            "s1",
            "# In progress\n- task @{session:sub1}\n",
            0,
        ));
        app.program_popup.as_mut().unwrap().buffer =
            "# In progress\n- task @{session:sub1}\n- human typing\n".into();
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "# Done\n- task @{session:sub1}\n".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.buffer,
            "# In progress\n- task @{session:sub1}\n- human typing\n"
        );
        assert_eq!(popup.program.version, 1);
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_rebases_caret_past_insertion_before_it() {
        // Spec 0065: adopt must remap the local caret through the content
        // change, not merely clamp it. Insertion lands before the caret
        // (after "alpha "), so the caret must shift by the insertion length
        // rather than stay pinned mid-text.
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 10));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "alpha INSERTED beta".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 19, "caret should shift by the 9-char insertion");
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_leaves_caret_unchanged_for_change_after_it() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 5));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "alpha zzz".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 5, "change is entirely after the caret");
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_shifts_caret_for_change_before_it() {
        let (mut app, _dir, server) = empty_app().await;
        // Caret at 8 sits after "alpha be" (inside "beta"). Replacing "alpha"
        // (5 chars) with "A" (1 char) shifts everything after it by -4.
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 8));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "A beta".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.cursor, 4, "caret shifts by the -4 length delta");
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_clamps_caret_inside_replaced_span() {
        let (mut app, _dir, server) = empty_app().await;
        // Caret at 2 sits inside "alpha" (the replaced span itself).
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 2));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "X beta".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.cursor, 1,
            "caret inside the replaced span clamps to the new span's end"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_rebases_selection_anchor_and_head() {
        let (mut app, _dir, server) = empty_app().await;
        let mut popup = program_popup_for_test("s1", "alpha beta", 0);
        popup.selection = Some(ProgramSelection {
            anchor: 7,
            head: 9,
            dragged: false,
        });
        app.program_popup = Some(popup);
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "A beta".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        let selection = popup.selection.as_ref().expect("selection survives adopt");
        assert_eq!((selection.anchor, selection.head), (3, 5));
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_rebases_search_anchor() {
        let (mut app, _dir, server) = empty_app().await;
        let mut popup = program_popup_for_test("s1", "alpha beta", 0);
        popup.search = Some(ProgramSearch {
            anchor_cursor: 6,
            query: "beta".into(),
            matches: Vec::new(),
            selected: 0,
        });
        app.program_popup = Some(popup);
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "A beta".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search survives adopt");
        assert_eq!(search.anchor_cursor, 2);
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_rebases_caret_across_clip_id_renormalization() {
        // The amplifier defect: a broadcast whose only difference is a
        // re-minted `clip_id=` value still adopts (clean check normalizes
        // instance ids), but the ids have different lengths (clip_9 vs
        // clip_10) so every offset after the clip must shift by the delta.
        let (mut app, _dir, server) = empty_app().await;
        let old_markdown = "before @{session:sub1 clip_id=9} after";
        let cursor = old_markdown.chars().count();
        app.program_popup = Some(program_popup_for_test("s1", old_markdown, cursor));
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "before @{session:sub1 clip_id=10} after".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.cursor,
            cursor + 1,
            "caret after the clip shifts by the 1-char id-length delta"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_recomputes_active_search_when_clean() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 0));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        for ch in "alpha".chars() {
            app.handle_program_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(
            app.program_popup
                .as_ref()
                .unwrap()
                .search
                .as_ref()
                .unwrap()
                .matches,
            vec![(0, 5)]
        );

        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "zero alpha".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search remains active");
        assert_eq!(search.query, "alpha");
        assert_eq!(search.matches, vec![(5, 10)]);
        assert_eq!(search.selected, 0);
        assert_eq!(popup.cursor, 5);
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_clamps_active_search_anchor_when_clean() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "alpha beta", 10));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL))
            .await;
        app.handle_program_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .await;

        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "tiny".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.search.as_ref().expect("search remains active");
        assert_eq!(search.query, "z");
        assert_eq!(search.anchor_cursor, 4);
        assert!(search.matches.is_empty());
        assert_eq!(popup.cursor, 4);
        server.abort();
    }

    #[test]
    fn program_blocks_split_heading_and_each_item() {
        // Heading + two consecutive items + a second heading → four blocks, so
        // each item shimmers independently of its siblings and the heading.
        let md = "# Todo\n- a\n- b\n\n# Done\n";
        let blocks = program_blocks(md);
        assert_eq!(blocks.len(), 4);
        assert_eq!((blocks[0].start_line, blocks[0].end_line), (0, 1)); // # Todo
        assert_eq!((blocks[1].start_line, blocks[1].end_line), (1, 2)); // - a
        assert_eq!((blocks[2].start_line, blocks[2].end_line), (2, 3)); // - b
        assert_eq!(blocks[3].start_line, 4); // # Done after blank line
        let spans = agentd_protocol::program_block_spans(md);
        assert_eq!(spans[0].signature, "# Todo");
        assert_eq!(spans[1].signature, "- a");
        assert_eq!(blocks[3].id, agentd_protocol::program_block_id("# Done"));
    }

    #[test]
    fn program_blocks_normalize_indentation_in_signature() {
        // Each item is its own block; signatures trim each line so cosmetic
        // indentation does not change identity (stable shimmer across re-indent).
        let spans = agentd_protocol::program_block_spans("  - a\n    - b\n");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].signature, "- a");
        assert_eq!(spans[1].signature, "- b");
    }

    #[test]
    fn program_run_pending_ids_cover_each_block() {
        let ids = program_run_pending_ids("# Todo\n- a\n\n# Done\n");
        assert_eq!(ids.len(), 3);
        assert!(ids.contains(&agentd_protocol::program_block_id("# Todo")));
        assert!(ids.contains(&agentd_protocol::program_block_id("- a")));
        assert!(ids.contains(&agentd_protocol::program_block_id("# Done")));
        // An empty body has nothing to shimmer.
        assert!(program_run_pending_ids("   \n").is_empty());
    }

    fn program_doc_for_test(
        session_id: &str,
        markdown: &str,
        version: u64,
    ) -> agentd_protocol::ProgramDocument {
        agentd_protocol::ProgramDocument {
            session_id: session_id.into(),
            markdown: markdown.into(),
            version,
            updated_at_ms: 0,
            template_id: None,
        }
    }

    fn program_progress_for_test(
        run_id: &str,
        pending: Vec<String>,
    ) -> agentd_protocol::ProgramRunProgress {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        agentd_protocol::ProgramRunProgress {
            run_id: run_id.into(),
            started_at_ms: now_ms - 1000,
            expires_at_ms: now_ms + 60_000,
            pending_block_ids: pending,
            pending_block_refs: Vec::new(),
            pending_block_tooltips: HashMap::new(),
            system_status: None,
            seen_running: true,
            first_output_seen: false,
            queued_behind_current_turn: false,
            agent_managed: false,
            stage: agentd_protocol::ProgramRunStage::Delivered,
            settled_block_count: 0,
            total_block_count: 1,
        }
    }

    #[tokio::test]
    async fn program_optimistic_run_lights_every_block_immediately() {
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let body = "# Todo\n\n- alpha\n\n- beta\n";

        app.start_program_run("s1", body, false, "");

        let run = app.program_runs.get("s1").expect("optimistic run exists");
        assert!(run.pending.contains(&id("# Todo")));
        assert!(run.pending.contains(&id("- alpha")));
        assert!(run.pending.contains(&id("- beta")));
        assert!(run.pending_tooltips.is_empty());
        assert!(!run.first_output_seen);
        assert!(run.deadline > Instant::now());
        server.abort();
    }

    #[tokio::test]
    async fn program_state_empty_run_keeps_unconfirmed_optimistic_run_then_clears_confirmed_run() {
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let body = "# Todo\n\n- alpha\n";

        app.start_program_run("s1", body, false, "");
        let optimistic_pending = app.program_runs["s1"].pending.clone();
        app.on_program_state(program_doc_for_test("s1", body, 2), None, Vec::new());

        let run = app
            .program_runs
            .get("s1")
            .expect("empty daemon state must not clear an unconfirmed optimistic run");
        assert_eq!(run.pending, optimistic_pending);
        assert!(
            app.program_settle_flourishes.get("s1").is_none(),
            "keeping the optimistic run must not flash blocks as settled"
        );

        app.on_program_state(
            program_doc_for_test("s1", body, 2),
            Some(program_progress_for_test("run-1", vec![id("- alpha")])),
            Vec::new(),
        );
        let run = app
            .program_runs
            .get("s1")
            .expect("daemon progress should be adopted");
        assert!(run.pending.contains(&id("- alpha")));

        app.on_program_state(program_doc_for_test("s1", body, 2), None, Vec::new());
        assert!(
            !app.program_runs.contains_key("s1"),
            "empty daemon state must still clear a daemon-confirmed run"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_state_empty_run_does_not_record_settle_flourish_for_kept_optimistic_run() {
        let (mut app, _dir, server) = empty_app().await;
        app.start_program_run("s1", "- pending\n", false, "");
        assert!(app.program_runs["s1"]
            .pending
            .contains(&agentd_protocol::program_block_id("- pending")));

        app.on_program_state(
            program_doc_for_test("s1", "- pending\n", 42),
            None,
            Vec::new(),
        );

        assert!(
            app.program_runs.contains_key("s1"),
            "the optimistic run should survive the empty broadcast"
        );
        assert!(
            app.program_settle_flourishes.get("s1").is_none(),
            "a skipped optimistic removal must not look like a settled block"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_optimistic_rerun_without_edits_preserves_existing_shimmer_only() {
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let body = "# Todo\n\n- settled\n\n- pending\n\n- also settled\n";

        app.start_program_run("s1", body, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::from([id("- pending")]);

        app.start_program_run("s1", body, false, body);

        let pending = &app.program_runs["s1"].pending;
        assert_eq!(pending.len(), 1);
        assert!(pending.contains(&id("- pending")));
        assert!(!pending.contains(&id("- settled")));
        assert!(!pending.contains(&id("- also settled")));
        server.abort();
    }

    #[tokio::test]
    async fn program_optimistic_rerun_falls_back_to_full_body_when_prior_pending_shares_nothing() {
        // "Shimmer block already exists" but shares no blocks with the fresh
        // press — e.g. its pending set transiently emptied mid-turn (spec
        // 0042) without the run record being reaped yet, or it was scoped to
        // a selection that no longer overlaps. Re-Run must still give
        // immediate optimistic feedback for the new request instead of
        // silently going dark.
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let body = "# Todo\n\n- alpha\n\n- beta\n";

        app.start_program_run("s1", body, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::new();

        app.start_program_run("s1", body, false, body);

        assert!(
            app.program_runs.contains_key("s1"),
            "re-Run must not clear the optimistic shimmer just because the \
             prior run's pending set had nothing in common with the fresh body"
        );
        let run = &app.program_runs["s1"];
        assert!(run.pending.contains(&id("# Todo")));
        assert!(run.pending.contains(&id("- alpha")));
        assert!(run.pending.contains(&id("- beta")));
        assert!(
            !run.first_output_seen,
            "the fresh press must restart the Run button pulse"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_optimistic_rerun_adds_user_edit_to_existing_shimmer() {
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let synced = "# Todo\n\n- settled\n\n- pending\n\n- untouched\n";
        let edited = "# Todo\n\n- settled\n\n- pending\n\n- user changed\n";

        app.start_program_run("s1", synced, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::from([id("- pending")]);

        app.start_program_run("s1", edited, false, synced);

        let pending = &app.program_runs["s1"].pending;
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&id("- pending")));
        assert!(pending.contains(&id("- user changed")));
        assert!(!pending.contains(&id("- settled")));
        assert!(!pending.contains(&id("- untouched")));
        server.abort();
    }

    #[tokio::test]
    async fn program_dirty_run_seeds_optimistic_pending_before_save_response() {
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let saved = "# Todo\n\n- settled\n\n- pending\n\n- untouched\n";
        let edited = "# Todo\n\n- settled\n\n- pending\n\n- user changed\n";

        app.program_popup = Some(program_popup_for_test("s1", saved, 0));
        app.program_popup.as_mut().unwrap().buffer = edited.to_string();
        app.start_program_run("s1", saved, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::from([id("- pending")]);
        app.start_program_run("s1", edited, false, saved);

        let pending = &app.program_runs["s1"].pending;
        assert_eq!(pending.len(), 2);
        assert!(pending.contains(&id("- pending")));
        assert!(
            pending.contains(&id("- user changed")),
            "dirty user edits must shimmer before any save RPC response"
        );
        assert!(!pending.contains(&id("- settled")));
        assert!(!pending.contains(&id("- untouched")));
        server.abort();
    }

    #[tokio::test]
    async fn program_execute_sends_explicit_shimmer_for_edited_and_pending_blocks() {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;
        use tokio::sync::mpsc;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Value)>();
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let tx = tx.clone();
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
                        let req: Value = serde_json::from_str(&line).expect("json request");
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        let params = req.get("params").cloned().unwrap_or(Value::Null);
                        let _ = tx.send((method.clone(), params.clone()));
                        let session_id = params
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or("s1");
                        let markdown = params
                            .get("markdown")
                            .and_then(Value::as_str)
                            .unwrap_or("# Todo\n\n- settled\n\n- pending\n\n- user changed\n");
                        let result = match method.as_str() {
                            ipc_method::PROGRAM_UPDATE => serde_json::json!({
                                "program": {
                                    "session_id": session_id,
                                    "markdown": markdown,
                                    "version": 2,
                                    "updated_at_ms": 0,
                                    "template_id": null
                                },
                                "blocks": []
                            }),
                            ipc_method::PROGRAM_EXECUTE => serde_json::json!({
                                "program": {
                                    "session_id": session_id,
                                    "markdown": "# Todo\n\n- settled\n\n- pending\n\n- user changed\n",
                                    "version": 2,
                                    "updated_at_ms": 0,
                                    "template_id": null
                                },
                                "prompt": "run",
                                "active_run": null,
                                "blocks": []
                            }),
                            _ => Value::Null,
                        };
                        let resp =
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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

        let client = Client::connect(&sock).await.expect("client connects");
        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.id = "s1".into();
        let mut app = test_app(client, vec![summary]);
        let id = agentd_protocol::program_block_id;
        let saved = "# Todo\n\n- settled\n\n- pending\n\n- untouched\n";
        let edited = "# Todo\n\n- settled\n\n- pending\n\n- user changed\n";
        app.program_popup = Some(program_popup_for_test("s1", saved, 0));
        app.program_popup.as_mut().unwrap().buffer = edited.to_string();
        app.start_program_run("s1", saved, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::from([id("- pending")]);

        assert!(app.execute_program_popup(None, None).await);

        let mut execute_params = None;
        while let Some((method, params)) = rx.recv().await {
            if method == ipc_method::PROGRAM_EXECUTE {
                execute_params = Some(params);
                break;
            }
        }
        let params = execute_params.expect("program.execute params");
        assert_eq!(params.get("base_version").and_then(Value::as_u64), Some(2));
        assert_eq!(
            params.get("shimmer").cloned(),
            Some(serde_json::json!([false, false, true, true])),
            "explicit shimmer must preserve still-pending and user-edited blocks"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_dirty_run_survives_empty_save_broadcast_before_execute_response() {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
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
                        let req: Value = serde_json::from_str(&line).expect("json request");
                        let id = req.get("id").cloned().unwrap_or(Value::Null);
                        let method = req
                            .get("method")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let params = req.get("params").cloned().unwrap_or(Value::Null);
                        let markdown = params
                            .get("markdown")
                            .and_then(Value::as_str)
                            .unwrap_or("# Todo\n\n- settled\n\n- pending\n\n- user changed\n");
                        let pending_id = agentd_protocol::program_block_id("- user changed");
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as i64;
                        let result = match method {
                            ipc_method::PROGRAM_UPDATE => serde_json::json!({
                                "program": {
                                    "session_id": "s1",
                                    "markdown": markdown,
                                    "version": 2,
                                    "updated_at_ms": 0,
                                    "template_id": null
                                },
                                "blocks": [],
                                "active_run": null
                            }),
                            ipc_method::PROGRAM_EXECUTE => serde_json::json!({
                                "program": {
                                    "session_id": "s1",
                                    "markdown": "# Todo\n\n- settled\n\n- pending\n\n- user changed\n",
                                    "version": 2,
                                    "updated_at_ms": 0,
                                    "template_id": null
                                },
                                "prompt": "run",
                                "active_run": {
                                    "run_id": "run-1",
                                    "started_at_ms": now_ms - 1000,
                                    "expires_at_ms": now_ms + 60000,
                                    "pending_block_ids": [pending_id],
                                    "pending_block_refs": [],
                                    "pending_block_tooltips": {},
                                    "system_status": null,
                                    "seen_running": true,
                                    "first_output_seen": false,
                                    "queued_behind_current_turn": false,
                                    "agent_managed": false,
                                    "stage": "delivered",
                                    "settled_block_count": 0,
                                    "total_block_count": 1
                                },
                                "blocks": []
                            }),
                            _ => Value::Null,
                        };
                        let resp =
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
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

        let client = Client::connect(&sock).await.expect("client connects");
        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.id = "s1".into();
        let mut app = test_app(client, vec![summary]);
        let saved = "# Todo\n\n- settled\n\n- pending\n\n- untouched\n";
        let edited = "# Todo\n\n- settled\n\n- pending\n\n- user changed\n";
        app.program_popup = Some(program_popup_for_test("s1", saved, 0));
        app.program_popup.as_mut().unwrap().buffer = edited.to_string();
        app.start_program_run("s1", edited, false, saved);
        assert!(
            !app.program_runs["s1"].pending.is_empty(),
            "dirty run should start with optimistic pending blocks"
        );

        assert!(app.save_program_popup().await);
        app.on_program_state(program_doc_for_test("s1", edited, 2), None, Vec::new());
        assert!(
            app.program_runs
                .get("s1")
                .is_some_and(|run| !run.pending.is_empty()),
            "the save's stale empty broadcast must not kill the optimistic run"
        );

        let result = app
            .client
            .program_execute(agentd_protocol::ProgramExecuteParams {
                session_id: "s1".into(),
                selection: None,
                base_version: Some(2),
                shimmer: None,
            })
            .await
            .expect("execute response");
        app.adopt_daemon_program_run("s1", result.active_run);
        let run = app.program_runs.get("s1").expect("execute confirms run");
        assert!(run.daemon_confirmed);
        assert!(!run.pending.is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn program_optimistic_selection_run_adds_to_existing_shimmer_scope() {
        // A selection run must not clear shimmer another in-flight run
        // already declared elsewhere in the program — it optimistically adds
        // its own scope on top of whatever is already pending.
        let (mut app, _dir, server) = empty_app().await;
        let id = agentd_protocol::program_block_id;
        let body = "# Todo\n\n- alpha\n\n- beta\n\n- gamma\n";

        app.start_program_run("s1", body, false, "");
        app.program_runs.get_mut("s1").expect("run").pending = HashSet::from([id("- alpha")]);

        app.start_program_run("s1", "- beta\n\n- gamma\n", true, body);

        let pending = &app.program_runs["s1"].pending;
        assert_eq!(pending.len(), 3);
        assert!(pending.contains(&id("- alpha")), "prior shimmer preserved");
        assert!(pending.contains(&id("- beta")));
        assert!(pending.contains(&id("- gamma")));
        server.abort();
    }

    #[tokio::test]
    async fn program_optimistic_empty_run_clears_existing_shimmer() {
        let (mut app, _dir, server) = empty_app().await;
        app.start_program_run("s1", "- pending\n", false, "");
        assert!(app.program_runs.contains_key("s1"));

        app.start_program_run("s1", "   \n", false, "- pending\n");

        assert!(!app.program_runs.contains_key("s1"));
        server.abort();
    }

    #[tokio::test]
    async fn program_run_waits_for_daemon_clear_state() {
        let (mut app, _dir, server) = empty_app().await;
        app.start_program_run("s1", "# Todo\n- a\n", false, "");
        app.program_runs
            .get_mut("s1")
            .expect("run")
            .daemon_confirmed = true;
        assert!(app.program_runs.contains_key("s1"));

        async fn feed(app: &mut App, session: &str, event: SessionEvent) {
            app.on_notification(Notification {
                jsonrpc: "2.0".into(),
                method: agentd_protocol::ipc_notif::EVENT.into(),
                params: Some(
                    serde_json::to_value(EventNotificationPayload {
                        session_id: session.into(),
                        at: chrono::Utc::now(),
                        event,
                        seq: 1,
                    })
                    .unwrap(),
                ),
            })
            .await;
        }

        // Session status alone (the old stop signal) must not clear the run.
        feed(
            &mut app,
            "s1",
            SessionEvent::Status {
                state: agentd_protocol::SessionState::AwaitingInput,
                detail: None,
            },
        )
        .await;
        assert!(app.program_runs.contains_key("s1"));

        // Raw session output is not authoritative in the TUI: for PTY-backed
        // runs it may be prompt echo from delivery. The daemon owns the shared
        // lifecycle and reports clears through program/state.
        feed(
            &mut app,
            "s1",
            SessionEvent::Reasoning {
                text: "thinking".into(),
            },
        )
        .await;
        assert!(app.program_runs.contains_key("s1"));

        app.on_notification(Notification {
            jsonrpc: "2.0".into(),
            method: agentd_protocol::ipc_notif::PROGRAM_STATE.into(),
            params: Some(
                serde_json::to_value(agentd_protocol::ProgramStateNotificationPayload {
                    program: agentd_protocol::ProgramDocument {
                        session_id: "s1".into(),
                        markdown: "# Todo\n- a\n".into(),
                        version: 1,
                        updated_at_ms: 0,
                        template_id: None,
                    },
                    active_run: None,
                    blocks: Vec::new(),
                })
                .unwrap(),
            ),
        })
        .await;
        assert!(
            !app.program_runs.contains_key("s1"),
            "shimmer should clear when daemon program state reports no active run"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_run_expires_at_deadline() {
        let (mut app, _dir, server) = empty_app().await;
        app.start_program_run("s1", "# Todo\n", false, "");
        // A missed first-output signal must never strand the animation.
        app.program_runs.get_mut("s1").unwrap().deadline =
            Instant::now() - Duration::from_millis(1);
        app.expire_program_runs(Instant::now());
        assert!(!app.program_runs.contains_key("s1"));
        server.abort();
    }

    #[tokio::test]
    async fn program_settle_flourish_tracks_pending_diff_and_expires() {
        let (mut app, _dir, server) = empty_app().await;
        let now = Instant::now();
        let previous = HashSet::from([
            "block-a:1".to_string(),
            "block-b:1".to_string(),
            "block-c:1".to_string(),
        ]);
        let next = HashSet::from(["block-b:1".to_string()]);

        app.record_program_settle_flourishes("s1", &previous, &next, now);

        let flourishes = app.program_settle_flourishes.get("s1").unwrap();
        assert_eq!(flourishes.len(), 2);
        assert_eq!(flourishes.get("block-a:1"), Some(&now));
        assert_eq!(flourishes.get("block-c:1"), Some(&now));
        assert!(!flourishes.contains_key("block-b:1"));

        app.expire_program_runs(
            now + Duration::from_millis(crate::app::PROGRAM_SETTLE_FLASH_MS - 1),
        );
        assert!(app
            .program_settle_flourishes
            .get("s1")
            .is_some_and(|flourishes| flourishes.contains_key("block-a:1")));

        app.expire_program_runs(now + Duration::from_millis(crate::app::PROGRAM_SETTLE_FLASH_MS));
        assert!(!app.program_settle_flourishes.contains_key("s1"));
        server.abort();
    }

    #[tokio::test]
    async fn program_state_empty_pending_progress_flashes_final_settled_refs() {
        let (mut app, _dir, server) = empty_app().await;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64;
        app.program_runs.insert(
            "s1".into(),
            ProgramRun {
                started_at: Instant::now(),
                pending: HashSet::from(["block-a:1".into(), "block-b:1".into()]),
                pending_tooltips: HashMap::new(),
                system_status: None,
                deadline: Instant::now() + Duration::from_secs(60),
                first_output_seen: true,
                stage: agentd_protocol::ProgramRunStage::default(),
                daemon_confirmed: true,
                settled_block_count: 0,
                total_block_count: 2,
            },
        );

        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "- a\n- b\n".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            Some(agentd_protocol::ProgramRunProgress {
                run_id: "run-1".into(),
                started_at_ms: now_ms - 1000,
                expires_at_ms: now_ms + 60_000,
                pending_block_ids: Vec::new(),
                pending_block_refs: Vec::new(),
                pending_block_tooltips: HashMap::new(),
                system_status: None,
                seen_running: true,
                first_output_seen: true,
                queued_behind_current_turn: false,
                agent_managed: true,
                stage: agentd_protocol::ProgramRunStage::default(),
                settled_block_count: 2,
                total_block_count: 2,
            }),
            Vec::new(),
        );

        assert!(!app.program_runs.contains_key("s1"));
        let flourishes = app.program_settle_flourishes.get("s1").unwrap();
        assert!(flourishes.contains_key("block-a:1"));
        assert!(flourishes.contains_key("block-b:1"));
        server.abort();
    }

    #[tokio::test]
    async fn program_rerun_preserves_agent_progress() {
        let (mut app, _dir, server) = empty_app().await;
        // Run 1 over a list where each item is its own block (blank-separated):
        // every block shimmers.
        let id = agentd_protocol::program_block_id;
        let original = "# Todo\n\n- alpha\n\n- beta\n\n- gamma\n";
        app.start_program_run("s1", original, false, "");
        let pending1 = &app.program_runs["s1"].pending;
        assert!(pending1.contains(&id("- alpha")));
        assert!(pending1.contains(&id("- beta")));
        assert!(pending1.contains(&id("- gamma")));

        // The agent settled the "alpha" block (rewrote it) — that's now the
        // last daemon-synced content. The user then edits "gamma" and re-Runs.
        let after_agent = "# Todo\n\n- alpha done\n\n- beta\n\n- gamma\n";
        let after_user_edit = "# Todo\n\n- alpha done\n\n- beta\n\n- gamma rework\n";
        app.start_program_run("s1", after_user_edit, false, after_agent);

        let pending2 = &app.program_runs["s1"].pending;
        // The agent's settled block does NOT re-shimmer.
        assert!(
            !pending2.contains(&id("- alpha done")),
            "a block the agent already settled must not re-shimmer on re-Run"
        );
        // The user's fresh edit shimmers (new instruction).
        assert!(pending2.contains(&id("- gamma rework")));
        // A block that was still pending and untouched keeps shimmering.
        assert!(pending2.contains(&id("- beta")));
        server.abort();
    }

    #[tokio::test]
    async fn program_selection_run_shimmers_whole_selection() {
        // A selection run is explicitly scoped by the user, so even mid-flight
        // it shimmers its whole region rather than preserving prior narrowing.
        let (mut app, _dir, server) = empty_app().await;
        app.start_program_run("s1", "# Todo\n\n- alpha\n", false, "");
        app.start_program_run("s1", "- alpha\n\n- beta\n", true, "- alpha\n");
        let pending = &app.program_runs["s1"].pending;
        assert!(pending.contains(&agentd_protocol::program_block_id("- alpha")));
        assert!(pending.contains(&agentd_protocol::program_block_id("- beta")));
        server.abort();
    }

    #[tokio::test]
    async fn program_state_notification_preserves_unsaved_edits() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "# Todo\n- a\n", 0));
        // The user is mid-edit (buffer diverges from the saved content).
        app.program_popup.as_mut().unwrap().buffer = "# Todo\n- a\n- human typing\n".into();
        app.on_program_state(
            agentd_protocol::ProgramDocument {
                session_id: "s1".into(),
                markdown: "# Todo\n- a\n- agent added\n".into(),
                version: 2,
                updated_at_ms: 0,
                template_id: None,
            },
            None,
            Vec::new(),
        );
        let popup = app.program_popup.as_ref().unwrap();
        // Unsaved edits are untouched and the base version stays stale, so the
        // save path detects the conflict and merges both sides.
        assert_eq!(popup.buffer, "# Todo\n- a\n- human typing\n");
        assert_eq!(popup.program.version, 1);
        server.abort();
    }

    #[tokio::test]
    async fn program_save_merges_disjoint_edits_on_conflict() {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (seen_tx, mut seen_rx) = tokio::sync::mpsc::unbounded_channel::<(String, Value)>();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            let mut update_calls = 0usize;
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
                let method = req
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let _ = seen_tx.send((method.clone(), params.clone()));
                let resp = match method.as_str() {
                    m if m == ipc_method::PROGRAM_UPDATE => {
                        update_calls += 1;
                        if update_calls == 1 {
                            // The human's base version is stale → conflict.
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {
                                    "code": -32603,
                                    "message": "program conflict: current version is 2, attempted base version is 1"
                                }
                            })
                        } else {
                            let md = params
                                .get("markdown")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "program": {
                                    "session_id": "s1",
                                    "markdown": md,
                                    "version": 3,
                                    "updated_at_ms": 0,
                                    "template_id": null,
                                }}
                            })
                        }
                    }
                    m if m == ipc_method::PROGRAM_GET => serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "program": {
                                "session_id": "s1",
                                "markdown": "alpha\nbeta\ngamma\n",
                                "version": 2,
                                "updated_at_ms": 0,
                                "template_id": null,
                            },
                            "revisions": []
                        }
                    }),
                    _ => serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": Value::Null }),
                };
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
        let mut app = test_app(client, Vec::new());
        // Ancestor "alpha\nbeta\n"; the human edits line 1 while the agent
        // appended "gamma" (a disjoint region) → a clean 3-way merge.
        app.program_popup = Some(program_popup_for_test("s1", "alpha\nbeta\n", 0));
        app.program_popup.as_mut().unwrap().buffer = "alpha CHANGED\nbeta\n".to_string();

        assert!(app.save_program_popup().await);

        let (m1, p1) = seen_rx.recv().await.expect("first update");
        assert_eq!(m1, ipc_method::PROGRAM_UPDATE);
        assert_eq!(p1.get("base_version").and_then(Value::as_u64), Some(1));
        let (m2, _p2) = seen_rx.recv().await.expect("program get");
        assert_eq!(m2, ipc_method::PROGRAM_GET);
        let (m3, p3) = seen_rx.recv().await.expect("second update");
        assert_eq!(m3, ipc_method::PROGRAM_UPDATE);
        assert_eq!(p3.get("base_version").and_then(Value::as_u64), Some(2));
        assert_eq!(
            p3.get("markdown").and_then(Value::as_str),
            Some("alpha CHANGED\nbeta\ngamma\n")
        );

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.program.version, 3);
        assert_eq!(popup.saved_markdown, "alpha CHANGED\nbeta\ngamma\n");
        server.abort();
    }

    #[tokio::test]
    async fn open_program_session_ids_include_active_and_cached_programes() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s2", "active", 0));
        app.program_popups
            .insert("s1".into(), program_popup_for_test("s1", "cached", 0));
        let mut closing = program_popup_for_test("s3", "closing", 0);
        closing.closing = true;
        app.program_popups.insert("s3".into(), closing);

        assert_eq!(app.open_program_session_ids(), vec!["s1", "s2"]);
        server.abort();
    }

    #[tokio::test]
    async fn program_at_trigger_filters_and_accepts_harness_smart_clip() {
        let (mut app, _dir, server) = empty_app().await;
        app.harnesses = vec![agentd_protocol::HarnessInfo {
            name: "codex".to_string(),
            available: true,
            detail: None,
            binary: None,
            description: Some("coding agent".to_string()),
            capabilities: Default::default(),
        }];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));

        app.insert_program_text("@");
        assert!(app.program_popup.as_ref().unwrap().smart_clip.is_some());
        app.insert_program_text("co");
        let candidates = app.program_smart_clip_candidates(app.program_popup.as_ref().unwrap());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].clip, "@{harness:codex}");

        app.accept_program_smart_clip();

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "@{harness:codex clip_id=clip_1}");
        assert_eq!(
            popup.cursor,
            "@{harness:codex clip_id=clip_1}".chars().count()
        );
        assert!(popup.smart_clip.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn accepted_program_smart_clips_get_unique_instance_ids() {
        let (mut app, _dir, server) = empty_app().await;
        app.harnesses = vec![agentd_protocol::HarnessInfo {
            name: "codex".to_string(),
            available: true,
            detail: None,
            binary: None,
            description: Some("coding agent".to_string()),
            capabilities: Default::default(),
        }];
        app.program_popup = Some(program_popup_for_test(
            "s1",
            "@{harness:codex clip_id=clip_1} ",
            "@{harness:codex clip_id=clip_1} ".chars().count(),
        ));

        app.insert_program_text("@");
        app.insert_program_text("co");
        app.accept_program_smart_clip();

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(
            popup.buffer,
            "@{harness:codex clip_id=clip_1} @{harness:codex clip_id=clip_2}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_candidates_are_grouped_by_type() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.title = Some("issue 132".to_string());
        app.sessions = vec![session];
        app.harnesses = vec![agentd_protocol::HarnessInfo {
            name: "codex".to_string(),
            available: true,
            detail: None,
            binary: None,
            description: Some("coding agent".to_string()),
            capabilities: Default::default(),
        }];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));

        app.insert_program_text("@");
        let candidates = app.program_smart_clip_candidates(app.program_popup.as_ref().unwrap());

        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].group, ProgramSmartClipGroup::Session);
        assert_eq!(candidates[0].label, "issue 132");
        assert_eq!(candidates[0].detail, "shell · running");
        assert_eq!(candidates[1].group, ProgramSmartClipGroup::Harness);
        assert_eq!(candidates[1].label, "codex");
        assert_eq!(candidates[1].detail, "");
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_search_cancels_on_separator() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "", 0));

        app.insert_program_text("@");
        app.insert_program_text("abc");
        assert!(app.program_popup.as_ref().unwrap().smart_clip.is_some());
        app.insert_program_text(" ");

        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "@abc ");
        assert!(popup.smart_clip.is_none());
        server.abort();
    }

    fn harness_info(name: &str, available: bool) -> agentd_protocol::HarnessInfo {
        agentd_protocol::HarnessInfo {
            name: name.to_string(),
            available,
            detail: None,
            binary: None,
            description: None,
            capabilities: Default::default(),
        }
    }

    #[tokio::test]
    async fn program_smart_clip_root_shows_top_then_separator_then_categories() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.title = Some("issue 132".to_string());
        app.sessions = vec![session];
        app.harnesses = vec![harness_info("codex", true)];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");

        let popup = app.program_popup.as_ref().unwrap();
        let rows = app.program_smart_clip_rows(popup);
        // 2 top clips, a separator, then a category per non-empty type.
        assert!(matches!(rows[0], ProgramSmartClipRow::Clip { .. }));
        assert!(matches!(rows[1], ProgramSmartClipRow::Clip { .. }));
        assert!(matches!(rows[2], ProgramSmartClipRow::Separator));
        assert!(matches!(
            rows[3],
            ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Session,
                count: 1
            }
        ));
        assert!(matches!(
            rows[4],
            ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Harness,
                count: 1
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_top_section_ranks_across_types_by_query() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.title = Some("codex helper".to_string());
        app.sessions = vec![session];
        app.harnesses = vec![harness_info("codex", true)];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));

        app.insert_program_text("@");
        app.insert_program_text("codex");
        let top = app.program_smart_clip_candidates(app.program_popup.as_ref().unwrap());
        // Exact harness-name match outranks the session's prefix match.
        assert_eq!(top[0].clip, "@{harness:codex}");
        assert_eq!(top[1].group, ProgramSmartClipGroup::Session);
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_session_category_opens_picker_dialog() {
        let (mut app, _dir, server) = empty_app().await;
        let mut alpha = summary_with_kind(agentd_protocol::SessionKind::User);
        alpha.id = "a".into();
        alpha.title = Some("alpha".into());
        alpha.position = 0;
        let mut beta = summary_with_kind(agentd_protocol::SessionKind::User);
        beta.id = "b".into();
        beta.title = Some("beta".into());
        beta.position = 1;
        app.sessions = vec![alpha, beta];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");

        // Root selectables: clip, clip, then the session category at position 2.
        app.program_popup
            .as_mut()
            .unwrap()
            .smart_clip
            .as_mut()
            .unwrap()
            .selected = 2;
        app.accept_program_smart_clip();

        // The session category now opens the richer picker dialog (spec 0063)
        // rather than the inline submenu. The underlying `@` smart-clip search
        // stays live so confirming can replace the `@` token, and the buffer is
        // untouched until then.
        assert!(app.session_picker_active());
        assert_eq!(
            app.session_picker.as_ref().unwrap().purpose,
            SessionPickerPurpose::InsertProgramClip
        );
        assert!(app.program_popup.as_ref().unwrap().smart_clip.is_some());
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "@");

        // Confirming the highlighted session (alpha) replaces the `@` token with
        // its clip and dismisses both the dialog and the smart-clip search.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.session_picker.is_none());
        let popup = app.program_popup.as_ref().unwrap();
        assert!(
            popup.buffer.starts_with("@{session:a"),
            "inserted alpha's clip, got {:?}",
            popup.buffer
        );
        assert!(
            popup.smart_clip.is_none(),
            "smart-clip search closes once the clip is inserted"
        );
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_left_returns_to_program_menu() {
        let (mut app, _dir, server) = empty_app().await;
        let mut alpha = summary_with_kind(agentd_protocol::SessionKind::User);
        alpha.id = "a".into();
        alpha.title = Some("alpha".into());
        alpha.position = 0;
        let mut beta = summary_with_kind(agentd_protocol::SessionKind::User);
        beta.id = "b".into();
        beta.title = Some("beta".into());
        beta.position = 1;
        app.sessions = vec![alpha, beta];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");

        // Open the `@`→session dialog from the session category (root position 2).
        app.program_popup
            .as_mut()
            .unwrap()
            .smart_clip
            .as_mut()
            .unwrap()
            .selected = 2;
        app.accept_program_smart_clip();
        assert!(app.session_picker_active());

        // Left backs out of the dialog to the inline `@` menu it was opened from
        // — the inverse of the Right/Enter that drilled into it.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));

        // The dialog is gone, but the `@` smart-clip menu is live again, the
        // buffer is untouched, and the "session" category is re-highlighted.
        assert!(!app.session_picker_active(), "Left closes the dialog");
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "@");
        let popup = app.program_popup.as_ref().unwrap();
        let search = popup
            .smart_clip
            .as_ref()
            .expect("the inline `@` menu is live again");
        assert!(matches!(search.view, ProgramSmartClipView::Root));
        let rows = app.program_smart_clip_rows(popup);
        let selectable: Vec<&ProgramSmartClipRow> =
            rows.iter().filter(|r| r.is_selectable()).collect();
        assert!(matches!(
            selectable[search.selected],
            ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Session,
                ..
            }
        ));

        // Right re-opens the dialog: the back-navigation is fully reversible.
        app.program_smart_clip_expand();
        assert!(app.session_picker_active(), "Right re-opens the dialog");
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_clip_variant_filters_from_buffer_typeahead() {
        let (mut app, _dir, server) = empty_app().await;
        let mut alpha = summary_with_kind(agentd_protocol::SessionKind::User);
        alpha.id = "a".into();
        alpha.title = Some("alpha".into());
        alpha.position = 0;
        let mut beta = summary_with_kind(agentd_protocol::SessionKind::User);
        beta.id = "b".into();
        beta.title = Some("beta".into());
        beta.position = 1;
        app.sessions = vec![alpha, beta];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");

        // Open the `@`→session dialog (session category sits at root position 2).
        app.program_popup
            .as_mut()
            .unwrap()
            .smart_clip
            .as_mut()
            .unwrap()
            .selected = 2;
        app.accept_program_smart_clip();
        assert_eq!(
            app.session_picker.as_ref().unwrap().purpose,
            SessionPickerPurpose::InsertProgramClip
        );
        // Empty `@` query: both sessions are bright.
        assert_eq!(
            picker_bright(&app.session_picker_rows()),
            vec!["alpha", "beta"]
        );

        // Typing routes into the buffer's `@<typeahead>` token — not a separate
        // dialog search line — and the dialog re-filters from it.
        picker_type(&mut app, "alph");
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "@alph");
        assert!(
            app.session_picker.as_ref().unwrap().query.is_empty(),
            "the dialog keeps no search line of its own for the `@` variant"
        );
        assert_eq!(picker_bright(&app.session_picker_rows()), vec!["alpha"]);

        // Backspacing edits the same token in place while the `@` survives.
        for _ in 0..4 {
            app.handle_session_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "@");
        assert!(app.session_picker_active(), "still open at the bare `@`");
        assert_eq!(
            picker_bright(&app.session_picker_rows()),
            vec!["alpha", "beta"]
        );

        // Backspacing over the `@` itself removes it and dismisses the picker.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert!(!app.session_picker_active());
        assert_eq!(app.program_popup.as_ref().unwrap().buffer, "");
        assert!(app.program_popup.as_ref().unwrap().smart_clip.is_none());
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_session_submenu_groups_and_dims() {
        let (mut app, _dir, server) = empty_app().await;
        let mut alpha = summary_with_kind(agentd_protocol::SessionKind::User);
        alpha.id = "a".into();
        alpha.title = Some("alpha".into());
        alpha.position = 0;
        let mut beta = summary_with_kind(agentd_protocol::SessionKind::User);
        beta.id = "b".into();
        beta.title = Some("beta".into());
        beta.position = 0;
        beta.group_id = Some("g1".into());
        app.sessions = vec![alpha, beta];
        app.groups = vec![agentd_protocol::GroupSummary {
            id: "g1".into(),
            name: "Proj".into(),
            created_at: chrono::Utc::now(),
            position: 0,
            collapsed: false,
        }];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");
        app.insert_program_text("al");
        app.program_popup
            .as_mut()
            .unwrap()
            .smart_clip
            .as_mut()
            .unwrap()
            .view = ProgramSmartClipView::Submenu(ProgramSmartClipGroup::Session);

        let popup = app.program_popup.as_ref().unwrap();
        let rows = app.program_smart_clip_rows(popup);
        // Ungrouped "alpha" first, then the "Proj" group header, then "beta".
        match &rows[0] {
            ProgramSmartClipRow::Clip { candidate, dimmed } => {
                assert_eq!(candidate.label, "alpha");
                assert!(!dimmed, "query 'al' matches alpha");
            }
            other => panic!("expected alpha clip, got {other:?}"),
        }
        assert!(matches!(&rows[1], ProgramSmartClipRow::Header(h) if h == "Proj"));
        match &rows[2] {
            ProgramSmartClipRow::Clip { candidate, dimmed } => {
                assert_eq!(candidate.label, "beta");
                assert!(
                    dimmed,
                    "query 'al' does not match beta — dimmed, not hidden"
                );
            }
            other => panic!("expected beta clip, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn program_smart_clip_collapse_returns_to_root_on_category() {
        let (mut app, _dir, server) = empty_app().await;
        let mut session = summary_with_kind(agentd_protocol::SessionKind::User);
        session.title = Some("alpha".into());
        app.sessions = vec![session];
        app.harnesses = vec![harness_info("codex", true)];
        app.program_popup = Some(program_popup_for_test("s1", "", 0));
        app.insert_program_text("@");
        app.program_popup
            .as_mut()
            .unwrap()
            .smart_clip
            .as_mut()
            .unwrap()
            .view = ProgramSmartClipView::Submenu(ProgramSmartClipGroup::Harness);

        app.program_smart_clip_collapse();

        let popup = app.program_popup.as_ref().unwrap();
        let search = popup.smart_clip.as_ref().unwrap();
        assert!(matches!(search.view, ProgramSmartClipView::Root));
        // Re-highlights the harness category we backed out of.
        let rows = app.program_smart_clip_rows(popup);
        let selectable: Vec<&ProgramSmartClipRow> =
            rows.iter().filter(|r| r.is_selectable()).collect();
        assert!(matches!(
            selectable[search.selected],
            ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Harness,
                ..
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn program_cursor_moves_over_smart_clip_as_one_unit() {
        let (mut app, _dir, server) = empty_app().await;
        let clip = "@{harness:codex}";
        let before = "a ";
        app.program_popup = Some(program_popup_for_test(
            "s1",
            &format!("{before}{clip} z"),
            before.chars().count(),
        ));

        app.move_program_cursor(1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            before.chars().count() + clip.chars().count()
        );

        app.move_program_cursor(-1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            before.chars().count()
        );

        app.program_popup.as_mut().unwrap().cursor = before.chars().count() + 3;
        app.move_program_cursor(1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            before.chars().count() + clip.chars().count()
        );
        server.abort();
    }

    // Vertical cursor navigation and mouse hit-testing must move through the
    // *visual* (word-wrapped) rows the program body paints, not jump over a whole
    // logical line. With an inner content width of 5 the single-word line
    // "abcdefghij" wraps into two visual rows: "abcde" (offsets 0–4) and "fghij"
    // (offsets 5–9). Nav reads the width from the inner area captured at the last
    // render; hit-testing derives it from the modal rect.

    #[tokio::test]
    async fn program_down_moves_to_next_visual_row_within_logical_line() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdefghij", 2));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 5, 20));

        app.move_program_cursor_vertical(1);

        // Down lands on the wrapped continuation row at the same column (offset
        // 5 + 2 = 7), staying inside the one logical line — not at its end.
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 7);
        server.abort();
    }

    #[tokio::test]
    async fn program_up_from_continuation_row_stays_in_logical_line() {
        let (mut app, _dir, server) = empty_app().await;
        // Cursor on the second visual row ("fghij") at column 2 → offset 7.
        app.program_popup = Some(program_popup_for_test("s1", "abcdefghij", 7));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 5, 20));

        app.move_program_cursor_vertical(-1);

        // Up moves to the first visual row of the *same* logical line (offset 2),
        // rather than doing nothing because there is no logical line above.
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 2);
        server.abort();
    }

    #[tokio::test]
    async fn program_down_from_line_start_with_clip_does_not_land_on_at_sign() {
        // "hello @{session:s1}" renders as "hello  session s1 " (18 visible
        // chars with chip padding). At width 7 the chip's rendering wraps
        // across visual rows 1 and 2 of the logical line. The cursor starts
        // at position 0 (before any text). Pressing Down must not land on the
        // '@' at offset 6 — the '@' is still on visual row 0 (the same row
        // the cursor is on), so landing there means the cursor did not actually
        // move down. It must skip past the clip so it advances to a new position.
        let (mut app, _dir, server) = empty_app().await;
        let clip = "@{session:s1}";
        let markdown = format!("hello {clip}\nnext");
        // cursor starts at 0: beginning of the first line (before "hello")
        app.program_popup = Some(program_popup_for_test("s1", &markdown, 0));
        // width 7 forces "hello  session s1 " to wrap across 3 visual rows
        app.layout.program_inner_area = Some(Rect::new(2, 2, 7, 20));

        app.move_program_cursor_vertical(1);

        let at_sign_offset = "hello ".chars().count(); // 6
        assert_ne!(
            app.program_popup.as_ref().unwrap().cursor,
            at_sign_offset,
            "Down from line start must not land on the '@' of a clip (cursor would not have moved down)"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_a_then_ctrl_f_does_not_skip_list_marker() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* alpha beta gamma", 0));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 5, 12));

        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            2,
            "Ctrl-A must jump to list content, not the list marker"
        );

        app.handle_program_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            3,
            "first Ctrl-F must move to the first content char, not jump over it"
        );

        app.handle_program_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            4,
            "single-step right should behave like Ctrl-F from list content start"
        );

        server.abort();
    }

    #[tokio::test]
    async fn program_horizontal_motion_skips_hidden_list_marker_offsets() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* alpha", 0));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 6, 12));

        app.move_program_cursor(1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            3,
            "right from the rendered list start should not stop inside hidden '* '"
        );

        app.move_program_cursor(-1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            2,
            "left from the first content char should land on content start, not hidden marker"
        );

        app.move_program_cursor(-1);
        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            2,
            "left at list content start should stay visible instead of entering '* '"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_click_on_list_prefix_normalizes_to_content_start() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* alpha beta gamma", 0));
        let modal = Rect::new(0, 0, 9, 20);

        // Click leftmost painted column on the first row. Without normalization
        // this resolves to cursor 0 (inside the hidden '* ' marker), then
        // moves with the next keypress.
        app.place_program_cursor(modal, 1, 2);

        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            2,
            "clicking list marker area should land on list content start"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_click_on_wrapped_list_continuation_row_maps_to_offset() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* alpha beta gamma", 0));
        let modal = Rect::new(0, 0, 9, 20);

        // inner width = 9 - 2 border - 2 pad = 6.
        // This line wraps and places continuation rows; clicking continuation
        // row 2 must stay inside the list content, not land before marker.
        app.place_program_cursor(modal, 2, 3);
        assert!(
            app.program_popup.as_ref().unwrap().cursor >= 2,
            "wrapped list hit-testing should remain within list content"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_ctrl_a_from_wrapped_list_continuation_row_preserves_content_start() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* alpha beta gamma", 0));
        let modal = Rect::new(0, 0, 9, 20);

        app.place_program_cursor(modal, 2, 3);
        app.handle_program_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL))
            .await;

        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            2,
            "Ctrl-A must land on list content even when cursor starts on wrapped row"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_down_moves_between_wrapped_rows_inside_list_content() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "* abcdefghij", 4));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 5, 20));

        app.move_program_cursor_vertical(1);

        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            9,
            "Down should preserve the content column across wrapped bullet rows"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_right_skips_collapsed_wrap_space() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcd efgh", 4));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 4, 20));

        app.move_program_cursor(1);

        assert_eq!(
            app.program_popup.as_ref().unwrap().cursor,
            6,
            "Right should skip word-wrap break whitespace that does not occupy a painted cell"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_cursor_position_matches_painted_wrapped_list_content() {
        let (app, _dir, server) = empty_app().await;
        let markdown = "* alpha beta gamma";
        let cursor = "* alpha ".chars().count();
        let width = 6u16;
        let (row, col) =
            crate::ui::program_cursor_visual_pos(Some(&app), markdown, cursor, width as usize);

        let backend = ratatui::backend::TestBackend::new(width, 6);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            let lines = crate::ui::render_program_markdown_lines_for_test(&app, markdown);
            let para = ratatui::widgets::Paragraph::new(lines)
                .wrap(ratatui::widgets::Wrap { trim: false });
            f.render_widget(para, Rect::new(0, 0, width, 6));
        })
        .expect("draw");
        let glyph = term
            .backend()
            .buffer()
            .cell((col as u16, row as u16))
            .map(|c| c.symbol().to_string())
            .unwrap_or_default();

        assert_eq!(
            glyph, "b",
            "computed cursor ({row}, {col}) should sit on painted wrapped list content"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_cursor_advances_when_space_appended_to_list_item() {
        let (app, _dir, server) = empty_app().await;
        let width = 40u16;

        // A bullet line and the same line after a trailing space is typed at the
        // end. The buffer offset moves by one char, so the rendered cursor column
        // must move by one too — otherwise the caret desyncs from the edit point.
        let before = "* foo";
        let after = "* foo ";
        let (_, col_before) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            before,
            before.chars().count(),
            width as usize,
        );
        let (_, col_after) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            after,
            after.chars().count(),
            width as usize,
        );

        assert_eq!(
            col_after,
            col_before + 1,
            "appending a trailing space to a list item must advance the cursor column by one"
        );

        // The cursor must also land exactly at the painted end of the rendered
        // bullet line ("  • foo ", 8 wide), with no gap between the text and the
        // caret — i.e. the trailing space is actually rendered.
        assert_eq!(
            col_after, 8,
            "cursor should sit immediately after the rendered trailing space"
        );

        server.abort();
    }

    #[tokio::test]
    async fn program_cursor_advances_when_space_appended_to_heading() {
        let (app, _dir, server) = empty_app().await;
        let width = 40u16;

        // A heading line and the same line after a trailing space is typed at the
        // end. The buffer offset moves by one char, so the rendered cursor column
        // must move by one too — otherwise the caret desyncs from the edit point.
        // Headings paint their `#` markers literally, so "## foo" renders 6 wide.
        let before = "## foo";
        let after = "## foo ";
        let (_, col_before) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            before,
            before.chars().count(),
            width as usize,
        );
        let (_, col_after) = crate::ui::program_cursor_visual_pos(
            Some(&app),
            after,
            after.chars().count(),
            width as usize,
        );

        assert_eq!(
            col_after,
            col_before + 1,
            "appending a trailing space to a heading must advance the cursor column by one"
        );

        // The cursor must land exactly at the painted end of the rendered heading
        // ("## foo ", 7 wide), with no gap between the text and the caret — i.e.
        // the trailing space is actually rendered.
        assert_eq!(
            col_after, 7,
            "cursor should sit immediately after the rendered trailing space"
        );

        server.abort();
    }

    #[tokio::test]
    async fn program_vertical_nav_preserves_preferred_column_across_wrap() {
        let (mut app, _dir, server) = empty_app().await;
        // "abcdefghij" wraps to two visual rows; "XY" is a short line below it.
        app.program_popup = Some(program_popup_for_test("s1", "abcdefghij\nXY", 3));
        app.layout.program_inner_area = Some(Rect::new(2, 2, 5, 20));

        // Down crosses the wrapped boundary: row 1 of the logical line, col 3.
        app.move_program_cursor_vertical(1);
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 8);
        assert_eq!(app.program_popup.as_ref().unwrap().preferred_col, Some(3));

        // Down again onto the short "XY" line clamps to its end (offset 13) but
        // the preferred column is remembered, not overwritten by the clamp.
        app.move_program_cursor_vertical(1);
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 13);
        assert_eq!(app.program_popup.as_ref().unwrap().preferred_col, Some(3));

        // Up returns to the long row and restores column 3 (offset 8).
        app.move_program_cursor_vertical(-1);
        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 8);
        assert_eq!(app.program_popup.as_ref().unwrap().preferred_col, Some(3));
        server.abort();
    }

    #[tokio::test]
    async fn program_click_on_wrapped_continuation_row_maps_to_offset() {
        let (mut app, _dir, server) = empty_app().await;
        app.program_popup = Some(program_popup_for_test("s1", "abcdefghij", 0));
        // inner content origin = (modal.x + 1 + pad, modal.y + 1 + pad) = (2, 2);
        // inner width = 9 - 2 border - 2 pad = 5. "fghij" paints at y = 3.
        let modal = Rect::new(0, 0, 9, 20);

        // Click column 2 of the continuation row (screen col 4, row 3) → offset 7.
        app.place_program_cursor(modal, 4, 3);

        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 7);
        server.abort();
    }

    #[tokio::test]
    async fn program_click_maps_through_scroll_offset_on_wrapped_row() {
        let (mut app, _dir, server) = empty_app().await;
        // Three logical lines; the first wraps to two visual rows (0,1), then
        // "second" is row 2 and "third" is row 3. Scroll one wrapped row off the
        // top so the viewport starts at visual row 1.
        let markdown = "abcdefghij\nsecond\nthird";
        app.program_popup = Some(program_popup_for_test("s1", markdown, 0));
        app.program_popup.as_mut().unwrap().scroll_offset = 1;
        let modal = Rect::new(0, 0, 9, 20);

        // Click the top visible screen row (y = 2). With scroll 1 that is visual
        // row 1 = "fghij" col 0 → offset 5, not the unscrolled row 0.
        app.place_program_cursor(modal, 2, 2);

        assert_eq!(app.program_popup.as_ref().unwrap().cursor, 5);
        server.abort();
    }

    #[test]
    fn program_normalizes_missing_and_duplicate_smart_clip_instance_ids() {
        let normalized = program_normalize_smart_clip_instance_ids(
            "a @{harness:codex} b @{harness:claude clip_id=clip_7} c @{harness:codex clip_id=clip_7}",
        );

        assert_eq!(
            normalized,
            "a @{harness:codex clip_id=clip_8} b @{harness:claude clip_id=clip_7} c @{harness:codex clip_id=clip_9}"
        );
    }

    #[test]
    fn program_line_start_skips_list_markers_for_cursor_commands() {
        assert_eq!(program_line_start("- alpha", 0), 2);
        assert_eq!(program_line_start("* alpha", 4), 2);
        assert_eq!(program_line_start("  - alpha", 0), 4);
        assert_eq!(program_line_start("- alpha", 7), 2);
        assert_eq!(program_line_start("plain", 0), 0);
    }

    #[tokio::test]
    async fn program_delete_removes_smart_clip_as_one_unit() {
        let (mut app, _dir, server) = empty_app().await;
        let clip = "@{harness:codex}";
        let before = "a ";
        let initial = format!("{before}{clip} z");
        app.program_popup = Some(program_popup_for_test(
            "s1",
            &initial,
            before.chars().count() + clip.chars().count(),
        ));

        app.delete_program_back();
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "a  z");
        assert_eq!(popup.cursor, before.chars().count());

        app.program_popup = Some(program_popup_for_test(
            "s1",
            &initial,
            before.chars().count(),
        ));
        app.delete_program_forward();
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "a  z");
        assert_eq!(popup.cursor, before.chars().count());

        app.program_popup = Some(program_popup_for_test(
            "s1",
            &initial,
            before.chars().count() + 3,
        ));
        app.delete_program_forward();
        let popup = app.program_popup.as_ref().unwrap();
        assert_eq!(popup.buffer, "a  z");
        assert_eq!(popup.cursor, before.chars().count());
        server.abort();
    }

    #[test]
    fn prune_window_tree_replaces_stale_sessions_and_preserves_splits() {
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        let tree = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 35,
            first: Box::new(MainWindowTree::Leaf {
                id: 4,
                selection: Selection::Session("missing".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 7,
                selection: Selection::Session("s2".into()),
            }),
        };

        let restored = prune_window_tree(tree, &[s2], &[], &Selection::Session("s2".into()));

        match restored {
            MainWindowTree::Split {
                ratio_percent,
                first,
                second,
                ..
            } => {
                assert_eq!(ratio_percent, 35);
                assert_eq!(
                    first.find_selection(4),
                    Some(&Selection::Session("s2".into()))
                );
                assert_eq!(
                    second.find_selection(7),
                    Some(&Selection::Session("s2".into()))
                );
                assert_eq!(second.max_id(), 7);
            }
            MainWindowTree::Leaf { .. } => panic!("split layout should be preserved"),
        }
    }

    #[tokio::test]
    async fn main_window_sessions_needing_hydration_includes_inactive_splits() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.has_pty = true;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.histories
            .insert("s1".into(), crate::pty_render::ItemHistory::new());

        assert_eq!(app.main_window_sessions_needing_hydration(), vec!["s2"]);
        server.abort();
    }

    #[test]
    fn program_referenced_session_ids_extracts_session_clips_only() {
        // Session clips are collected (deduped, in order); harness clips and
        // other kinds are ignored, and a `clip_id=` suffix does not split the id.
        let md = "Build @{session:s3} then @{harness:codex} and @{session:s3 clip_id=4} \
                  finally @{session:s7}";
        assert_eq!(
            program_referenced_session_ids(md),
            vec!["s3".to_string(), "s7".to_string()]
        );
        assert!(program_referenced_session_ids("no clips here").is_empty());
    }

    #[tokio::test]
    async fn program_referenced_sessions_need_hydration_for_hover_preview() {
        // A program shown in a main-window leaf references a worker session that
        // is neither selected, pinned, nor the orchestrator. Its PTY history
        // must be hydrated so the program hover preview (spec 0060) can paint a
        // live terminal tail instead of degrading to the bare text tooltip.
        let (mut app, _dir, server) = empty_app().await;
        let mut owner = summary_with_kind(agentd_protocol::SessionKind::User);
        owner.id = "s1".into();
        owner.has_pty = true;
        let mut worker = summary_with_kind(agentd_protocol::SessionKind::User);
        worker.id = "s3".into();
        worker.has_pty = true;
        // A second referenced session with no PTY can't be previewed, so it must
        // not be queued for hydration.
        let mut no_pty = summary_with_kind(agentd_protocol::SessionKind::User);
        no_pty.id = "s4".into();
        no_pty.has_pty = false;
        app.sessions = vec![owner, worker, no_pty];
        app.main_windows = MainWindowTree::Leaf {
            id: 1,
            selection: Selection::Session("s1".into()),
        };
        app.program_popups.insert(
            "s1".into(),
            program_popup_for_test(
                "s1",
                "Build the PR @{session:s3}\nDocs @{session:s4}\nUnknown @{session:s9}",
                0,
            ),
        );

        // s3 has a previewable PTY and no warm history yet → queue it. s4 has no
        // PTY and s9 is unknown → skip both.
        assert_eq!(
            app.program_referenced_sessions_needing_hydration(),
            vec!["s3".to_string()]
        );

        // Once its history is warm, it drops out of the queue (idempotent).
        app.histories
            .insert("s3".into(), crate::pty_render::ItemHistory::new());
        assert!(app
            .program_referenced_sessions_needing_hydration()
            .is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn delete_active_window_preserves_remaining_nested_splits() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Below,
                ratio_percent: 40,
                first: Box::new(MainWindowTree::Leaf {
                    id: 2,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 3,
                    selection: Selection::Session("s1".into()),
                }),
            }),
        };
        app.active_window_id = 3;

        app.delete_active_window();

        match &app.main_windows {
            MainWindowTree::Split { first, second, .. } => {
                assert!(matches!(first.as_ref(), MainWindowTree::Leaf { id: 1, .. }));
                assert!(matches!(
                    second.as_ref(),
                    MainWindowTree::Leaf { id: 2, .. }
                ));
            }
            MainWindowTree::Leaf { .. } => panic!("expected two remaining split panes"),
        }
        assert_eq!(app.leaf_window_ids(), vec![1, 2]);
        server.abort();
    }

    /// The session-title menu paints over the pane content. When that pane's
    /// child has grabbed the mouse (claude-code / vim fullscreen), clicks on the
    /// menu's action rows must still drive the menu — they must NOT be forwarded
    /// into the child. Regression for split / close / rename silently doing
    /// nothing on a pane whose harness tracks the mouse.
    #[tokio::test]
    async fn session_menu_action_dispatches_over_mouse_grabbing_child() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = MainWindowTree::single(1, Selection::Session("s1".into()));
        app.active_window_id = 1;
        app.next_window_id = 2;
        app.selection = Selection::Session("s1".into());
        app.focus = PaneFocus::View;

        // Give s1 a child that has enabled mouse tracking (DECSET ?1000h).
        let mut hist = crate::pty_render::ItemHistory::new();
        hist.feed_pty(b"\x1b[?1000h");
        let _ = hist.replay(58, 26, 0);
        assert_ne!(
            hist.mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "precondition: the child grabs the mouse"
        );
        app.histories.insert("s1".into(), hist);

        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("render");

        let pane = app
            .layout
            .main_window_areas
            .first()
            .copied()
            .expect("single pane registered");
        let (bx, _, by) = crate::ui::view_close_button_range(pane.area);
        let click = |kind, col, row| MouseEvent {
            kind,
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        };

        // Click the ☰ button (on the border, never forwarded) to open the menu.
        app.on_mouse(click(MouseEventKind::Down(MouseButton::Left), bx + 1, by))
            .await;
        app.on_mouse(click(MouseEventKind::Up(MouseButton::Left), bx + 1, by))
            .await;
        let menu = app
            .session_title_menu
            .clone()
            .expect("clicking the actions button opens the session menu");

        // Click the "split horizontal" row, which sits over the pane content the
        // child is tracking. With the fix it dispatches; without it, the click
        // is swallowed by the child and the layout never splits.
        let idx = SessionTitleMenuAction::ALL
            .iter()
            .position(|a| *a == SessionTitleMenuAction::SplitHorizontal)
            .unwrap();
        let item_row = menu.area.y + 1 + idx as u16;
        let item_col = menu.area.x + 2;
        assert_eq!(
            menu.item_at(item_col, item_row),
            Some(SessionTitleMenuAction::SplitHorizontal)
        );
        app.on_mouse(click(
            MouseEventKind::Down(MouseButton::Left),
            item_col,
            item_row,
        ))
        .await;
        app.on_mouse(click(
            MouseEventKind::Up(MouseButton::Left),
            item_col,
            item_row,
        ))
        .await;

        assert!(
            app.is_split_layout(),
            "menu split action must fire even over a mouse-grabbing child"
        );
        assert_eq!(app.leaf_window_ids().len(), 2);
        server.abort();
    }

    #[tokio::test]
    async fn set_split_ratio_by_render_order_updates_nested_split() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Below,
                ratio_percent: 40,
                first: Box::new(MainWindowTree::Leaf {
                    id: 2,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 3,
                    selection: Selection::Session("s1".into()),
                }),
            }),
        };

        assert!(app.set_split_ratio_by_order(2, 65));

        match &app.main_windows {
            MainWindowTree::Split {
                ratio_percent,
                second,
                ..
            } => {
                assert_eq!(*ratio_percent, 50);
                match second.as_ref() {
                    MainWindowTree::Split { ratio_percent, .. } => assert_eq!(*ratio_percent, 65),
                    MainWindowTree::Leaf { .. } => panic!("expected nested split"),
                }
            }
            MainWindowTree::Leaf { .. } => panic!("expected root split"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn split_windows_track_individual_pty_sizes() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.window_pane_sizes.insert(1, (38, 18));
        app.window_pane_sizes.insert(2, (28, 18));
        app.active_window_id = 2;

        assert_eq!(app.active_pane_size(), (28, 18));
        assert_eq!(
            app.window_session_pane_sizes(),
            vec![("s1".to_string(), (38, 18)), ("s2".to_string(), (28, 18))]
        );

        // Each session resolves to its OWN pane size, not the active pane's.
        // s2 is active (28×18); the passive pane s1 must still report 38×18 so
        // hydration builds its parser at the width it renders at — otherwise an
        // alt-screen child garbles until a focus-switch re-hydrates it.
        assert_eq!(app.session_pane_size("s1"), Some((38, 18)));
        assert_eq!(app.session_pane_size("s2"), Some((28, 18)));
        // A session not in any visible pane has no pane size (callers fall back
        // to the active pane).
        assert_eq!(app.session_pane_size("nope"), None);
        server.abort();
    }

    /// Regression: `C-x t` (ToggleView) must flip only the focused split's
    /// transcript/terminal mode, not every split. View mode is per-window
    /// (`window_views`), and each pane is remembered across focus changes.
    #[tokio::test]
    async fn transcript_toggle_is_scoped_to_focused_split() {
        let (mut app, _dir, server) = captured_app().await;
        // Two PTY-backed sessions side by side; both default to Terminal.
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.has_pty = true;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.view = ViewMode::Terminal;

        // Both panes start in their natural (Terminal) mode.
        assert_eq!(app.view_for_window(Some(1)), ViewMode::Terminal);
        assert_eq!(app.view_for_window(Some(2)), ViewMode::Terminal);

        // Toggle transcript mode on the focused split (window 1).
        app.run_action(KeyAction::ToggleView).await;

        // Only the focused split flips to Chat; the sibling stays Terminal.
        assert_eq!(app.view, ViewMode::Chat);
        assert_eq!(app.view_for_window(Some(1)), ViewMode::Chat);
        assert_eq!(
            app.view_for_window(Some(2)),
            ViewMode::Terminal,
            "C-x t must not change the unfocused split's view mode"
        );

        // Focus the sibling: it shows its own natural Terminal mode, while
        // window 1 keeps the Chat mode it was toggled to.
        app.focus_main_window(2);
        assert_eq!(app.active_window_id, 2);
        assert_eq!(app.view, ViewMode::Terminal);
        assert_eq!(app.view_for_window(Some(1)), ViewMode::Chat);

        // Toggling window 2 flips only it; window 1 is untouched.
        app.run_action(KeyAction::ToggleView).await;
        assert_eq!(app.view_for_window(Some(2)), ViewMode::Chat);
        assert_eq!(app.view_for_window(Some(1)), ViewMode::Chat);

        // Refocusing window 1 restores its remembered Chat mode.
        app.focus_main_window(1);
        assert_eq!(app.view, ViewMode::Chat);

        server.abort();
    }

    /// Regression: a passive split pane whose size changed without the active
    /// pane changing must still trigger a PTY resize. The event loop gates the
    /// resize fire on the active pane's size, so the divergence check is what
    /// catches the passive pane — otherwise its child renders at a stale width
    /// and garbles until a manual window resize. See the diagnosis in the split
    /// pane garble investigation.
    #[test]
    fn pane_size_divergence_catches_passive_pane() {
        let visible = vec![
            ("s1".to_string(), (38u16, 18u16)),
            ("s2".to_string(), (28u16, 18u16)),
        ];

        // Nothing sent yet → everything diverges.
        let mut last_sent: HashMap<String, (u16, u16)> = HashMap::new();
        assert!(pane_sizes_diverged(&visible, &last_sent));

        // Both panes at their last-sent sizes → quiescent.
        last_sent.insert("s1".to_string(), (38, 18));
        last_sent.insert("s2".to_string(), (28, 18));
        assert!(!pane_sizes_diverged(&visible, &last_sent));

        // The *passive* pane (s1) shrinks while the active pane (s2) is
        // unchanged. The active-pane gate would miss this; the divergence
        // check must not.
        let after_drag = vec![
            ("s1".to_string(), (20u16, 18u16)),
            ("s2".to_string(), (28u16, 18u16)),
        ];
        assert!(pane_sizes_diverged(&after_drag, &last_sent));
    }

    /// Regression: in a single-pane (non-split) layout the render path still
    /// goes through `render_main_windows`, which always passes
    /// `Some(window_id)` to `render_terminal_for_window`. The scroll handlers
    /// must therefore key on `Some(active_window_id)` too — keying on `None`
    /// when not split (the previous behaviour) wrote to `view_scrollback` but
    /// the render read `window_scrollback[active_window_id]`, so the
    /// scrollbar appeared but the viewport never moved.
    #[tokio::test]
    async fn scroll_in_single_pane_updates_what_render_reads() {
        let (mut app, _dir, server) = captured_app().await;
        app.selection = Selection::Session("s1".into());
        app.view = ViewMode::Terminal;
        assert!(
            !app.is_split_layout(),
            "captured_app must start non-split for this regression"
        );
        let win = app.active_window_id;

        app.adjust_scrollback(10);

        // Both views of the offset must agree, since the render path passes
        // `Some(active_window_id)` even in single-pane mode while the zoomed
        // render path reads `view_scrollback` directly.
        assert_eq!(app.scrollback_for_window(Some(win)), 10);
        assert_eq!(app.view_scrollback, 10);
        assert_eq!(app.scrollback_for_window(None), 10);
        server.abort();
    }

    #[tokio::test]
    async fn mouse_scrollback_targets_only_hovered_split() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.has_pty = true;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.view = ViewMode::Terminal;
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(20, 0, 40, 20),
                inner_area: Rect::new(21, 1, 38, 18),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(60, 0, 40, 20),
                inner_area: Rect::new(61, 1, 38, 18),
            },
        ];

        app.adjust_mouse_scrollback(65, 5, 10);

        assert_eq!(app.active_window_id, 2);
        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(app.scrollback_for_window(Some(1)), 0);
        assert_eq!(app.scrollback_for_window(Some(2)), 10);
        assert_eq!(app.view_scrollback, 10);

        // The scrollbar overlay must reveal only on the scrolled pane (2), not
        // its at-bottom sibling (1). Keying the reveal timer per window is what
        // keeps a sibling split from flashing a scrollbar it isn't scrolling.
        assert!(
            app.terminal_scrollbar_visible_until(Some(2)).is_some(),
            "scrolled split should reveal its own scrollbar overlay"
        );
        assert!(
            app.terminal_scrollbar_visible_until(Some(1)).is_none(),
            "at-bottom sibling split must not reveal a scrollbar overlay"
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_open_state_survives_split_focus_changes() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.has_pty = true;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.focus = PaneFocus::View;
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(20, 0, 40, 20),
                inner_area: Rect::new(21, 1, 38, 18),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(60, 0, 40, 20),
                inner_area: Rect::new(61, 1, 38, 18),
            },
        ];
        app.layout.modal_area = Some(Rect::new(20, 0, 40, 20));
        app.program_popup = Some(program_popup_for_test("s1", "draft", 3));
        app.set_program_terminal_focus(true);
        {
            let popup = app.program_popup.as_mut().expect("active s1 program");
            popup.slide_from = 1.0;
            popup.slide_changed_at = Some(
                Instant::now() - Duration::from_millis(PROGRAM_REVEAL_MS),
            );
        }

        app.handle_left_click(65, 5).await;

        assert_eq!(app.active_window_id, 2);
        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert!(app.program_popup.is_none());
        assert!(
            app.program_popups.contains_key("s1"),
            "clicking another split should stash, not close, the open program"
        );
        let stashed = app.program_popups.get("s1").expect("stashed s1 program");
        assert!(
            stashed.terminal_focus,
            "focusing another split must not unslide split 1's Program"
        );
        assert_eq!(
            stashed.slide_fraction(Instant::now()),
            1.0,
            "stashed Program should keep its slid position"
        );

        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.sync_program_popup_with_selection();
        let restored = app.program_popup.as_ref().expect("restored s1 program");
        assert!(
            restored.terminal_focus,
            "returning to split 1 should restore the slid Program state"
        );

        app.run_action(KeyAction::SwitchFocus).await;
        app.sync_program_popup_with_selection();

        assert_eq!(app.active_window_id, 2);
        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert!(app.program_popup.is_none());
        assert!(
            app.program_popups.contains_key("s1"),
            "C-x o should keep split 1's program attached to split 1's session"
        );
        let stashed = app.program_popups.get("s1").expect("stashed s1 program");
        assert!(
            stashed.terminal_focus,
            "C-x o to another split must not reset split 1's slide state"
        );
        server.abort();
    }

    #[tokio::test]
    async fn mouse_list_click_updates_active_split_selection() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 2;
        app.layout = test_layout();
        app.layout.list_row_count = app.list_items().len();
        app.layout.list_items_area = Some(Rect::new(1, 1, 18, 8));

        app.click_list(Rect::new(0, 0, 20, 10), 5, 2).await;

        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(2),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s1".into()))
        );
        server.abort();
    }

    #[tokio::test]
    async fn program_stays_open_when_clicking_selected_session_in_list() {
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.position = 0;
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.position = 1;
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "draft", 3));
        app.layout = test_layout();
        app.layout.list_row_count = app.list_items().len();
        app.layout.list_items_area = Some(Rect::new(1, 1, 18, 8));
        // The program modal sits over the view pane; the click lands in the list.
        app.layout.modal_area = Some(Rect::new(20, 0, 40, 20));

        // Click the already-selected session's row in the list.
        app.handle_left_click(10, 1).await;

        // A list click neither closes nor hides the program of the selected
        // session — only the title-glyph toggle / C-x Space do.
        let popup = app
            .program_popup
            .as_ref()
            .expect("program stays open after a list click");
        assert!(
            !popup.closing,
            "list click must not start closing the program"
        );
        assert_eq!(popup.buffer, "draft");
        server.abort();
    }

    #[tokio::test]
    async fn clicking_another_session_in_list_stashes_program_not_closes() {
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.position = 0;
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.position = 1;
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());
        app.program_popup = Some(program_popup_for_test("s1", "draft", 3));
        app.layout = test_layout();
        app.layout.list_row_count = app.list_items().len();
        app.layout.list_items_area = Some(Rect::new(1, 1, 18, 8));
        app.layout.modal_area = Some(Rect::new(20, 0, 40, 20));

        // Click a *different* session's row in the list.
        app.handle_left_click(10, 2).await;

        assert_eq!(app.selection, Selection::Session("s2".into()));
        // The prior session's program is preserved (stashed), not destroyed —
        // it reappears on return. A list click is never a close gesture.
        let stashed = app
            .program_popups
            .get("s1")
            .expect("the prior session's program is stashed, not closed");
        assert!(
            !stashed.closing,
            "a list click must never close the program"
        );
        assert_eq!(stashed.buffer, "draft");
        server.abort();
    }

    // End-to-end guard for the #488 fix: a session-list click while a program
    // is open must switch sessions. The sibling tests
    // (`clicking_another_session_in_list_stashes_program_not_closes`, …) call
    // `handle_left_click` directly, which skips the `on_mouse` →
    // `handle_program_mouse` dispatch that decides whether the program swallows
    // the click. This drives a real render (so `modal_area` / `list_items_area`
    // are live geometry, not hand-set) and the full Down/Up mouse path, so a
    // regression that re-swallows outside clicks would be caught here even if
    // `handle_left_click` stayed correct in isolation.
    #[tokio::test]
    async fn on_mouse_list_click_switches_session_with_program_open() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let (mut app, _dir, server) = empty_app().await;
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.position = 0;
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.position = 1;
        app.sessions = vec![s1, s2];
        app.selection = Selection::Session("s1".into());
        app.main_windows = MainWindowTree::single(1, Selection::Session("s1".into()));
        app.active_window_id = 1;
        app.program_popup = Some(program_popup_for_test("s1", "draft", 3));

        // Render the real layout so modal_area / list_items_area reflect the
        // live geometry the mouse handler will see.
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("render");

        let items_area = app.layout.list_items_area.expect("list items area");
        // s2 is the second session row (s1 at +0, s2 at +1). Click past the
        // pin/status gutter so it registers as a select, not a pin toggle.
        let row = items_area.y + 1;
        let col = items_area.x + items_area.width / 2;

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: col,
            row,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        // The click switched sessions through the full dispatch path...
        assert_eq!(
            app.selection,
            Selection::Session("s2".into()),
            "clicking s2 in the list while a program is open must switch to s2"
        );
        // ...and stashed s1's program rather than destroying it (navigation
        // never closes a program; only the toggle / C-x Space do).
        assert!(
            app.program_popups.contains_key("s1"),
            "s1's program must be stashed, not discarded, when the click switches away"
        );
        server.abort();
    }

    #[tokio::test]
    async fn mouse_list_click_swaps_session_already_visible_in_another_split() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());
        app.layout = test_layout();
        app.layout.list_row_count = app.list_items().len();
        app.layout.list_items_area = Some(Rect::new(1, 1, 18, 8));
        app.window_scrollback.insert(2, 8);

        app.click_list(Rect::new(0, 0, 20, 10), 5, 2).await;

        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(
            app.selection_for_window(2),
            Some(Selection::Session("s1".into()))
        );
        assert_eq!(app.scrollback_for_window(Some(2)), 0);
        server.abort();
    }

    #[tokio::test]
    async fn switch_session_swaps_session_already_visible_in_another_split() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 1;
        app.selection = Selection::Session("s1".into());

        app.select_session("s2".into());
        app.sync_active_window_selection();

        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(
            app.selection_for_window(2),
            Some(Selection::Session("s1".into()))
        );
        server.abort();
    }

    #[tokio::test]
    async fn focus_neighbor_updates_split_leaf_when_selected_session_disappears() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        second.has_pty = false;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 2;
        app.selection = Selection::Session("s1".into());
        app.view = ViewMode::Terminal;
        app.transcript_session = Some("s1".into());
        app.transcript.push(TimestampedEvent {
            seq: 1,
            at: chrono::Utc::now(),
            event: SessionEvent::Done { exit_code: 0 },
        });
        app.window_scrollback.insert(2, 10);

        app.focus_neighbor_of("s1");

        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(
            app.selection_for_window(2),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(app.view, ViewMode::Chat);
        assert!(app.transcript.is_empty());
        assert_eq!(app.transcript_session, None);
        assert_eq!(app.scrollback_for_window(Some(2)), 0);
        server.abort();
    }

    #[tokio::test]
    async fn focus_neighbor_replaces_inactive_split_without_stealing_focus() {
        let (mut app, _dir, server) = captured_app().await;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s2".into()),
            }),
        };
        app.active_window_id = 2;
        app.selection = Selection::Session("s2".into());

        app.focus_neighbor_of("s1");

        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s2".into()))
        );
        assert_eq!(
            app.selection_for_window(2),
            Some(Selection::Session("s2".into()))
        );
        server.abort();
    }

    #[tokio::test]
    async fn mouse_pin_strip_click_focuses_tile_without_changing_main_window_or_glitching() {
        let (mut app, _dir, server) = captured_app().await;
        app.sessions[0].pinned = true;
        let mut second = summary_with_kind(agentd_protocol::SessionKind::User);
        second.id = "s2".into();
        second.position = 1;
        second.pinned = true;
        app.sessions.push(second);
        app.main_windows = MainWindowTree::single(1, Selection::Session("s1".into()));
        app.active_window_id = 1;
        app.layout = test_layout();
        app.session_transitions.clear();

        // Second tile in an 80-cell, two-tile pin strip starts at x=60; click
        // inside its body, not on the top-border unpin diamond.
        app.click_pin_strip(Rect::new(20, 20, 80, 8), 62, 22).await;

        assert_eq!(app.focus, PaneFocus::View);
        assert_eq!(app.selection, Selection::Session("s2".into()));
        assert_eq!(
            app.selection_for_window(1),
            Some(Selection::Session("s1".into())),
            "pin-strip clicks focus the tile for input without replacing the main pane"
        );
        assert!(
            app.session_transitions.is_empty(),
            "clicking a live pinned preview should not paint the main-pane glitch overlay"
        );
        server.abort();
    }

    #[tokio::test]
    async fn switch_focus_cycles_list_then_first_split_window() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 2;
        app.focus = PaneFocus::List;
        app.session_transitions.clear();

        app.run_action(KeyAction::SwitchFocus).await;

        assert_eq!(app.focus, PaneFocus::View);
        assert_eq!(app.active_window_id, 1);
        assert!(
            app.session_transitions.is_empty(),
            "window focus changes must not glitch"
        );
        server.abort();
    }

    /// A 3-leaf window tree: `[1, 2, 3]` in `leaf_window_ids` order.
    fn three_window_tree() -> MainWindowTree {
        MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Right,
                ratio_percent: 50,
                first: Box::new(MainWindowTree::Leaf {
                    id: 2,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 3,
                    selection: Selection::Session("s1".into()),
                }),
            }),
        }
    }

    #[tokio::test]
    async fn c_digit_focuses_pane_by_index() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = three_window_tree();
        app.focus = PaneFocus::List;

        // C-2 -> first split window, C-3 -> second, C-4 -> third.
        assert!(app.focus_pane_by_index(1));
        assert_eq!(app.focus, PaneFocus::View);
        assert_eq!(app.active_window_id, 1);

        assert!(app.focus_pane_by_index(2));
        assert_eq!(app.active_window_id, 2);

        assert!(app.focus_pane_by_index(3));
        assert_eq!(app.active_window_id, 3);

        // C-5 with only three windows is a no-op — focus stays put.
        assert!(!app.focus_pane_by_index(4));
        assert_eq!(app.active_window_id, 3);
        assert_eq!(app.focus, PaneFocus::View);
        server.abort();
    }

    #[tokio::test]
    async fn focus_pane_index_zero_is_the_list() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = three_window_tree();
        app.focus = PaneFocus::View;
        app.active_window_id = 2;

        // Pane 1 (index 0) is the session list.
        assert!(app.focus_pane_by_index(0));
        assert_eq!(app.focus, PaneFocus::List);
        server.abort();
    }

    #[tokio::test]
    async fn shift_arrow_focuses_spatially_adjacent_window() {
        let (mut app, _dir, server) = captured_app().await;
        // A 2x2 grid of panes:
        //   w1 (top-left)    w2 (top-right)
        //   w3 (bottom-left) w4 (bottom-right)
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(0, 0, 40, 10),
                inner_area: Rect::new(1, 1, 38, 8),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(40, 0, 40, 10),
                inner_area: Rect::new(41, 1, 38, 8),
            },
            WindowPaneHit {
                id: 3,
                area: Rect::new(0, 10, 40, 10),
                inner_area: Rect::new(1, 11, 38, 8),
            },
            WindowPaneHit {
                id: 4,
                area: Rect::new(40, 10, 40, 10),
                inner_area: Rect::new(41, 11, 38, 8),
            },
        ];

        // From the top-left pane: right -> w2, down -> w3, no up/left neighbor.
        app.active_window_id = 1;
        assert_eq!(app.adjacent_window_id(FocusDir::Right), Some(2));
        assert_eq!(app.adjacent_window_id(FocusDir::Down), Some(3));
        assert_eq!(app.adjacent_window_id(FocusDir::Up), None);
        assert_eq!(app.adjacent_window_id(FocusDir::Left), None);

        // From the bottom-right pane: left -> w3, up -> w2.
        app.active_window_id = 4;
        assert_eq!(app.adjacent_window_id(FocusDir::Left), Some(3));
        assert_eq!(app.adjacent_window_id(FocusDir::Up), Some(2));

        // The mutating wrapper actually moves focus, and reports no-op moves.
        app.active_window_id = 1;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        assert!(app.focus_adjacent_window(FocusDir::Right));
        assert_eq!(app.active_window_id, 2);
        assert_eq!(app.focus, PaneFocus::View);
        assert!(!app.focus_adjacent_window(FocusDir::Right));
        assert_eq!(app.active_window_id, 2);
        server.abort();
    }

    #[tokio::test]
    async fn shift_arrow_on_key_moves_focus_in_all_four_directions() {
        let (mut app, _dir, server) = captured_app().await;
        // A 2x2 grid of panes, same geometry as the adjacency unit test.
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(0, 0, 40, 10),
                inner_area: Rect::new(1, 1, 38, 8),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(40, 0, 40, 10),
                inner_area: Rect::new(41, 1, 38, 8),
            },
            WindowPaneHit {
                id: 3,
                area: Rect::new(0, 10, 40, 10),
                inner_area: Rect::new(1, 11, 38, 8),
            },
            WindowPaneHit {
                id: 4,
                area: Rect::new(40, 10, 40, 10),
                inner_area: Rect::new(41, 11, 38, 8),
            },
        ];
        // A real split tree so `is_split_layout()` holds and each window has a
        // selection to focus.
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Below,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Right,
                ratio_percent: 50,
                first: Box::new(MainWindowTree::Leaf {
                    id: 1,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 2,
                    selection: Selection::Session("s1".into()),
                }),
            }),
            second: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Right,
                ratio_percent: 50,
                first: Box::new(MainWindowTree::Leaf {
                    id: 3,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 4,
                    selection: Selection::Session("s1".into()),
                }),
            }),
        };
        app.focus = PaneFocus::View;
        app.zoom = ZoomMode::None;

        // Drive the *full* on_key dispatch (not just `adjacent_window_id`) so we
        // exercise the same path a real keypress takes.
        app.active_window_id = 1;
        app.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.active_window_id, 2,
            "Shift+Right: top-left -> top-right"
        );

        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.active_window_id, 4,
            "Shift+Down: top-right -> bottom-right"
        );

        app.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.active_window_id, 3,
            "Shift+Left: bottom-right -> bottom-left"
        );

        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))
            .await;
        assert_eq!(app.active_window_id, 1, "Shift+Up: bottom-left -> top-left");

        server.abort();
    }

    #[tokio::test]
    async fn shift_up_down_move_focus_with_real_rendered_geometry() {
        let (mut app, _dir, server) = captured_app().await;
        // A vertical (Below) split: window 1 on top, window 2 on the bottom.
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Below,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.focus = PaneFocus::View;
        app.zoom = ZoomMode::None;
        app.active_window_id = 1;

        // Render a real frame so `main_window_areas` is populated by the actual
        // renderer (not hand-built geometry) for the vertical split.
        let backend = ratatui::backend::TestBackend::new(160, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("render split");

        // Shift+Down should move focus from the top window to the bottom one.
        app.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.active_window_id, 2,
            "Shift+Down should focus the window below"
        );

        // Re-render so geometry reflects the new active window, then Shift+Up
        // should move focus back to the top window.
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("render split");
        app.on_key(KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT))
            .await;
        assert_eq!(
            app.active_window_id, 1,
            "Shift+Up should focus the window above"
        );

        server.abort();
    }

    #[tokio::test]
    async fn c_x_arrow_chord_moves_split_focus_in_all_four_directions() {
        // `C-x <arrow>` is the terminal-agnostic alias for `Shift+<arrow>`.
        // Terminals (iTerm2, Terminal.app, GNOME Terminal) reserve Shift+Up/
        // Down for scrollback and never deliver them, so this chord is the
        // path that actually reaches the vertical-focus code there. Drive it
        // through the full on_key dispatch, two keystrokes per move.
        let (mut app, _dir, server) = captured_app().await;
        // A 2x2 grid of panes.
        app.layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(0, 0, 40, 10),
                inner_area: Rect::new(1, 1, 38, 8),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(40, 0, 40, 10),
                inner_area: Rect::new(41, 1, 38, 8),
            },
            WindowPaneHit {
                id: 3,
                area: Rect::new(0, 10, 40, 10),
                inner_area: Rect::new(1, 11, 38, 8),
            },
            WindowPaneHit {
                id: 4,
                area: Rect::new(40, 10, 40, 10),
                inner_area: Rect::new(41, 11, 38, 8),
            },
        ];
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Below,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Right,
                ratio_percent: 50,
                first: Box::new(MainWindowTree::Leaf {
                    id: 1,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 2,
                    selection: Selection::Session("s1".into()),
                }),
            }),
            second: Box::new(MainWindowTree::Split {
                direction: WindowSplitDirection::Right,
                ratio_percent: 50,
                first: Box::new(MainWindowTree::Leaf {
                    id: 3,
                    selection: Selection::Session("s1".into()),
                }),
                second: Box::new(MainWindowTree::Leaf {
                    id: 4,
                    selection: Selection::Session("s1".into()),
                }),
            }),
        };
        app.focus = PaneFocus::View;
        app.zoom = ZoomMode::None;

        // Helper: C-x then a plain arrow (no Shift) -> directional focus.
        async fn cx_arrow(app: &mut App, arrow: KeyCode) {
            app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
                .await;
            app.on_key(KeyEvent::new(arrow, KeyModifiers::NONE)).await;
        }

        app.active_window_id = 1;
        cx_arrow(&mut app, KeyCode::Right).await;
        assert_eq!(app.active_window_id, 2, "C-x Right: top-left -> top-right");

        cx_arrow(&mut app, KeyCode::Down).await;
        assert_eq!(
            app.active_window_id, 4,
            "C-x Down: top-right -> bottom-right"
        );

        cx_arrow(&mut app, KeyCode::Left).await;
        assert_eq!(
            app.active_window_id, 3,
            "C-x Left: bottom-right -> bottom-left"
        );

        cx_arrow(&mut app, KeyCode::Up).await;
        assert_eq!(app.active_window_id, 1, "C-x Up: bottom-left -> top-left");

        server.abort();
    }

    #[tokio::test]
    async fn applying_selected_hydration_does_not_start_transition() {
        let (mut app, _dir, server) = captured_app().await;
        app.selection = Selection::Session("s1".into());
        app.session_transitions.clear();
        let hydration = SessionHydration {
            session_id: "s1".into(),
            transcript: Vec::new(),
            history: Some(crate::pty_render::ItemHistory::new()),
            editor_state: None,
            agent_status: None,
            ui_panels: HashMap::new(),
            status_messages: Vec::new(),
            history_is_alt_screen: false,
        };

        app.apply_session_hydration(hydration).await;

        assert!(app.session_transitions.is_empty());
        assert!(app.histories.contains_key("s1"));
        server.abort();
    }

    #[tokio::test]
    async fn session_transition_is_scoped_to_active_split() {
        let (mut app, _dir, server) = captured_app().await;
        app.main_windows = MainWindowTree::Split {
            direction: WindowSplitDirection::Right,
            ratio_percent: 50,
            first: Box::new(MainWindowTree::Leaf {
                id: 1,
                selection: Selection::Session("s1".into()),
            }),
            second: Box::new(MainWindowTree::Leaf {
                id: 2,
                selection: Selection::Session("s1".into()),
            }),
        };
        app.active_window_id = 2;

        app.start_session_transition();

        assert!(!app.session_transitions.contains_key(&1));
        assert!(app.session_transitions.contains_key(&2));
        server.abort();
    }

    #[test]
    fn switch_session_matches_title_id_harness_and_fuzzy() {
        // `switch_session_match_score` is the shared match notion that drives
        // both the old switcher and the session-picker dialog's dimming: a
        // `Some` score means "matches" (rendered bright / selectable), `None`
        // means "no match" (dimmed). Covers title, id, harness, and a loose
        // fuzzy subsequence — and confirms non-matches return `None`.
        let mut shell = summary_with_kind(agentd_protocol::SessionKind::User);
        shell.id = "shell-session-abcdef".into();
        shell.harness = "shell".into();
        shell.title = Some("Build logs".into());
        let mut codex = summary_with_kind(agentd_protocol::SessionKind::User);
        codex.id = "codex-session-abcdef".into();
        codex.harness = "codex".into();
        codex.title = Some("Review PR".into());

        // Title substring.
        assert!(switch_session_match_score(&shell, "Build").is_some());
        assert!(switch_session_match_score(&codex, "Build").is_none());
        // Id substring.
        assert!(switch_session_match_score(&codex, "codex-session").is_some());
        assert!(switch_session_match_score(&shell, "codex-session").is_none());
        // Harness name.
        assert!(switch_session_match_score(&codex, "codex").is_some());
        assert!(switch_session_match_score(&shell, "codex").is_none());
        // Fuzzy subsequence over "Review PR".
        assert!(switch_session_match_score(&codex, "rvpr").is_some());
        assert!(switch_session_match_score(&shell, "rvpr").is_none());
        // An empty query matches everything (nothing is dimmed).
        assert!(switch_session_match_score(&shell, "").is_some());
        assert!(switch_session_match_score(&codex, "").is_some());
    }

    /// Fixture for the session-picker dialog: two ungrouped sessions (`alpha`,
    /// `beta`), a project `Proj` holding an active `gamma` and an archived
    /// `delta`.
    async fn session_picker_app() -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        let (mut app, dir, server) = empty_app().await;
        let mk = |id: &str, title: &str, harness: &str, pos: i64| {
            let mut s = summary_with_kind(agentd_protocol::SessionKind::User);
            s.id = id.into();
            s.title = Some(title.into());
            s.harness = harness.into();
            s.position = pos;
            s
        };
        let mut alpha = mk("s1", "alpha", "shell", 0);
        let mut beta = mk("s2", "beta", "codex", 1);
        let mut gamma = mk("s3", "gamma", "shell", 0);
        gamma.group_id = Some("g1".into());
        let mut delta = mk("s4", "delta", "shell", 1);
        delta.group_id = Some("g1".into());
        delta.archived = true;
        alpha.group_id = None;
        beta.group_id = None;
        app.sessions = vec![alpha, beta, gamma, delta];
        app.groups = vec![GroupSummary {
            id: "g1".into(),
            name: "Proj".into(),
            created_at: chrono::Utc::now(),
            position: 0,
            collapsed: false,
        }];
        (app, dir, server)
    }

    /// Titles of the non-dimmed (selectable) session rows, top to bottom.
    fn picker_bright(rows: &[SessionPickerRow]) -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                SessionPickerRow::Session {
                    summary,
                    dimmed: false,
                    ..
                } => summary.title.clone(),
                _ => None,
            })
            .collect()
    }

    fn picker_group(rows: &[SessionPickerRow], name: &str) -> Option<bool> {
        rows.iter().find_map(|r| match r {
            SessionPickerRow::GroupHeader {
                name: n, expanded, ..
            } if n == name => Some(*expanded),
            _ => None,
        })
    }

    fn picker_type(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_session_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
    }

    #[tokio::test]
    async fn session_picker_empty_query_lists_active_and_hides_archived() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        assert!(app.session_picker_active());
        let rows = app.session_picker_rows();
        // Ungrouped actives + group's active member are bright; archived delta
        // stays hidden behind a collapsed "1 archived" row.
        assert_eq!(picker_bright(&rows), vec!["alpha", "beta", "gamma"]);
        assert_eq!(picker_group(&rows, "Proj"), Some(true));
        let archive_open = rows
            .iter()
            .any(|r| matches!(r, SessionPickerRow::ArchiveHeader { expanded: true, .. }));
        assert!(!archive_open, "archive section should start collapsed");
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_query_dims_nonmatches_and_collapses_empty_groups() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        picker_type(&mut app, "alpha");
        let rows = app.session_picker_rows();
        // Only `alpha` matches; `beta` is dimmed (still present), and `Proj`
        // collapses because none of its sessions match.
        assert_eq!(picker_bright(&rows), vec!["alpha"]);
        assert_eq!(picker_group(&rows, "Proj"), Some(false));
        let beta_dimmed = rows.iter().any(|r| matches!(
            r,
            SessionPickerRow::Session { summary, dimmed: true, .. } if summary.title.as_deref() == Some("beta")
        ));
        assert!(beta_dimmed, "non-matching beta stays visible but dimmed");
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_query_autoexpands_group_with_match() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        picker_type(&mut app, "gamma");
        let rows = app.session_picker_rows();
        assert_eq!(picker_group(&rows, "Proj"), Some(true));
        assert_eq!(picker_bright(&rows), vec!["gamma"]);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_query_reveals_matching_archived_session() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        picker_type(&mut app, "delta");
        let rows = app.session_picker_rows();
        // The group opens (its archived member matches) and the archive section
        // expands to surface `delta`.
        assert_eq!(picker_group(&rows, "Proj"), Some(true));
        let archive_open = rows
            .iter()
            .any(|r| matches!(r, SessionPickerRow::ArchiveHeader { expanded: true, .. }));
        assert!(
            archive_open,
            "archive section should reveal the matching session"
        );
        assert_eq!(picker_bright(&rows), vec!["delta"]);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_navigation_wraps_and_confirms() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        // Three selectable rows: alpha(0), beta(1), gamma(2). Down twice lands
        // on gamma; once more wraps to alpha.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.session_picker.as_ref().unwrap().selected, 2);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.session_picker.as_ref().unwrap().selected, 0);
        // C-p wraps backwards to the last match.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().selected, 2);
        // Back to beta and confirm: switches focus to s2.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().selected, 1);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.session_picker.is_none(), "confirm closes the dialog");
        assert_eq!(app.selection.session_id(), Some("s2"));
        assert_eq!(app.focus, PaneFocus::View);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_escape_cancels() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.session_picker_active());
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_switch_left_is_noop() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let before = app.session_picker.as_ref().unwrap().selected;
        // The `C-x b` switcher has no parent menu to return to, so Left does
        // nothing — neither closing the dialog nor moving the selection.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            app.session_picker_active(),
            "Left is a no-op for the switcher"
        );
        assert_eq!(app.session_picker.as_ref().unwrap().selected, before);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_emacs_cursor_motion_edits_at_point() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        picker_type(&mut app, "ac"); // cursor at 2, query "ac"
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 1);
        // Insert between the two chars: "ac" -> "abc".
        picker_type(&mut app, "b");
        assert_eq!(app.session_picker.as_ref().unwrap().query, "abc");
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        // C-a jumps to the start; C-f steps one char forward.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 0);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 1);
        // C-e jumps to the end; backspace there deletes the last char, "abc" -> "ab".
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 3);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(app.session_picker.as_ref().unwrap().query, "ab");
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        // Motion clamps at both ends instead of wrapping or going negative.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 0);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_ctrl_k_kills_to_end_of_line() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        picker_type(&mut app, "abcdef"); // cursor at 6, query "abcdef"
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().query, "ab");
        // The cursor was already at (the new) end; it doesn't move.
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        // Killing again at the end of the line is a no-op.
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        assert_eq!(app.session_picker.as_ref().unwrap().query, "ab");
        assert_eq!(app.session_picker.as_ref().unwrap().cursor, 2);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_open_with_no_sessions_is_noop() {
        let (mut app, _dir, server) = empty_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        assert!(!app.session_picker_active());
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_renders_title_and_sessions() {
        let (mut app, _dir, server) = session_picker_app().await;
        app.open_session_picker(SessionPickerPurpose::Switch);
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("session picker should render");
        let text = app.frame_text.join("\n");
        assert!(text.contains("switch session"), "dialog title is shown");
        assert!(text.contains("alpha"), "sessions are listed");
        assert!(text.contains("Proj"), "project headers are shown");
        server.abort();
    }

    fn picker_program_block_texts(rows: &[SessionPickerRow]) -> Vec<&str> {
        rows.iter()
            .filter_map(|r| match r {
                SessionPickerRow::ProgramBlock { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn session_picker_lists_program_blocks_for_switch_purpose() {
        let (mut app, _dir, server) = session_picker_app().await;
        let markdown = "# Heading\n\nblock two\n\nblock three\n";
        app.program_popup = Some(program_popup_for_test("s1", markdown, 0));

        app.open_session_picker(SessionPickerPurpose::Switch);
        let rows = app.session_picker_rows();
        assert!(
            rows.iter()
                .any(|r| matches!(r, SessionPickerRow::ProgramHeader)),
            "a Program separator row is shown"
        );
        assert_eq!(
            picker_program_block_texts(&rows),
            vec!["# Heading", "block two", "block three"]
        );
        assert!(
            rows.iter()
                .any(|r| r.is_selectable() && matches!(r, SessionPickerRow::ProgramBlock { .. })),
            "blocks are navigable alongside sessions"
        );

        // A query narrows blocks to first-line matches, same as sessions.
        let filtered = app.session_picker_rows_for_query("two");
        assert_eq!(picker_program_block_texts(&filtered), vec!["block two"]);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_clip_variant_excludes_program_blocks() {
        let (mut app, _dir, server) = session_picker_app().await;
        let markdown = "# Heading\n\nblock two\n";
        app.program_popup = Some(program_popup_for_test("s1", markdown, 0));

        app.open_session_picker(SessionPickerPurpose::InsertProgramClip);
        let rows = app.session_picker_rows_for_query("");
        assert!(
            !rows.iter().any(|r| matches!(
                r,
                SessionPickerRow::ProgramHeader | SessionPickerRow::ProgramBlock { .. }
            )),
            "the `@`→session clip picker only lists sessions"
        );
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_hides_blocks_for_a_closing_program() {
        let (mut app, _dir, server) = session_picker_app().await;
        let markdown = "# Heading\n\nblock two\n";
        let mut popup = program_popup_for_test("s1", markdown, 0);
        popup.closing = true;
        app.program_popup = Some(popup);

        app.open_session_picker(SessionPickerPurpose::Switch);
        let rows = app.session_picker_rows();
        assert!(
            !rows.iter().any(|r| matches!(
                r,
                SessionPickerRow::ProgramHeader | SessionPickerRow::ProgramBlock { .. }
            )),
            "a program mid-close animation shouldn't offer stale blocks"
        );
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_caps_program_blocks_at_ten() {
        let (mut app, _dir, server) = session_picker_app().await;
        let markdown = (0..15)
            .map(|i| format!("block {i}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.program_popup = Some(program_popup_for_test("s1", &markdown, 0));

        app.open_session_picker(SessionPickerPurpose::Switch);
        let rows = app.session_picker_rows();
        assert_eq!(picker_program_block_texts(&rows).len(), 10);
        server.abort();
    }

    #[tokio::test]
    async fn session_picker_confirm_program_block_scrolls_to_it() {
        let (mut app, _dir, server) = session_picker_app().await;
        let markdown = "# Heading\n\nblock two\n\nblock three\n";
        app.program_popup = Some(program_popup_for_test("s1", markdown, 0));
        app.program_popup.as_mut().unwrap().terminal_focus = true; // slid aside
        app.layout.program_inner_area = Some(ratatui::layout::Rect::new(0, 0, 40, 5));

        app.open_session_picker(SessionPickerPurpose::Switch);
        // Selectable rows: alpha, beta, gamma, then the three program blocks —
        // five Downs from the initial `alpha` selection lands on "block three".
        for _ in 0..5 {
            app.handle_session_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_session_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

        assert!(app.session_picker.is_none(), "confirm closes the dialog");
        assert_eq!(app.focus, PaneFocus::View);
        let popup = app.program_popup.as_ref().expect("program stays open");
        assert!(
            !popup.terminal_focus,
            "confirm undoes a terminal-focus slide so the buffer is visible"
        );
        assert_eq!(
            popup.scroll_offset, 4,
            "scrolls so block three's line (a non-wrapping buffer, so line == visual row) is in view"
        );
        server.abort();
    }

    #[tokio::test]
    async fn approval_prompt_opens_for_selected_session() {
        let (mut app, _dir, server) = captured_app().await;

        app.maybe_open_approval_prompt(
            "s1".into(),
            "call-1".into(),
            "shell".into(),
            "echo hi".into(),
            agentd_protocol::ToolRisk::Risky,
            true,
        );

        assert!(matches!(
            app.minibuffer.as_ref().map(|mb| &mb.intent),
            Some(MinibufferIntent::ApproveTool { session_id, .. }) if session_id == "s1"
        ));
        let prompt = &app.minibuffer.as_ref().unwrap().prompt;
        assert!(prompt.contains("approve [risky] shell(echo hi)"));
        assert!(prompt.contains("y=approve"));
        assert!(prompt.contains("a=auto-review"));
        server.abort();
    }

    #[tokio::test]
    async fn approval_prompt_does_not_open_for_smith_session() {
        let (mut app, _dir, server) = captured_app().await;
        app.sessions[0].harness = "smith".into();

        app.maybe_open_approval_prompt(
            "s1".into(),
            "call-1".into(),
            "shell".into(),
            "echo hi".into(),
            agentd_protocol::ToolRisk::Risky,
            true,
        );

        assert!(
            app.minibuffer.is_none(),
            "smith renders approval inline in the session PTY"
        );
        server.abort();
    }

    #[tokio::test]
    async fn approval_prompt_does_not_open_for_orchestrator_session() {
        let (mut app, _dir, server) = captured_app().await;
        app.orchestrator_id = Some("s1".into());

        app.maybe_open_approval_prompt(
            "s1".into(),
            "call-1".into(),
            "shell".into(),
            "echo hi".into(),
            agentd_protocol::ToolRisk::Risky,
            true,
        );

        assert!(
            app.minibuffer.is_none(),
            "orchestrator renders approval inline in its PTY"
        );
        server.abort();
    }

    #[tokio::test]
    async fn approval_prompt_hides_auto_review_when_disallowed() {
        let (mut app, _dir, server) = captured_app().await;

        app.maybe_open_approval_prompt(
            "s1".into(),
            "call-1".into(),
            "shell".into(),
            "echo hi".into(),
            agentd_protocol::ToolRisk::Risky,
            false,
        );

        let prompt = &app.minibuffer.as_ref().unwrap().prompt;
        assert!(prompt.contains("y=approve"));
        assert!(prompt.contains("n=deny"));
        assert!(prompt.contains("f=unsafe-auto"));
        assert!(!prompt.contains("a=auto-review"));
        server.abort();
    }

    #[tokio::test]
    async fn approval_prompt_ignores_unselected_session() {
        let (mut app, _dir, server) = captured_app().await;
        let mut background = summary_with_kind(agentd_protocol::SessionKind::User);
        background.id = "background".into();
        app.sessions.push(background);
        app.selection = Selection::Session("s1".into());

        app.maybe_open_approval_prompt(
            "background".into(),
            "call-1".into(),
            "shell".into(),
            "echo hi".into(),
            agentd_protocol::ToolRisk::Risky,
            true,
        );

        assert!(
            app.minibuffer.is_none(),
            "background approval requests should not open the global minibuffer"
        );
        server.abort();
    }

    // --- repeated-key latency regression guards (see PR #157) ---
    //
    // These encode the two optimizations as invariants so a future
    // change that reintroduces per-keystroke RPCs or per-keystroke
    // stale renders fails in CI — without depending on wall-clock
    // timing (which is too flaky on shared runners to assert).

    /// A burst of queued same-session keystrokes must coalesce into
    /// ONE batched write. Regression guard for the "one awaited
    /// `pty_input` RPC per keystroke" latency bug.
    #[test]
    fn coalesce_pty_input_batches_same_session_burst() {
        let (tx, mut rx) = mpsc::unbounded_channel::<PtyInputJob>();
        for i in 0..40u8 {
            tx.send(PtyInputJob {
                session_id: "s1".into(),
                bytes: vec![i],
                label: "pty_input",
            })
            .unwrap();
        }
        let first = rx.try_recv().unwrap();
        let (sid, bytes, _label, carried) = coalesce_pty_input(first, &mut rx);
        assert_eq!(sid, "s1");
        assert_eq!(
            bytes.len(),
            40,
            "all 40 same-session keystrokes must batch into one write"
        );
        assert!(carried.is_none());
    }

    /// Coalescing stops at the first different-session job, which is
    /// carried over so its own burst still batches next call.
    #[test]
    fn coalesce_pty_input_stops_at_session_boundary() {
        let (tx, mut rx) = mpsc::unbounded_channel::<PtyInputJob>();
        for (s, b) in [("s1", b'a'), ("s1", b'b'), ("s2", b'c'), ("s2", b'd')] {
            tx.send(PtyInputJob {
                session_id: s.into(),
                bytes: vec![b],
                label: "pty_input",
            })
            .unwrap();
        }
        let first = rx.try_recv().unwrap();
        let (sid, bytes, _label, carried) = coalesce_pty_input(first, &mut rx);
        assert_eq!(sid, "s1");
        assert_eq!(bytes, b"ab");
        let carried = carried.expect("different-session job must carry over");
        assert_eq!(carried.session_id, "s2");
        assert_eq!(carried.bytes, b"c");
    }

    /// Build an App that's in PTY-capture mode (view focus, terminal
    /// view, a live PTY session), connected to a mock daemon that
    /// just accepts the socket. Returns the temp dir + server task so
    /// the caller keeps them alive.
    async fn captured_app() -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");
        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.has_pty = true;
        let app = test_app(client, vec![summary]);
        (app, dir, server)
    }

    async fn empty_app() -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");
        let app = test_app(client, Vec::new());
        (app, dir, server)
    }

    /// Like `empty_app`, but pre-populated with two live user sessions (`s1`,
    /// `s2`); selection and the active window both start on `s1`. The minimum
    /// fixture for exercising list navigation.
    async fn two_session_app() -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");
        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        let app = test_app(client, vec![s1, s2]);
        (app, dir, server)
    }

    fn rendered_text(buffer: &ratatui::buffer::Buffer) -> String {
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn selection_bounds_use_split_window_inner_area() {
        let mut layout = test_layout();
        layout.main_window_areas = vec![
            WindowPaneHit {
                id: 1,
                area: Rect::new(20, 0, 40, 20),
                inner_area: Rect::new(21, 1, 38, 18),
            },
            WindowPaneHit {
                id: 2,
                area: Rect::new(60, 0, 40, 20),
                inner_area: Rect::new(61, 1, 38, 18),
            },
        ];

        assert_eq!(
            selection_bounds_for_layout(&layout, 0, false, 65, 5),
            Some(Rect::new(61, 1, 38, 18))
        );
        assert_eq!(
            selection_bounds_for_layout(&layout, 0, false, 60, 5),
            None,
            "split borders should not be selectable text"
        );
        assert_eq!(
            selection_bounds_for_layout(&layout, 0, false, 55, 5),
            Some(Rect::new(21, 1, 38, 18))
        );
    }

    #[tokio::test]
    async fn operator_monolog_typewriter_renders_then_expires() {
        let (mut app, _dir, server) = empty_app().await;
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let area = Rect::new(0, 0, 60, 12);

        // ~500ms in: several characters revealed, still showing.
        app.operator_monolog = Some(OperatorMonolog {
            text: "session waiting at trust prompt".into(),
            started_at: Instant::now() - std::time::Duration::from_millis(500),
        });
        let mut showing = false;
        terminal
            .draw(|f| {
                showing = crate::ui::render_operator_monolog(f, area, &mut app, Instant::now())
            })
            .expect("draw");
        assert!(showing, "monolog should be showing mid-cycle");
        let screen = rendered_text(terminal.backend().buffer());
        assert!(screen.contains("session"), "missing typed text:\n{screen}");
        // No "operator ▸" label — the matrix panel title already says "operator".
        assert!(!screen.contains("▸"), "label should be gone:\n{screen}");

        // Far past type+hold+fade: clears itself and yields the rain.
        app.operator_monolog = Some(OperatorMonolog {
            text: "session waiting at trust prompt".into(),
            started_at: Instant::now() - std::time::Duration::from_secs(30),
        });
        terminal
            .draw(|f| {
                showing = crate::ui::render_operator_monolog(f, area, &mut app, Instant::now())
            })
            .expect("draw");
        assert!(!showing, "monolog should have expired");
        assert!(
            app.operator_monolog.is_none(),
            "expired monolog not cleared"
        );

        server.abort();
    }

    #[tokio::test]
    async fn pty_notification_dirties_view_only_for_visible_session() {
        // The run loop reads `notification_dirtied_view` to skip a full-frame
        // repaint for background PTY chunks. Verify the gate: visible session
        // PTY -> dirties; off-screen session PTY -> does not; structural events
        // and pinned/orchestrator sessions -> always dirty.
        let (mut app, _dir, server) = empty_app().await;
        let mut vis = summary_with_kind(agentd_protocol::SessionKind::User);
        vis.id = "vis".into();
        vis.has_pty = true;
        let mut bg = summary_with_kind(agentd_protocol::SessionKind::User);
        bg.id = "bg".into();
        bg.has_pty = true;
        app.sessions = vec![vis, bg];
        app.selection = Selection::Session("vis".into());
        app.main_windows = MainWindowTree::single(1, Selection::Session("vis".into()));
        app.active_window_id = 1;

        async fn feed(app: &mut App, session: &str, event: SessionEvent) {
            app.on_notification(Notification {
                jsonrpc: "2.0".into(),
                method: agentd_protocol::ipc_notif::EVENT.into(),
                params: Some(
                    serde_json::to_value(EventNotificationPayload {
                        session_id: session.into(),
                        at: chrono::Utc::now(),
                        event,
                        seq: 1,
                    })
                    .unwrap(),
                ),
            })
            .await;
        }
        let pty = || SessionEvent::Pty {
            data: String::new(),
        };

        feed(&mut app, "vis", pty()).await;
        assert!(
            app.notification_dirtied_view,
            "visible session PTY must repaint"
        );

        feed(&mut app, "bg", pty()).await;
        assert!(
            !app.notification_dirtied_view,
            "off-screen session PTY must NOT force a repaint"
        );

        // A structural/status event for the same off-screen session still
        // repaints (it changes the list), because only the `Pty` arm clears it.
        feed(
            &mut app,
            "bg",
            SessionEvent::Status {
                state: agentd_protocol::SessionState::Done,
                detail: None,
            },
        )
        .await;
        assert!(
            app.notification_dirtied_view,
            "non-PTY events must always repaint"
        );

        // Pinning the off-screen session makes its PTY visible (pin strip).
        app.sessions[1].pinned = true;
        feed(&mut app, "bg", pty()).await;
        assert!(
            app.notification_dirtied_view,
            "pinned session PTY is visible and must repaint"
        );

        server.abort();
    }

    #[tokio::test]
    async fn operator_monolog_skipped_while_orchestrator_panel_open() {
        let (mut app, _dir, server) = empty_app().await;
        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let area = Rect::new(0, 0, 60, 12);
        app.operator_monolog = Some(OperatorMonolog {
            text: "session waiting at trust prompt".into(),
            started_at: Instant::now() - std::time::Duration::from_millis(500),
        });
        // Orchestrator panel open → the text is visible below, so don't overlay.
        app.minibuffer = Some(Minibuffer {
            prompt: String::new(),
            input: String::new(),
            cursor: 0,
            intent: MinibufferIntent::Orchestrator,
            error: None,
        });
        let mut drew = true;
        terminal
            .draw(|f| drew = crate::ui::render_operator_monolog(f, area, &mut app, Instant::now()))
            .expect("draw");
        assert!(!drew, "monolog should be skipped while the panel is open");
        let screen = rendered_text(terminal.backend().buffer());
        assert!(
            !screen.contains("session"),
            "should not draw over rain:\n{screen}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn operator_monolog_accumulates_across_delta_heartbeats() {
        // `AgentStatus active=true` fires on every delta (a per-token "Working"
        // heartbeat), so the utterance must accumulate across them, not reset —
        // otherwise only the final delta survives ("noted" → "ed").
        let (mut app, _dir, server) = empty_app().await;
        app.orchestrator_id = Some("op".into());

        async fn feed(app: &mut App, event: SessionEvent, seq: u64) {
            let n = Notification {
                jsonrpc: "2.0".into(),
                method: agentd_protocol::ipc_notif::EVENT.into(),
                params: Some(
                    serde_json::to_value(EventNotificationPayload {
                        session_id: "op".into(),
                        at: chrono::Utc::now(),
                        event,
                        seq,
                    })
                    .unwrap(),
                ),
            };
            app.on_notification(n).await;
        }
        fn working() -> SessionEvent {
            SessionEvent::AgentStatus(agentd_protocol::AgentStatus {
                active: true,
                started_at_ms: 1,
                status: "Working".into(),
            })
        }
        fn worked() -> SessionEvent {
            SessionEvent::AgentStatus(agentd_protocol::AgentStatus {
                active: false,
                started_at_ms: 1,
                status: "Worked".into(),
            })
        }
        fn say(t: &str) -> SessionEvent {
            SessionEvent::Message {
                role: MessageRole::Assistant,
                text: t.into(),
            }
        }

        // "noted" as two deltas, each preceded by a heartbeat → full "noted"
        // accumulates → filtered → no monolog (NOT the bare tail "ed").
        feed(&mut app, working(), 1).await;
        feed(&mut app, say("not"), 2).await;
        feed(&mut app, working(), 3).await;
        feed(&mut app, say("ed"), 4).await;
        feed(&mut app, worked(), 5).await;
        assert!(
            app.operator_monolog.is_none(),
            "noted must be filtered, got {:?}",
            app.operator_monolog.as_ref().map(|m| &m.text)
        );

        // A real finding across deltas → monolog gets the FULL text, not a tail.
        feed(&mut app, working(), 6).await;
        feed(&mut app, say("session "), 7).await;
        feed(&mut app, working(), 8).await;
        feed(&mut app, say("blocked"), 9).await;
        feed(&mut app, worked(), 10).await;
        assert_eq!(
            app.operator_monolog.as_ref().map(|m| m.text.as_str()),
            Some("session blocked")
        );

        server.abort();
    }

    #[tokio::test]
    async fn empty_tui_renders_welcome_and_modeline_hint() {
        let (mut app, _dir, server) = empty_app().await;
        let backend = ratatui::backend::TestBackend::new(120, 36);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        let screen = rendered_text(terminal.backend().buffer());
        assert!(
            screen.contains("Welcome to construct"),
            "missing welcome:\n{screen}"
        );
        assert!(
            screen.contains("C-x C-f"),
            "missing create shortcut:\n{screen}"
        );
        assert!(
            screen.contains("exit TUI"),
            "missing exit shortcut:\n{screen}"
        );
        assert!(
            screen.contains("C-x C-c  exit TUI"),
            "missing quit shortcut:\n{screen}"
        );
        assert!(
            !screen.contains("q        exit construct"),
            "empty state should not show q as the quit shortcut:\n{screen}"
        );
        assert!(
            screen.contains("new: C-x C-f  help: ?  palette: C-x x"),
            "missing modeline hint:\n{screen}"
        );
        assert!(
            !screen.contains("CLI examples:"),
            "empty state should not include CLI examples:\n{screen}"
        );
        assert!(
            app.layout.shortcut_hints.len() >= 4,
            "expected clickable shortcuts, got {:?}",
            app.layout.shortcut_hints
        );
        assert!(app
            .layout
            .shortcut_hints
            .iter()
            .any(|h| h.action == KeyAction::OpenNewSession));
        assert!(app
            .layout
            .shortcut_hints
            .iter()
            .any(|h| h.action == KeyAction::OpenCommandPalette));
        assert!(app
            .layout
            .shortcut_hints
            .iter()
            .any(|h| h.action == KeyAction::ToggleHelp));
        assert!(app
            .layout
            .shortcut_hints
            .iter()
            .any(|h| h.action == KeyAction::Quit));
        server.abort();
    }

    #[tokio::test]
    async fn update_notice_renders_right_aligned_in_modeline() {
        let (mut app, _dir, server) = empty_app().await;
        app.update_notice = Some("↑ construct 9.9.9 · construct upgrade".to_string());
        let backend = ratatui::backend::TestBackend::new(120, 36);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        let screen = rendered_text(terminal.backend().buffer());
        let modeline = screen
            .lines()
            .find(|l| l.contains("↑ construct 9.9.9 · construct upgrade"))
            .expect("update notice should be on screen");

        // Right-aligned: only padding follows it to the right edge.
        assert!(
            modeline.trim_end().ends_with("construct upgrade"),
            "notice should sit at the right edge:\n{modeline}"
        );
        // ...and it lives in the right half, not inline on the left.
        let col = modeline.find('↑').expect("arrow present");
        assert!(
            col > 60,
            "notice should start in the right half (byte col {col}):\n{modeline}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn empty_state_shortcut_clicks_dispatch_actions() {
        let (mut app, _dir, server) = empty_app().await;
        app.harnesses = vec![agentd_protocol::HarnessInfo {
            name: "shell".to_string(),
            available: true,
            detail: None,
            binary: None,
            description: None,
            capabilities: Default::default(),
        }];
        let backend = ratatui::backend::TestBackend::new(120, 36);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        let click = |app: &App, action: KeyAction| {
            let h = app
                .layout
                .shortcut_hints
                .iter()
                .find(|h| h.action == action)
                .expect("shortcut hit")
                .clone();
            (h.x_start, h.y)
        };

        let (x, y) = click(&app, KeyAction::OpenCommandPalette);
        app.handle_left_click(x, y).await;
        assert!(matches!(
            app.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::CommandPalette)
        ));
        app.minibuffer = None;

        let (x, y) = click(&app, KeyAction::ToggleHelp);
        app.handle_left_click(x, y).await;
        assert!(app.help_visible);
        app.help_visible = false;

        let (x, y) = click(&app, KeyAction::OpenNewSession);
        app.handle_left_click(x, y).await;
        assert!(matches!(
            app.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::NewSessionHarness)
        ));
        app.minibuffer = None;

        let (x, y) = click(&app, KeyAction::Quit);
        app.handle_left_click(x, y).await;
        assert!(app.should_quit);
        server.abort();
    }

    #[tokio::test]
    async fn help_modal_includes_getting_started_concepts() {
        let (mut app, _dir, server) = empty_app().await;
        app.help_visible = true;
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        let screen = rendered_text(terminal.backend().buffer());
        assert!(
            screen.contains("getting started"),
            "missing section:\n{screen}"
        );
        assert!(
            screen.contains("A session is one live task"),
            "missing session concept:\n{screen}"
        );
        assert!(
            screen.contains("A harness is the runtime"),
            "missing harness concept:\n{screen}"
        );
        assert!(
            screen.contains("emacs keymap"),
            "default help should show emacs profile:\n{screen}"
        );
        assert!(
            screen.contains("Use C-x C-f to create a session"),
            "default help should show emacs shortcuts:\n{screen}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn help_modal_uses_vim_shortcuts_in_vim_profile() {
        let (mut app, _dir, server) = empty_app().await;
        app.help_visible = true;
        app.profile = Profile::Vim;
        app.keymap = keymap::default_for(Profile::Vim);
        let backend = ratatui::backend::TestBackend::new(120, 70);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        let screen = rendered_text(terminal.backend().buffer());
        assert!(
            screen.contains("vim keymap"),
            "vim help should show vim profile:\n{screen}"
        );
        assert!(
            screen.contains("Use n to create a session"),
            "vim help should show vim create shortcut:\n{screen}"
        );
        assert!(
            screen.contains(":               command palette"),
            "vim help should show vim command palette shortcut:\n{screen}"
        );
        assert!(
            !screen.contains("Use C-x C-f to create a session"),
            "vim help should not show the emacs create shortcut as primary:\n{screen}"
        );
        server.abort();
    }

    /// A plain keystroke forwarded to the PTY must set
    /// `skip_redraw_after_event` — its visible effect arrives later
    /// as PTY output, so the immediate top-of-loop draw would be a
    /// wasted stale frame. Regression guard for per-keystroke renders.
    #[tokio::test]
    async fn pty_passthrough_keystroke_skips_redraw() {
        let (mut app, _dir, _srv) = captured_app().await;
        assert!(app.is_pty_captured(), "precondition: PTY-capture mode");
        app.skip_redraw_after_event = false;
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        assert!(
            app.skip_redraw_after_event,
            "a PTY-passthrough keystroke must skip the immediate stale redraw"
        );
    }

    /// But a keystroke that snaps scrollback back to live IS a local
    /// display change with no PTY output of its own — it must redraw.
    #[tokio::test]
    async fn pty_keystroke_snapping_scrollback_still_redraws() {
        let (mut app, _dir, _srv) = captured_app().await;
        // Use the proper setter so the per-window store stays the canonical
        // source: a bare `view_scrollback = 5` only updates the mirror and
        // the keystroke path now reads the per-window store.
        let win = Some(app.active_window_id);
        app.set_scrollback_for_window(win, 5);
        app.skip_redraw_after_event = false;
        app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .await;
        assert_eq!(app.view_scrollback, 0, "typing snaps the view to live");
        assert_eq!(
            app.scrollback_for_window(win),
            0,
            "per-window store must also snap to live, not just the mirror"
        );
        assert!(
            !app.skip_redraw_after_event,
            "snapping scrollback to live has no PTY output, so it must redraw"
        );
    }

    #[tokio::test]
    async fn codex_pty_view_scrolls_with_keyboard_chord() {
        let (mut app, _dir, _srv) = captured_app().await;
        app.sessions[0].harness = "codex".into();
        app.view_scrollback = 0;

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('['), KeyModifiers::NONE))
            .await;

        assert_eq!(
            app.view_scrollback, 10,
            "C-x [ should page PTY scrollback up in a codex terminal view"
        );
    }

    /// PageUp/PageDown page the TUI scrollback even while the PTY is captured,
    /// instead of being forwarded to the child — same effect as `C-x [` / `]`.
    #[tokio::test]
    async fn pageup_pagedown_scroll_captured_pty_view() {
        let (mut app, _dir, _srv) = captured_app().await;
        app.sessions[0].harness = "codex".into();
        app.view_scrollback = 0;

        app.on_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.view_scrollback, 10,
            "PageUp should page PTY scrollback up in a captured terminal view"
        );

        app.on_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.view_scrollback, 0,
            "PageDown should page PTY scrollback back down"
        );
    }

    #[tokio::test]
    async fn codex_pty_view_scrolls_with_mouse_wheel() {
        let (mut app, _dir, _srv) = captured_app().await;
        app.sessions[0].harness = "codex".into();
        app.layout.list_items_area = Some(Rect::new(0, 0, 20, 20));
        app.terminal_pane_size = (80, 40);
        app.view_scrollback = 0;

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 50,
            row: 10,
            modifiers: KeyModifiers::NONE,
        })
        .await;

        assert_eq!(
            app.view_scrollback, 10,
            "mouse wheel outside the list should scroll codex PTY history by a partial page"
        );
        assert!(
            app.terminal_scrollbar_visible_until(None).is_some(),
            "mouse-wheel scroll should reveal the terminal scrollbar overlay"
        );
    }

    #[tokio::test]
    async fn terminal_render_clamps_scrollback_label_to_available_history() {
        let (mut app, _dir, _srv) = captured_app().await;
        app.sessions[0].harness = "codex".into();
        app.view = ViewMode::Terminal;
        app.focus = PaneFocus::View;
        let id = app.sessions[0].id.clone();
        let mut history = crate::pty_render::ItemHistory::new();
        history.feed_pty(b"only one visible line\r\n");
        app.histories.insert(id, history);
        app.view_scrollback = SCROLLBACK_MAX;

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| crate::ui::render(f, &mut app))
            .expect("draw");

        assert_eq!(
            app.view_scrollback, 0,
            "modeline scrollback value should be the effective rendered scrollback"
        );
    }

    /// Starting a `C-x` chord is not a passthrough — it must redraw
    /// (to surface the chord indicator), never skip.
    #[tokio::test]
    async fn ctrl_x_chord_start_does_not_skip_redraw() {
        let (mut app, _dir, _srv) = captured_app().await;
        app.skip_redraw_after_event = false;
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        assert!(
            !app.skip_redraw_after_event,
            "a C-x chord start must redraw, not skip"
        );
    }

    /// `C-x C-x` in a captured PTY is the escape hatch: the first C-x opens
    /// the chord prefix, the second C-x cancels it *and* forwards a literal
    /// C-x byte (0x18) to the focused session — so harnesses that bind C-x
    /// internally (grok, bash's `C-x C-e`, vim completion) still receive it.
    #[tokio::test]
    async fn ctrl_x_ctrl_x_forwards_literal_ctrl_x_to_focused_pty() {
        let (mut app, _dir, _srv) = captured_app().await;
        assert!(app.is_pty_captured(), "precondition: PTY-capture mode");

        // Swap in a channel we own so the forwarded bytes are observable
        // instead of being drained straight to the (mock) daemon.
        let (tx, mut rx) = mpsc::unbounded_channel::<PtyInputJob>();
        app.pty_input_tx = tx;

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        assert!(
            !app.chord_state.is_empty(),
            "first C-x must arm the chord prefix"
        );
        assert!(
            rx.try_recv().is_err(),
            "the prefix C-x must not be forwarded on its own"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;

        let job = rx
            .try_recv()
            .expect("second C-x must forward a byte to the PTY");
        assert_eq!(
            job.bytes,
            vec![0x18],
            "C-x C-x forwards a literal C-x (0x18) to the focused session"
        );
        assert!(
            app.chord_state.is_empty(),
            "the second C-x must cancel the chord prefix"
        );
        assert!(
            app.chord_label.is_empty(),
            "the chord indicator must be cleared after the escape hatch fires"
        );
    }

    /// The orchestrator/operator panel ("the minibuffer is just another
    /// session") has its own copy of the escape hatch: `C-x C-x` inside the
    /// panel cancels the chord and forwards a literal C-x (0x18) to the
    /// orchestrator's PTY, matching the main-view behavior.
    #[tokio::test]
    async fn orchestrator_panel_ctrl_x_ctrl_x_forwards_literal_ctrl_x() {
        let (mut app, _dir, server) = empty_app().await;
        // An orchestrator session → `C-x x` opens the operator input panel.
        app.orchestrator_id = Some("orch".to_string());

        // Swap in a channel we own so the forwarded bytes are observable.
        let (tx, mut rx) = mpsc::unbounded_channel::<PtyInputJob>();
        app.pty_input_tx = tx;

        // Open the orchestrator panel (`C-x x`) — this routes later keys
        // through `handle_orchestrator_key` and forwards no PTY bytes itself.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .await;
        assert!(
            matches!(
                app.minibuffer.as_ref().map(|m| &m.intent),
                Some(MinibufferIntent::Orchestrator)
            ),
            "precondition: the orchestrator panel must be open"
        );
        assert!(
            rx.try_recv().is_err(),
            "opening the panel must not forward any PTY bytes"
        );

        // C-x C-x inside the panel forwards a literal C-x to the orchestrator.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;

        let job = rx
            .try_recv()
            .expect("C-x C-x must forward a byte to the orchestrator PTY");
        assert_eq!(
            job.session_id, "orch",
            "the byte must target the orchestrator session"
        );
        assert_eq!(
            job.bytes,
            vec![0x18],
            "C-x C-x forwards a literal C-x (0x18) to the orchestrator"
        );
        assert!(
            app.chord_state.is_empty(),
            "the second C-x must cancel the chord prefix"
        );
        server.abort();
    }

    #[tokio::test]
    async fn disconnected_c_x_c_quits_even_when_pty_would_capture_keys() {
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
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

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;

        assert!(app.should_quit);
        server.abort();
    }

    // issue #140: clicking a matrix-rain horizontal reveal word switches
    // the selection to the session that produced it; clicking a word
    // whose session has ended is a no-op (just a status message).
    #[tokio::test]
    async fn matrix_reveal_click_switches_to_source_session() {
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        let mut app = test_app(client, vec![s1, s2]);
        assert_eq!(app.selection.session_id(), Some("s1"));

        app.matrix_reveal_hits = vec![MatrixRevealHit {
            col_start: 5,
            col_end: 10,
            row: 20,
            text: "deploy".into(),
            session_id: "s2".into(),
        }];
        // Click inside the word span -> switch to s2.
        app.handle_left_click(7, 20).await;
        assert_eq!(
            app.selection.session_id(),
            Some("s2"),
            "click on a reveal word switches to its session"
        );
        // Click outside the span -> no change.
        app.handle_left_click(30, 20).await;
        assert_eq!(app.selection.session_id(), Some("s2"));

        // Word whose session has ended -> no switch.
        app.matrix_reveal_hits = vec![MatrixRevealHit {
            col_start: 5,
            col_end: 10,
            row: 20,
            text: "ghost".into(),
            session_id: "gone".into(),
        }];
        app.handle_left_click(7, 20).await;
        assert_eq!(
            app.selection.session_id(),
            Some("s2"),
            "click for a missing session must not switch selection"
        );
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
        let sock = dir.path().join("construct.sock");
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

        // In Chat view a missing history must NOT spin up fetches.
        app.view = ViewMode::Chat;
        assert!(
            !app.selected_needs_hydration(),
            "transcript view should not re-fetch PTY history"
        );

        server.abort();
    }

    #[tokio::test]
    async fn stale_selected_hydration_warms_history_without_replacing_transcript() {
        use agentd_client::Client;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
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
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        s1.has_pty = true;
        let mut s2 = summary_with_kind(agentd_protocol::SessionKind::User);
        s2.id = "s2".into();
        s2.has_pty = true;
        let mut app = test_app(client, vec![s1, s2]);
        app.selection = Selection::Session("s2".into());
        app.transcript_session = Some("s2".into());
        app.hydrating_sessions.insert("s1".into());

        app.apply_session_hydration(SessionHydration {
            session_id: "s1".into(),
            transcript: vec![TimestampedEvent {
                seq: 1,
                at: chrono::Utc::now(),
                event: SessionEvent::Message {
                    role: agentd_protocol::MessageRole::Assistant,
                    text: "old selected transcript".into(),
                },
            }],
            history: Some(crate::pty_render::ItemHistory::new()),
            editor_state: None,
            agent_status: None,
            ui_panels: HashMap::new(),
            status_messages: Vec::new(),
            history_is_alt_screen: false,
        })
        .await;

        assert!(
            app.histories.contains_key("s1"),
            "stale selected hydration should still warm the history cache"
        );
        assert_eq!(
            app.transcript_session.as_deref(),
            Some("s2"),
            "stale selected hydration must not replace the visible transcript"
        );
        assert!(
            !app.hydrating_sessions.contains("s1"),
            "completed stale hydration should clear the loading marker"
        );

        server.abort();
    }

    #[tokio::test]
    async fn orchestrator_hydration_loads_existing_sticky_widgets() {
        use agentd_protocol::{UiPanel, UiPlacement};

        let (mut app, _dir, server) = captured_app().await;
        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch".into();
        orch.has_pty = true;
        app.sessions.push(orch);
        app.refresh_orchestrator_id();

        assert_eq!(
            app.orchestrator_session_needing_hydration().as_deref(),
            Some("orch"),
            "a freshly launched TUI should hydrate the hidden orchestrator before live widget events arrive"
        );

        app.apply_pinned_session_hydration(SessionHydration {
            session_id: "orch".into(),
            transcript: Vec::new(),
            history: Some(crate::pty_render::ItemHistory::new()),
            editor_state: None,
            agent_status: None,
            ui_panels: HashMap::from([(
                "fleet-pulse".into(),
                UiPanel {
                    id: "fleet-pulse".into(),
                    source: Some("fleet-pulse.md".into()),
                    title: Some("Fleet pulse".into()),
                    created_at_ms: 1,
                    placement: UiPlacement::Sticky,
                    markdown: "# Fleet pulse".into(),
                },
            )]),
            status_messages: Vec::new(),
            history_is_alt_screen: false,
        })
        .await;

        assert!(app.orchestrator_session_needing_hydration().is_none());
        assert_eq!(app.orchestrator_widget_panels().len(), 1);
        assert_eq!(app.orchestrator_widget_panels()[0].id, "fleet-pulse");
        server.abort();
    }

    // The selected session's browser preview is painted as a wallpaper
    // behind the matrix rain (half-block `▀` cells), and vanishes when
    // the preview is gone — in lock-step with the terminal-view overlay.
    #[tokio::test]
    async fn operator_matrix_widgets_render_without_unbounded_padding() {
        use agentd_protocol::{UiPanel, UiPlacement};

        let (mut app, _dir, server) = captured_app().await;
        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch".into();
        app.sessions.push(orch);
        app.refresh_orchestrator_id();
        app.matrix_rain_hidden = false;
        app.matrix_widget_pinned = Some("fleet-pulse".into());
        app.ui_panels.insert(
            "orch".into(),
            HashMap::from([
                (
                    "ambient-note".into(),
                    UiPanel {
                        id: "ambient-note".into(),
                        source: Some("ambient-note.md".into()),
                        title: Some("Ambient note".into()),
                        created_at_ms: 1,
                        placement: UiPlacement::Sticky,
                        markdown: "# Ambient note\n\nOperator widgets are sticky.".into(),
                    },
                ),
                (
                    "fleet-pulse".into(),
                    UiPanel {
                        id: "fleet-pulse".into(),
                        source: Some("fleet-pulse.md".into()),
                        title: Some("Fleet pulse".into()),
                        created_at_ms: 2,
                        placement: UiPlacement::Sticky,
                        markdown: "# Fleet pulse\n\n:::timeline\n- [x] Demo widget visible\n- [~] Operator can surface fleet status here\n- [ ] Hover/click square indicators\n:::".into(),
                    },
                ),
                (
                    "merge-queue".into(),
                    UiPanel {
                        id: "merge-queue".into(),
                        source: Some("merge-queue.md".into()),
                        title: Some("Merge queue".into()),
                        created_at_ms: 3,
                        placement: UiPlacement::Sticky,
                        markdown: "# Merge queue\n\n| PR | State |\n| --- | --- |\n| demo | ready |".into(),
                    },
                ),
            ]),
        );

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("operator widget render should not panic");
        assert_eq!(app.layout.matrix_widget_hits.len(), 3);
        let text = rendered_text(term.backend().buffer());
        assert!(text.contains("operator ─"));
        assert!(
            text.contains("■"),
            "selected widget indicator should be filled"
        );
        assert!(
            !text.contains("2/3"),
            "widget viewport title should not include widget count"
        );
        server.abort();
    }

    #[tokio::test]
    async fn matrix_widget_hover_previews_over_pin_then_reverts() {
        use agentd_protocol::{UiPanel, UiPlacement};
        use std::time::{Duration, Instant};

        let (mut app, _dir, server) = captured_app().await;
        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch".into();
        app.sessions.push(orch);
        app.refresh_orchestrator_id();
        let panel = |id: &str, at: u64| {
            (
                id.to_string(),
                UiPanel {
                    id: id.into(),
                    source: Some(format!("{id}.md")),
                    title: Some(id.into()),
                    created_at_ms: at,
                    placement: UiPlacement::Sticky,
                    markdown: format!("# {id}"),
                },
            )
        };
        app.ui_panels.insert(
            "orch".into(),
            HashMap::from([panel("alpha", 1), panel("beta", 2)]),
        );

        let now = Instant::now();
        // Click pins alpha — persistent, no expiry.
        app.toggle_matrix_widget_panel("alpha".into());
        assert_eq!(app.matrix_widget_pinned.as_deref(), Some("alpha"));
        assert_eq!(app.matrix_widget_shown(now).as_deref(), Some("alpha"));

        // Hovering beta previews it over the pinned alpha.
        app.matrix_widget_hover = Some(MatrixWidgetHover {
            panel_id: "beta".into(),
            until: now + Duration::from_millis(DYNAMIC_UI_HOVER_GRACE_MS),
        });
        assert_eq!(app.matrix_widget_shown(now).as_deref(), Some("beta"));
        assert!(app.matrix_widget_visible(now));

        // Once the grace lapses, it reverts to the pinned alpha, and the lapsed
        // hover is cleared as a side effect of the visibility check.
        let later = now + Duration::from_secs(2);
        assert!(app.matrix_widget_visible(later));
        assert_eq!(app.matrix_widget_shown(later).as_deref(), Some("alpha"));
        assert!(app.matrix_widget_hover.is_none());

        // Clicking alpha again unpins it — nothing shown, viewport hidden.
        app.toggle_matrix_widget_panel("alpha".into());
        assert!(app.matrix_widget_pinned.is_none());
        assert!(!app.matrix_widget_visible(later));
        assert!(app.matrix_widget_shown(later).is_none());

        server.abort();
    }

    #[tokio::test]
    async fn matrix_widget_hover_keeps_title_icon_outline_until_pinned() {
        use agentd_protocol::{UiPanel, UiPlacement};

        let (mut app, _dir, server) = captured_app().await;
        let mut orch = summary_with_kind(agentd_protocol::SessionKind::Orchestrator);
        orch.id = "orch".into();
        app.sessions.push(orch);
        app.refresh_orchestrator_id();
        app.matrix_rain_hidden = false;
        app.ui_panels.insert(
            "orch".into(),
            HashMap::from([(
                "alpha".into(),
                UiPanel {
                    id: "alpha".into(),
                    source: Some("alpha.md".into()),
                    title: Some("alpha".into()),
                    created_at_ms: 1,
                    placement: UiPlacement::Sticky,
                    markdown: "# alpha".into(),
                },
            )]),
        );

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("operator widget title indicator should render");
        let hit = app
            .layout
            .matrix_widget_hits
            .first()
            .cloned()
            .expect("widget title hit");

        app.mouse_pos = Some((hit.start_col, hit.row));
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("hovered operator widget title indicator should render");
        assert_eq!(
            term.backend()
                .buffer()
                .cell((hit.start_col, hit.row))
                .map(|cell| cell.symbol()),
            Some("□"),
            "hover preview should keep the operator widget title icon outlined"
        );
        assert_eq!(
            app.matrix_widget_hover
                .as_ref()
                .map(|h| h.panel_id.as_str()),
            Some("alpha")
        );

        app.toggle_matrix_widget_panel("alpha".into());
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("pinned operator widget title indicator should render");
        assert_eq!(
            term.backend()
                .buffer()
                .cell((hit.start_col, hit.row))
                .map(|cell| cell.symbol()),
            Some("■"),
            "clicked/pinned operator widget title icon should be filled"
        );
        server.abort();
    }

    #[tokio::test]
    async fn dynamic_ui_hover_reveals_only_the_hovered_session_panel() {
        use std::time::{Duration, Instant};

        let (mut app, _dir, server) = captured_app().await;
        // Nothing pinned / temporarily revealed → hidden.
        assert!(!app.dynamic_ui_panel_visible("s1", "w1"));

        let now = Instant::now();
        app.dynamic_ui_hover = Some(DynamicUiHover {
            session_id: "s1".into(),
            panel_id: "w1".into(),
            until: now + Duration::from_millis(DYNAMIC_UI_HOVER_GRACE_MS),
        });
        // Visible for exactly the hovered (session, panel), nothing else.
        assert!(app.dynamic_ui_panel_visible("s1", "w1"));
        assert!(!app.dynamic_ui_panel_visible("s1", "w2"));
        assert!(!app.dynamic_ui_panel_visible("s2", "w1"));

        // A lapsed hover stops revealing the panel.
        app.dynamic_ui_hover = Some(DynamicUiHover {
            session_id: "s1".into(),
            panel_id: "w1".into(),
            until: now.checked_sub(Duration::from_millis(1)).unwrap_or(now),
        });
        assert!(!app.dynamic_ui_panel_visible("s1", "w1"));

        server.abort();
    }

    #[tokio::test]
    async fn collapsed_matrix_rain_shows_title_bar_with_expand_button() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = captured_app().await;
        app.matrix_rain_hidden = true; // collapsed → only the title bar shows

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("collapsed rain title bar should render");
        let text = rendered_text(term.backend().buffer());
        assert!(
            text.contains("operator"),
            "collapsed panel should keep its title bar: {text:?}"
        );

        let rain = app
            .layout
            .matrix_rain_area
            .expect("collapsed rain title bar area");
        assert_eq!(
            rain.height, 1,
            "collapsed panel should be a 1-row title bar"
        );

        let (x_start, _x_end, y) =
            crate::ui::matrix_rain_close_button_range(rain).expect("expand button hitbox");

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        assert!(
            !app.matrix_rain_hidden,
            "clicking + on the collapsed title bar should expand the panel"
        );

        server.abort();
    }

    #[tokio::test]
    async fn operator_title_marks_pending_approval_and_toggles_panel_on_click() {
        use agentd_protocol::SessionKind;
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = captured_app().await;
        let mut orch = summary_with_kind(SessionKind::Orchestrator);
        orch.id = "orch".into();
        app.sessions.push(orch);
        app.refresh_orchestrator_id();
        app.matrix_rain_hidden = false;
        app.pending_tool_approvals
            .insert("orch".into(), HashSet::from(["call-1".to_string()]));

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("operator approval title should render");
        let text = rendered_text(term.backend().buffer());
        assert!(text.contains("operator !"));
        let (x_start, _x_end, y) = app
            .layout
            .matrix_operator_title_hit
            .expect("operator title hitbox");

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        assert!(app.is_orchestrator_panel_open());

        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: x_start,
            row: y,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;
        assert!(!app.is_orchestrator_panel_open());
        server.abort();
    }

    // Regression: PR #480 forwards mouse events into a pane whose child grabbed
    // the mouse (e.g. Claude Code fullscreen), returning early from `on_mouse`.
    // That early return skipped construct's click-to-focus, so clicking inside
    // such a pane reached the child but never moved keyboard focus there — only
    // clicking the title bar (a border, never forwarded) would. A button press
    // must focus the pane *and* forward.
    #[tokio::test]
    async fn click_in_mouse_grabbing_pane_still_focuses_it() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let (mut app, _dir, server) = captured_app().await;
        app.view = ViewMode::Terminal;

        // The child enables mouse press tracking (DECSET ?1000h); replay caches
        // the parser so `mouse_protocol_mode()` reflects it.
        let mut history = crate::pty_render::ItemHistory::new();
        history.feed_pty(b"\x1b[?1000h");
        let _ = history.replay(80, 24, 0);
        app.histories.insert("s1".into(), history);

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("terminal view should render");

        // Precondition: the pane's child is tracking the mouse, so the click
        // takes the forwarding path (not construct's normal click handling).
        assert_ne!(
            app.histories.get("s1").unwrap().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "test setup must put the pane's child in a mouse-tracking mode"
        );
        let inner = app
            .layout
            .main_window_areas
            .first()
            .expect("rendered session pane")
            .inner_area;
        assert!(
            inner.width > 0 && inner.height > 0,
            "pane has a content area"
        );

        // Focus the list, then click in the middle of the pane's content area.
        app.focus = PaneFocus::List;
        app.on_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: inner.x + inner.width / 2,
            row: inner.y + inner.height / 2,
            modifiers: crossterm::event::KeyModifiers::empty(),
        })
        .await;

        // Even though the click was forwarded into the child, focus followed it.
        assert_eq!(
            app.focus,
            PaneFocus::View,
            "a click inside a mouse-grabbing pane must focus the pane"
        );
        assert_eq!(app.active_window_id, 1);

        server.abort();
    }

    #[tokio::test]
    async fn session_widgets_render_as_title_bar_indicators_by_creation_time() {
        use agentd_protocol::{UiPanel, UiPlacement};

        let (mut app, _dir, server) = captured_app().await;
        app.dynamic_ui_selected
            .insert(("s1".into(), "newer".into()));
        app.ui_panels.insert(
            "s1".into(),
            HashMap::from([
                (
                    "newer".into(),
                    UiPanel {
                        id: "newer".into(),
                        source: Some("newer.md".into()),
                        title: Some("Newer".into()),
                        created_at_ms: 20,
                        placement: UiPlacement::Sticky,
                        markdown: "# Newer".into(),
                    },
                ),
                (
                    "older".into(),
                    UiPanel {
                        id: "older".into(),
                        source: Some("older.md".into()),
                        title: Some("Older".into()),
                        created_at_ms: 10,
                        placement: UiPlacement::Sticky,
                        markdown: "# Older".into(),
                    },
                ),
            ]),
        );

        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| crate::ui::render(f, &mut app))
            .expect("session widget title indicators should render");

        let hits: Vec<_> = app.layout.dynamic_ui_widget_hits.clone();
        let hit_ids: Vec<_> = hits.iter().map(|hit| hit.panel_id.as_str()).collect();
        assert_eq!(hit_ids, vec!["older", "newer"]);
        let buf = term.backend().buffer();
        let text = rendered_text(buf);
        assert!(text.contains("□"));
        assert!(text.contains("■"));
        // Each registered hit cell must paint the visible □/■ glyph — the
        // session title bar's hover/click hitbox aligns exactly with the
        // square the user sees, with no off-by-one aiming gap.
        for hit in &hits {
            let glyph = buf
                .cell((hit.start_col, hit.row))
                .map(|c| c.symbol().to_string())
                .unwrap_or_default();
            assert!(
                glyph == "□" || glyph == "■",
                "widget hit {hit:?} must land on the visible glyph, got {glyph:?}"
            );
        }
        server.abort();
    }

    #[tokio::test]
    async fn matrix_rain_paints_browser_preview_wallpaper() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut s1 = summary_with_kind(agentd_protocol::SessionKind::User);
        s1.id = "s1".into();
        let mut app = test_app(client, vec![s1]);
        app.matrix_rain_hidden = false;

        let count_wallpaper_cells = |app: &mut App| -> usize {
            let backend = ratatui::backend::TestBackend::new(140, 44);
            let mut term = ratatui::Terminal::new(backend).expect("terminal");
            term.draw(|f| crate::ui::render(f, app)).expect("draw");
            let area = app
                .layout
                .matrix_rain_area
                .expect("matrix rain area rendered");
            let buf = term.backend().buffer();
            let mut n = 0;
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    // The quadrant wallpaper is the only thing that sets an
                    // Rgb *background*; the rain only ever sets fg.
                    if matches!(
                        buf.cell((x, y)).map(|c| c.style().bg),
                        Some(Some(ratatui::style::Color::Rgb(..)))
                    ) {
                        n += 1;
                    }
                }
            }
            n
        };

        // No preview → no thumbnail cells in the rain (the rain only
        // sets fg colors, never an Rgb background).
        assert_eq!(count_wallpaper_cells(&mut app), 0);

        // Insert a preview for "s1", which is the session currently visible
        // in the main window (see test_app). The thumbnail must be suppressed
        // in the rain area — the user can already see it in their session pane.
        app.browser_previews.insert(
            "s1".into(),
            BrowserPreviewState {
                hide_after: Instant::now() + Duration::from_secs(60),
                hover_started: None,
                decoded: Some(std::sync::Arc::new(image::RgbaImage::from_pixel(
                    32,
                    24,
                    image::Rgba([180, 40, 40, 255]),
                ))),
                revealed_at: Instant::now() - Duration::from_secs(10),
            },
        );
        assert_eq!(
            count_wallpaper_cells(&mut app),
            0,
            "preview for a session visible in the main view must be suppressed in the rain area"
        );

        // A preview from a session NOT visible in the main window IS shown
        // in the rain area as a foreground thumbnail.
        app.browser_previews.insert(
            "s-other".into(),
            BrowserPreviewState {
                hide_after: Instant::now() + Duration::from_secs(60),
                hover_started: None,
                decoded: Some(std::sync::Arc::new(image::RgbaImage::from_pixel(
                    32,
                    24,
                    image::Rgba([40, 180, 40, 255]),
                ))),
                revealed_at: Instant::now() - Duration::from_secs(10),
            },
        );
        assert!(
            count_wallpaper_cells(&mut app) > 0,
            "non-visible session's preview should paint as foreground thumbnail in the rain area"
        );

        // Preview removed → thumbnail gone again.
        app.browser_previews.clear();
        assert_eq!(
            count_wallpaper_cells(&mut app),
            0,
            "thumbnail must vanish when the preview is gone"
        );
        server.abort();
    }

    #[test]
    fn browser_preview_image_decodes_once() {
        use base64::Engine;
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode png");
        let b64 = base64::engine::general_purpose::STANDARD.encode(buf.into_inner());

        let decoded = decode_browser_preview_image(&b64).expect("valid png decodes");
        assert_eq!(decoded.dimensions(), (3, 2));
        assert_eq!(decoded.get_pixel(0, 0).0, [10, 20, 30, 255]);

        // Garbage in → None (no panic), so a bad preview just renders nothing.
        assert!(decode_browser_preview_image("not-base64-@@@").is_none());
    }

    #[test]
    fn only_user_sessions_are_visible_list_items() {
        assert!(is_user_list_session(&summary_with_kind(
            agentd_protocol::SessionKind::User
        )));
        assert!(!is_user_list_session(&summary_with_kind(
            agentd_protocol::SessionKind::Orchestrator
        )));
        assert!(!is_user_list_session(&summary_with_kind(
            agentd_protocol::SessionKind::Subagent
        )));
    }

    #[tokio::test]
    async fn subagents_render_under_parent_and_default_expanded() {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");
        let mut parent = summary_with_kind(agentd_protocol::SessionKind::User);
        parent.id = "sparent".into();
        parent.position = 0;
        let mut child = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        child.id = "schild".into();
        child.parent_session_id = Some("sparent".into());
        child.position = 1;
        let mut orphan = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        orphan.id = "sorphan".into();
        orphan.position = -1;

        let mut app = test_app(client, vec![orphan, child, parent]);
        let items = app.list_items();
        assert_eq!(items.len(), 2);
        match &items[0] {
            ListItem::Session {
                summary,
                indented,
                has_children,
                children_expanded,
            } => {
                assert_eq!(summary.id, "sparent");
                assert!(!indented);
                assert!(*has_children);
                assert!(*children_expanded);
            }
            _ => panic!("expected parent session"),
        }
        match &items[1] {
            ListItem::Session {
                summary,
                indented,
                has_children,
                ..
            } => {
                assert_eq!(summary.id, "schild");
                assert!(*indented);
                assert!(!has_children);
            }
            _ => panic!("expected subagent session"),
        }

        app.selection = Selection::Session("sparent".into());
        app.focus = PaneFocus::List;
        app.run_action(KeyAction::CollapseGroup).await;
        let collapsed_by_key = app.list_items();
        assert_eq!(collapsed_by_key.len(), 1);
        app.run_action(KeyAction::ExpandGroup).await;
        assert_eq!(app.list_items().len(), 2);

        app.subagent_collapsed.insert("sparent".into());
        let collapsed = app.list_items();
        assert_eq!(collapsed.len(), 1);
        match &collapsed[0] {
            ListItem::Session {
                summary,
                has_children,
                children_expanded,
                ..
            } => {
                assert_eq!(summary.id, "sparent");
                assert!(*has_children);
                assert!(!children_expanded);
            }
            _ => panic!("expected collapsed parent session"),
        }
    }

    #[tokio::test]
    async fn archived_subagents_render_under_parent_archived_row() {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut parent = summary_with_kind(agentd_protocol::SessionKind::User);
        parent.id = "sparent".into();
        parent.position = 0;
        let mut active_child = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        active_child.id = "sactive-child".into();
        active_child.parent_session_id = Some("sparent".into());
        active_child.position = 0;
        let mut archived_child = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        archived_child.id = "sarchived-child".into();
        archived_child.parent_session_id = Some("sparent".into());
        archived_child.position = 1;
        archived_child.archived = true;

        let mut app = test_app(client, vec![archived_child, active_child, parent]);
        let items = app.list_items();
        assert_eq!(items.len(), 3);
        assert!(
            matches!(&items[0], ListItem::Session { summary, has_children: true, children_expanded: true, .. } if summary.id == "sparent")
        );
        assert!(
            matches!(&items[1], ListItem::Session { summary, indented: true, .. } if summary.id == "sactive-child"),
            "active subagent should remain ungrouped under its parent",
        );
        match &items[2] {
            ListItem::ArchivedRow {
                section,
                count,
                expanded,
                indented,
            } => {
                assert_eq!(*section, ArchiveSection::Subagents("sparent".into()));
                assert_eq!(*count, 1);
                assert!(!*expanded, "archived subagent row starts collapsed");
                assert!(*indented, "archived subagent row is nested under parent");
            }
            other => panic!("expected archived subagent disclosure row, got {other:?}"),
        }

        app.focus = PaneFocus::List;
        app.selection = Selection::ArchivedRow(ArchiveSection::Subagents("sparent".into()));
        app.run_action(KeyAction::ExpandGroup).await;
        let expanded = app.list_items();
        assert_eq!(expanded.len(), 4);
        assert!(matches!(
            &expanded[2],
            ListItem::ArchivedRow { expanded: true, .. }
        ));
        assert!(
            matches!(&expanded[3], ListItem::Session { summary, indented: true, .. } if summary.id == "sarchived-child" && summary.archived),
            "expanded archived subagent should follow its disclosure row",
        );

        app.run_action(KeyAction::CollapseGroup).await;
        assert_eq!(app.list_items().len(), 3);
        assert_eq!(
            app.selection,
            Selection::ArchivedRow(ArchiveSection::Subagents("sparent".into()))
        );
    }

    #[tokio::test]
    async fn list_items_hides_archived_behind_expandable_row() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut active = summary_with_kind(agentd_protocol::SessionKind::User);
        active.id = "active".into();
        active.position = 0;
        let mut archived = summary_with_kind(agentd_protocol::SessionKind::User);
        archived.id = "archived".into();
        archived.position = 1;
        archived.archived = true;

        let mut app = test_app(client, vec![active, archived]);

        // Collapsed by default: the active session plus a "1 archived" row.
        let items = app.list_items();
        assert_eq!(items.len(), 2);
        assert!(
            matches!(&items[0], ListItem::Session { summary, .. } if summary.id == "active"),
            "active session should render directly",
        );
        match &items[1] {
            ListItem::ArchivedRow {
                section,
                count,
                expanded,
                ..
            } => {
                assert_eq!(*section, ArchiveSection::Ungrouped);
                assert_eq!(*count, 1);
                assert!(!*expanded, "archived row starts collapsed");
            }
            other => panic!("expected an archived row, got {other:?}"),
        }

        // Reveal: active session, the open row, then the archived session.
        app.toggle_archive_section(&ArchiveSection::Ungrouped);
        let items = app.list_items();
        assert_eq!(items.len(), 3);
        assert!(matches!(
            &items[1],
            ListItem::ArchivedRow { expanded: true, .. }
        ));
        assert!(
            matches!(&items[2], ListItem::Session { summary, .. } if summary.id == "archived"),
            "revealed archived session should follow its row",
        );

        // Toggle back off: collapsed again.
        app.toggle_archive_section(&ArchiveSection::Ungrouped);
        assert_eq!(app.list_items().len(), 2);
    }

    #[tokio::test]
    async fn archived_row_is_navigable_and_arrows_expand_collapse_it() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut active = summary_with_kind(agentd_protocol::SessionKind::User);
        active.id = "active".into();
        active.position = 0;
        let mut archived = summary_with_kind(agentd_protocol::SessionKind::User);
        archived.id = "arch".into();
        archived.position = 1;
        archived.archived = true;

        let mut app = test_app(client, vec![active, archived]);
        app.focus = PaneFocus::List;
        app.select_session("active".into());

        // Down lands on the archived row — it's a navigation stop now, not skipped.
        app.run_action(KeyAction::NextSession).await;
        assert_eq!(
            app.selection,
            Selection::ArchivedRow(ArchiveSection::Ungrouped)
        );

        // Right (ExpandGroup) reveals the section's archived sessions.
        app.run_action(KeyAction::ExpandGroup).await;
        assert!(app.show_archived_ungrouped);
        assert!(
            app.list_items()
                .iter()
                .any(|it| matches!(it, ListItem::Session { summary, .. } if summary.id == "arch")),
            "expand should reveal the archived session",
        );

        // Left (CollapseGroup) hides them again; the row stays selected.
        app.run_action(KeyAction::CollapseGroup).await;
        assert!(!app.show_archived_ungrouped);
        assert_eq!(
            app.selection,
            Selection::ArchivedRow(ArchiveSection::Ungrouped)
        );
    }

    #[tokio::test]
    async fn next_session_skips_collapsed_top_level_archived_row_before_project() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut top_level = summary_with_kind(agentd_protocol::SessionKind::User);
        top_level.id = "top-level".into();
        top_level.position = 0;
        let mut archived = summary_with_kind(agentd_protocol::SessionKind::User);
        archived.id = "archived".into();
        archived.position = 1;
        archived.archived = true;
        let mut project_member = summary_with_kind(agentd_protocol::SessionKind::User);
        project_member.id = "project-member".into();
        project_member.group_id = Some("project".into());

        let mut app = test_app(client, vec![top_level, archived, project_member]);
        app.groups = vec![GroupSummary {
            id: "project".into(),
            name: "Project".into(),
            created_at: chrono::Utc::now(),
            position: 0,
            collapsed: false,
        }];
        app.focus = PaneFocus::List;
        app.select_session("top-level".into());

        let items = app.list_items();
        assert_eq!(items.len(), 4);
        assert!(matches!(
            &items[0],
            ListItem::Session { summary, .. } if summary.id == "top-level"
        ));
        assert!(matches!(
            &items[1],
            ListItem::ArchivedRow {
                section: ArchiveSection::Ungrouped,
                expanded: false,
                ..
            }
        ));
        assert!(matches!(
            &items[2],
            ListItem::GroupHeader { group, .. } if group.id == "project"
        ));

        app.run_action(KeyAction::NextSession).await;
        assert_eq!(app.selection, Selection::Group("project".into()));
    }

    #[tokio::test]
    async fn archived_section_resolves_only_its_archived_members() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let _server = tokio::spawn(async move {
            loop {
                if listener.accept().await.is_err() {
                    break;
                }
            }
        });
        let client = Client::connect(&sock).await.expect("client connects");

        // Ungrouped: one active + one archived.
        let mut ung_active = summary_with_kind(agentd_protocol::SessionKind::User);
        ung_active.id = "ung_active".into();
        let mut ung_arch = summary_with_kind(agentd_protocol::SessionKind::User);
        ung_arch.id = "ung_arch".into();
        ung_arch.archived = true;

        // Project "p1": one active + one archived member.
        let mut grp_active = summary_with_kind(agentd_protocol::SessionKind::User);
        grp_active.id = "grp_active".into();
        grp_active.group_id = Some("p1".into());
        let mut grp_arch = summary_with_kind(agentd_protocol::SessionKind::User);
        grp_arch.id = "grp_arch".into();
        grp_arch.group_id = Some("p1".into());
        grp_arch.archived = true;

        // Subagents of "ung_active": one active + one archived.
        let mut sub_active = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        sub_active.id = "sub_active".into();
        sub_active.parent_session_id = Some("ung_active".into());
        let mut sub_arch = summary_with_kind(agentd_protocol::SessionKind::Subagent);
        sub_arch.id = "sub_arch".into();
        sub_arch.parent_session_id = Some("ung_active".into());
        sub_arch.archived = true;

        let mut app = test_app(
            client,
            vec![
                ung_active, ung_arch, grp_active, grp_arch, sub_active, sub_arch,
            ],
        );
        app.groups = vec![GroupSummary {
            id: "p1".into(),
            name: "Project One".into(),
            created_at: chrono::Utc::now(),
            position: 0,
            collapsed: false,
        }];

        // Each section resolves exactly its own archived members — never the
        // active ones, and never another section's.
        assert_eq!(
            app.archived_sessions_in_section(&ArchiveSection::Ungrouped),
            vec!["ung_arch".to_string()],
        );
        assert_eq!(
            app.archived_sessions_in_section(&ArchiveSection::Group("p1".into())),
            vec!["grp_arch".to_string()],
        );
        assert_eq!(
            app.archived_sessions_in_section(&ArchiveSection::Subagents("ung_active".into())),
            vec!["sub_arch".to_string()],
        );

        // The label distinguishes a project by name from the ungrouped run.
        assert_eq!(
            app.archive_section_label(&ArchiveSection::Group("p1".into())),
            "project 'Project One'",
        );
        assert_eq!(
            app.archive_section_label(&ArchiveSection::Ungrouped),
            "ungrouped",
        );
    }

    #[test]
    fn list_session_indent_policy_distinguishes_subagents_and_grouped_parents() {
        let user = summary_with_kind(agentd_protocol::SessionKind::User);
        let subagent = summary_with_kind(agentd_protocol::SessionKind::Subagent);

        assert_eq!(list_session_indent_cells(&user, false, false), 0);
        assert_eq!(list_session_indent_cells(&user, true, false), 2);
        assert_eq!(list_session_indent_cells(&user, true, true), 1);
        assert_eq!(list_session_indent_cells(&subagent, true, false), 4);
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
        let sock = dir.path().join("construct.sock");
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

    #[tokio::test]
    async fn large_tui_paste_uploads_attachment_and_inserts_reference() {
        use agentd_client::Client;
        use agentd_protocol::ipc_method;
        use base64::Engine as _;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;
        use tokio::sync::oneshot;

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let (seen_tx, seen_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            let Ok(n) = reader.read_line(&mut line).await else {
                return;
            };
            if n == 0 {
                return;
            }
            let req: Value = serde_json::from_str(&line).expect("json request");
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let params = req.get("params").cloned().unwrap_or(Value::Null);
            if method == ipc_method::SESSION_ATTACH_CLIPBOARD {
                let _ = seen_tx.send(params.clone());
            }
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "path": "/tmp/clipboard.txt",
                    "reference": "[#file:/tmp/clipboard.txt]"
                }
            });
            let _ = writer.write_all((resp.to_string() + "\n").as_bytes()).await;
        });

        let client = Client::connect(&sock).await.expect("client connects");
        let mut app = test_app(
            client,
            vec![summary_with_kind(agentd_protocol::SessionKind::User)],
        );
        app.minibuffer = Some(Minibuffer {
            prompt: "Input: ".into(),
            input: "see ".into(),
            cursor: 4,
            intent: MinibufferIntent::SendInput {
                session_id: "s1".into(),
            },
            error: None,
        });

        let paste = "x".repeat(LARGE_TEXT_PASTE_CHARS);
        app.on_term_event(CtEvent::Paste(paste.clone())).await;

        let params = tokio::time::timeout(std::time::Duration::from_secs(1), seen_rx)
            .await
            .expect("attach request should reach mock daemon")
            .expect("attach params");
        assert_eq!(params["session_id"], "s1");
        assert_eq!(params["filename"], "clipboard.txt");
        assert_eq!(params["mime"], "text/plain");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(params["data"].as_str().expect("base64 data"))
            .expect("decode attachment");
        assert_eq!(decoded, paste.as_bytes());
        assert_eq!(
            app.minibuffer.as_ref().map(|mb| mb.input.as_str()),
            Some("see [#file:/tmp/clipboard.txt]")
        );

        server.abort();
    }

    /// A paste forwarded to a PTY child is wrapped in `ESC[200~` /
    /// `ESC[201~` only when that child has DEC bracketed-paste mode
    /// enabled — the framing a real terminal supplies. This is what lets
    /// claude code recognize a dragged image path as a paste (→ `[image
    /// #N]`) instead of literal typed text. Children that never enable
    /// mode 2004 still get raw bytes, preserving the prior behavior.
    #[tokio::test]
    async fn paste_wraps_in_bracketed_markers_only_when_child_enables_mode_2004() {
        let (mut app, _dir, server) = captured_app().await;

        // Child has not enabled bracketed paste yet → forwarded raw.
        let raw = app.encode_paste_for_pty("s1", "/tmp/cat.png".into());
        assert_eq!(raw, b"/tmp/cat.png".to_vec());

        // Child enables DEC mode 2004 (claude code does this on startup).
        let h = app
            .histories
            .entry("s1".into())
            .or_insert_with(crate::pty_render::ItemHistory::new);
        h.feed_pty(b"\x1b[?2004h");
        let _ = h.replay(80, 24, 0);

        let wrapped = app.encode_paste_for_pty("s1", "/tmp/cat.png".into());
        assert_eq!(wrapped, b"\x1b[200~/tmp/cat.png\x1b[201~".to_vec());

        // An embedded end-marker is stripped so it can't end the paste early.
        let injected = app.encode_paste_for_pty("s1", "a\x1b[201~b".into());
        assert_eq!(injected, b"\x1b[200~ab\x1b[201~".to_vec());

        server.abort();
    }

    // Typing into a smith prompt grows the editor pane, shrinking the
    // chat area. The chat parser must stay at the full pane height so
    // editor growth never resizes (and O(history)-rebuilds) it — that
    // rebuild-per-keystroke was the typing lag. Structural, timing-free.
    #[tokio::test]
    async fn smith_editor_growth_does_not_resize_chat_parser() {
        use agentd_client::Client;
        use tokio::net::UnixListener;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let _ = listener.accept().await;
            futures::future::pending::<()>().await;
        });
        let client = Client::connect(&sock).await.expect("client connects");

        let mut summary = summary_with_kind(agentd_protocol::SessionKind::User);
        summary.harness = "smith".into();
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

        assert!(
            small.is_some() && small == big,
            "editor growth resized the chat parser: {small:?} -> {big:?}"
        );

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

    fn session_detail_summary_json(id: &str, has_pty: bool) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "harness": "shell",
            "cwd": "/tmp",
            "state": "running",
            "created_at": "2026-05-21T00:00:00Z",
            "pending_input": false,
            "event_count": 0,
            "has_pty": has_pty,
            "pinned": false,
            "position": 0,
            "approval_mode": "manual",
            "kind": "user"
        })
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
        let sock = dir.path().join("construct.sock");
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
                            ipc_method::SESSION_GET => {
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
                                serde_json::json!({
                                    "summary": session_detail_summary_json("s-big", false),
                                    "events": events,
                                    "ui_panels": []
                                })
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
            is_headless: false,
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

    /// REGRESSION: a headless session's conversation must be reconstructed
    /// when the TUI re-attaches (relaunch) or the daemon restarts. Headless
    /// harnesses carry their prose as structured `Message` / `Reasoning`
    /// events with no PTY, so `load_session_hydration` must replay them into
    /// the history when the session `is_headless` — otherwise the session
    /// renders blank after a restart. Exercises the full hydration path,
    /// including the `SessionHydrationRequest.is_headless` plumbing, against
    /// a mock daemon serving a PTY-less transcript.
    #[tokio::test]
    async fn headless_session_history_reconstructed_on_hydration() {
        use agentd_protocol::ipc_method;
        use serde_json::Value;
        use tempfile::tempdir;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let dir = tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
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
                            ipc_method::SESSION_GET => {
                                // Headless transcript: assistant + reasoning
                                // deltas, no PTY events at all.
                                let ev = |seq: u64, event: Value| {
                                    serde_json::json!({
                                        "seq": seq, "at": "2026-05-21T00:00:00Z", "event": event
                                    })
                                };
                                let events = vec![
                                    ev(
                                        1,
                                        serde_json::json!({
                                            "type": "reasoning", "text": "considering options"
                                        }),
                                    ),
                                    ev(
                                        2,
                                        serde_json::json!({
                                            "type": "message", "role": "assistant", "text": "Hello "
                                        }),
                                    ),
                                    ev(
                                        3,
                                        serde_json::json!({
                                            "type": "message", "role": "assistant",
                                            "text": "from headless"
                                        }),
                                    ),
                                ];
                                serde_json::json!({
                                    "summary": session_detail_summary_json("s-headless", false),
                                    "events": events,
                                    "ui_panels": []
                                })
                            }
                            ipc_method::SESSION_PTY_REPLAY => {
                                serde_json::json!({"data": "", "size": {"cols": 80, "rows": 24}})
                            }
                            _ => Value::Null,
                        };
                        let resp = serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": result,
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

        fn screen_text(h: &mut crate::pty_render::ItemHistory) -> String {
            let out = h.replay(80, 24, 0);
            (0..24u16)
                .flat_map(|r| {
                    let mut row = String::new();
                    for c in 0..80u16 {
                        if let Some(cell) = out.screen.cell(r, c) {
                            row.push_str(&cell.contents());
                        }
                    }
                    row.push('\n');
                    row.chars().collect::<Vec<_>>()
                })
                .collect()
        }

        // Headless → the prose is folded back into history on hydration.
        let loaded = load_session_hydration(SessionHydrationRequest {
            socket: sock.clone(),
            session_id: "s-headless".to_string(),
            needs_history: true,
            terminal_pane_size: (80, 24),
            is_headless: true,
        })
        .await
        .expect("headless hydration should succeed");
        let mut h = loaded
            .history
            .expect("headless hydration must produce a history");
        let text = screen_text(&mut h);
        assert!(
            text.contains("Hello from headless"),
            "headless prose must be reconstructed on hydration:\n{text}"
        );
        assert!(
            text.contains("considering options"),
            "headless reasoning must be reconstructed on hydration:\n{text}"
        );

        // Contrast: a PTY-backed session ignores the same Message/Reasoning
        // events — that prose lives in the PTY stream, so re-rendering it
        // from the transcript would double it up.
        let loaded_pty = load_session_hydration(SessionHydrationRequest {
            socket: sock.clone(),
            session_id: "s-pty".to_string(),
            needs_history: true,
            terminal_pane_size: (80, 24),
            is_headless: false,
        })
        .await
        .expect("hydration should succeed");
        if let Some(mut h) = loaded_pty.history {
            let text = screen_text(&mut h);
            assert!(
                !text.contains("Hello from headless"),
                "PTY-backed hydration must NOT re-render transcript Message prose:\n{text}"
            );
        }

        server.abort();
    }

    /// REGRESSION: a TUI re-attaching to an existing smith session
    /// shows the tool blocks again. Current smith interactive
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
                    call_id: None,
                },
            ),
        ];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor: Option<EditorState> = None;
        let mut status: Option<AgentStatus> = None;
        let mut ui_panels = HashMap::new();
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor,
            &mut status,
            &mut ui_panels,
            false,
        );

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
             Without it, fresh-TUI bootstrap of an existing smith session \
             rebuilds history with no tool blocks. Got render:\n{text}",
        );
    }

    /// Headless sessions carry their conversation as structured
    /// Message/Reasoning events (no PTY). Replay must fold them into the
    /// items history when the session is headless, and ignore them when
    /// it's PTY-backed (the prose is already in the PTY stream there).
    #[test]
    fn transcript_replay_renders_messages_only_when_headless() {
        use agentd_protocol::{MessageRole, SessionEvent, TimestampedEvent};
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
                SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: "answer from a headless run".into(),
                },
            ),
            ev(
                2,
                SessionEvent::Reasoning {
                    text: "some reasoning".into(),
                },
            ),
        ];

        let render = |is_headless: bool| {
            let mut history = crate::pty_render::ItemHistory::new();
            let mut editor: Option<EditorState> = None;
            let mut status: Option<agentd_protocol::AgentStatus> = None;
            apply_transcript_to_local_state(
                &events,
                &mut history,
                &mut editor,
                &mut status,
                &mut HashMap::new(),
                is_headless,
            );
            let out = history.replay(80, 24, 0);
            (0..24u16)
                .flat_map(|r| {
                    let mut row = String::new();
                    for c in 0..80u16 {
                        if let Some(cell) = out.screen.cell(r, c) {
                            row.push_str(&cell.contents());
                        }
                    }
                    row.push('\n');
                    row.chars().collect::<Vec<_>>()
                })
                .collect::<String>()
        };

        let headless = render(true);
        assert!(
            headless.contains("answer from a headless run"),
            "headless replay must render assistant prose:\n{headless}"
        );
        assert!(
            headless.contains("some reasoning"),
            "headless replay must render reasoning:\n{headless}"
        );

        let interactive = render(false);
        assert!(
            !interactive.contains("answer from a headless run"),
            "PTY-backed replay must NOT re-render Message prose (it's in the PTY):\n{interactive}"
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
                    call_id: None,
                },
            ),
            ev(4, SessionEvent::pty(b"after tool answer\r\n")),
        ];

        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor: Option<EditorState> = None;
        let mut status = None;
        let mut ui_panels = HashMap::new();
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor,
            &mut status,
            &mut ui_panels,
            false,
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
        let mut ui_panels = HashMap::new();
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor,
            &mut status,
            &mut ui_panels,
            false,
        );

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
    fn ui_panel_replay_tracks_create_patch_delete() {
        use agentd_protocol::{SessionEvent, TimestampedEvent, UiPanel, UiPlacement};
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
                SessionEvent::UiPanel(UiPanel {
                    id: "task".into(),
                    source: None,
                    title: Some("Task".into()),
                    created_at_ms: 0,
                    placement: UiPlacement::Sticky,
                    markdown: "old".into(),
                }),
            ),
            ev(
                2,
                SessionEvent::UiPanel(UiPanel {
                    id: "task".into(),
                    source: None,
                    title: Some("Task".into()),
                    created_at_ms: 0,
                    placement: UiPlacement::Sticky,
                    markdown: "new".into(),
                }),
            ),
            ev(
                3,
                SessionEvent::UiPanel(UiPanel {
                    id: "other".into(),
                    source: None,
                    title: None,
                    created_at_ms: 0,
                    placement: UiPlacement::Sticky,
                    markdown: "keep".into(),
                }),
            ),
            ev(4, SessionEvent::UiDelete { id: "task".into() }),
        ];
        let mut history = crate::pty_render::ItemHistory::new();
        let mut editor = None;
        let mut status = None;
        let mut panels = HashMap::new();
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor,
            &mut status,
            &mut panels,
            false,
        );
        assert!(!panels.contains_key("task"));
        assert_eq!(
            panels.get("other").map(|p| p.markdown.as_str()),
            Some("keep")
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
    fn selection_bounds_use_minibuffer_line_for_operator_area() {
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

    /// Symptom-level repro for the smith-prompt-overlap bug.
    ///
    /// User report (against `tui reconnect`, harness=smith): after
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
                    call_id: None,
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
        let mut ui_panels = HashMap::new();
        apply_transcript_to_local_state(
            &events,
            &mut history,
            &mut editor_state,
            &mut agent_status,
            &mut ui_panels,
            false,
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
    adjusted_scroll_offset(current, delta, SCROLLBACK_MAX)
}

fn adjusted_scroll_offset(current: usize, delta: i32, max_scroll: usize) -> usize {
    let next = current as i32 + delta;
    next.max(0).min(max_scroll as i32) as usize
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

fn json_escape(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".to_string())
        .trim_matches('"')
        .to_string()
}

fn markdown_display_rows(markdown: &str) -> usize {
    let mut rows = 0usize;
    let mut pending_actions = false;
    for raw in markdown.lines() {
        let line = raw.trim_end();
        if line.contains("](agentd:action/") {
            if !pending_actions {
                pending_actions = true;
                rows = rows.saturating_add(1);
            }
            continue;
        }
        pending_actions = false;
        rows = rows.saturating_add(1);
    }
    rows
}

fn parse_markdown_action_target(target: &str) -> (String, Option<String>, bool) {
    let Some((id, query)) = target.split_once('?') else {
        return (target.to_string(), None, false);
    };
    let mut key = None;
    let mut close = false;
    for part in query.split('&') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        if name == "key" && !value.is_empty() {
            key = Some(value.to_string());
        } else if name == "close" && matches!(value, "1" | "true" | "yes") {
            close = true;
        }
    }
    (id.to_string(), key, close)
}

fn markdown_actions(markdown: &str) -> Vec<agentd_protocol::UiAction> {
    let mut out = Vec::new();
    let mut rest = markdown;
    while let Some(label_start) = rest.find('[') {
        rest = &rest[label_start + 1..];
        let Some(label_end) = rest.find(']') else {
            break;
        };
        let label = &rest[..label_end];
        let after_label = &rest[label_end + 1..];
        let Some(after_open) = after_label.strip_prefix("(agentd:action/") else {
            rest = after_label;
            continue;
        };
        let Some(id_end) = after_open.find(')') else {
            break;
        };
        let (id, key, close) = parse_markdown_action_target(&after_open[..id_end]);
        if !label.is_empty() && !id.is_empty() {
            out.push(agentd_protocol::UiAction {
                id,
                label: label.to_string(),
                key,
                style: None,
                close,
            });
        }
        rest = &after_open[id_end + 1..];
    }
    out
}

#[cfg(test)]
mod widget_action_tests {
    use super::*;

    #[test]
    fn markdown_actions_parse_explicit_keys_only() {
        let actions = markdown_actions(
            "[Run checks](agentd:action/run-checks?key=r) [Start demo](agentd:action/start-demo)",
        );
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].id, "run-checks");
        assert_eq!(actions[0].key.as_deref(), Some("r"));
        assert_eq!(actions[1].id, "start-demo");
        assert_eq!(actions[1].key, None);
        assert!(!actions[1].close);
    }

    #[test]
    fn markdown_actions_parse_close_flag() {
        let actions = markdown_actions("[OK](agentd:action/ok?close=1&key=o)");

        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].id, "ok");
        assert_eq!(actions[0].key.as_deref(), Some("o"));
        assert!(actions[0].close);
    }
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

/// True when the just-handled input event should trigger the mouse
/// burst drain (which calls `now_or_never` on the input stream,
/// briefly poisoning crossterm's wake task). Only high-volume mouse
/// gestures qualify; gating like this keeps typing off the noop-waker
/// path. See the comment at the drain call-site in `run_loop` for the
/// full failure mode.
fn should_drain_after(ev: &CtEvent) -> bool {
    matches!(ev, CtEvent::Mouse(m) if drainable_mouse_burst_kind(&m.kind))
}

fn drainable_mouse_burst_kind(kind: &MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::Drag(crossterm::event::MouseButton::Left)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
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

    /// Wheel events join left drags in the burst drain so a fast
    /// wheel gesture coalesces before the next render instead of
    /// replaying one queued row at a time after the user stops.
    #[test]
    fn wheel_events_trigger_drain() {
        assert!(should_drain_after(&mouse(MouseEventKind::ScrollUp)));
        assert!(should_drain_after(&mouse(MouseEventKind::ScrollDown)));
    }

    #[test]
    fn low_volume_mouse_events_do_not_trigger_drain() {
        assert!(!should_drain_after(&mouse(MouseEventKind::Moved)));
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

    /// Zoomed mode renders without borders, so the content fills `view_area`
    /// starting at (x, y) rather than (x+1, y+1).  A URL at column 0 / row 0
    /// must be hit-testable when the bounds rect starts at the origin.
    #[test]
    fn url_hit_detects_url_at_first_row_and_col_of_borderless_area() {
        let frame = vec![
            "https://example.com/zoomed      ".to_string(),
            "                                ".to_string(),
        ];
        // Simulate zoomed-mode bounds: full area, no border shrink.
        let zoomed_bounds = Rect::new(0, 0, 32, 2);
        let hit = super::url_hit_in_frame(&frame, 0, 0, zoomed_bounds)
            .expect("URL at (col=0, row=0) must be clickable with borderless bounds");
        assert_eq!(hit.url, "https://example.com/zoomed");

        // With normal-mode bounds (border shrunk by 1 on each side), the same
        // click at (0, 0) falls outside the inner area and returns None.
        let bordered_bounds = Rect::new(1, 1, 30, 0);
        assert!(
            super::url_hit_in_frame(&frame, 0, 0, bordered_bounds).is_none(),
            "click at (0, 0) must not hit when bounds are border-shrunk"
        );
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

/// Outcome of the session kill prompt (`C-x k` / the view `x` button).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndChoice {
    /// `d` / `y` / `delete` — drop the transcript, worktree, and all on-disk state.
    Delete,
    /// `a` / `archive` — terminate the adapter but keep the session; it's
    /// hidden from the list until the "show archived" toggle is on, and can be
    /// restarted later.
    Archive,
    /// Anything else (`n`, empty Enter) — do nothing.
    Cancel,
}

pub fn parse_session_end_choice(input: &str) -> SessionEndChoice {
    match input.trim().to_lowercase().as_str() {
        "d" | "y" | "delete" => SessionEndChoice::Delete,
        "a" | "archive" => SessionEndChoice::Archive,
        _ => SessionEndChoice::Cancel,
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

#[cfg(test)]
mod session_end_prompt_tests {
    use super::{parse_session_end_choice, SessionEndChoice};

    #[test]
    fn d_or_y_or_delete_deletes() {
        for s in [
            "d", "D", "  d  ", "y", "Y", "  y  ", "delete", "DELETE", " Delete ",
        ] {
            assert_eq!(
                parse_session_end_choice(s),
                SessionEndChoice::Delete,
                "input {s:?} should delete",
            );
        }
    }

    #[test]
    fn a_or_archive_archives() {
        for s in ["a", "A", "  a  ", "archive", "ARCHIVE", " Archive "] {
            assert_eq!(
                parse_session_end_choice(s),
                SessionEndChoice::Archive,
                "input {s:?} should archive",
            );
        }
    }

    #[test]
    fn anything_else_cancels() {
        for s in ["", " ", "n", "N", "no", "x", "1"] {
            assert_eq!(
                parse_session_end_choice(s),
                SessionEndChoice::Cancel,
                "input {s:?} should cancel",
            );
        }
    }
}
