//! Reusable session-picker dialog (spec 0063).
//!
//! A modal overlay that lists every user session grouped exactly like the
//! session-list view — ungrouped sessions first, then each project behind its
//! header, then that section's archived sessions behind an "N archived" row.
//! A typeahead search *dims* non-matching sessions rather than removing them,
//! and auto-expands the project/archive groups that contain a match (and
//! auto-collapses the ones that don't), so the list never reflows out from
//! under the user. Navigation (Up/Down, `C-n`/`C-p`) walks only the visible,
//! non-dimmed sessions.
//!
//! The same dialog serves two callers, distinguished by [`SessionPickerPurpose`]:
//!   * `C-x b` opens it as a session switcher — confirming focuses the chosen
//!     session in the active window.
//!   * The program view's `@`→session path opens it as a clip picker —
//!     confirming inserts an `@{session:id}` clip into the program buffer via
//!     the existing smart-clip insertion path (which stays live underneath).

use super::*;

/// What confirming a pick does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPickerPurpose {
    /// `C-x b` — switch the active window's focus to the chosen session.
    Switch,
    /// Program-view `@` → session — insert an `@{session:id}` clip. The
    /// program's smart-clip search is left active so the existing insertion
    /// path can replace the `@…` token with the clip.
    InsertProgramClip,
}

/// Modal session-picker state. `App::session_picker == None` means closed.
#[derive(Debug, Clone)]
pub struct SessionPickerDialog {
    pub purpose: SessionPickerPurpose,
    /// Live typeahead query. Dims non-matching sessions and drives auto-expand.
    pub query: String,
    /// Char index into `query` where typing/backspace/Emacs motion acts. Only
    /// meaningful for [`SessionPickerPurpose::Switch`], which owns its own
    /// search line — the `@`→session variant's "query" is the program
    /// buffer's live token, tracked by the buffer's own cursor instead.
    pub cursor: usize,
    /// Logical index into the *selectable* (visible, non-dimmed) session rows.
    pub selected: usize,
    /// First visible raw-row index, clamped at render time to keep `selected`
    /// on screen.
    pub scroll: usize,
}

impl SessionPickerDialog {
    pub fn title(&self) -> &'static str {
        match self.purpose {
            SessionPickerPurpose::Switch => " switch session ",
            SessionPickerPurpose::InsertProgramClip => " insert session clip ",
        }
    }
}

/// A materialized row in the dialog body. Only [`SessionPickerRow::Session`]
/// rows with `dimmed == false` and [`SessionPickerRow::ProgramBlock`] rows are
/// selectable; headers are decoration.
#[derive(Debug, Clone)]
pub enum SessionPickerRow {
    /// A project/group header. `expanded` controls whether its members follow;
    /// `matches` is how many of its active members match the query.
    GroupHeader {
        name: String,
        expanded: bool,
        matches: usize,
    },
    /// An "N archived" disclosure row ending a section.
    ArchiveHeader {
        count: usize,
        expanded: bool,
        indented: bool,
    },
    /// A session row. Dimmed sessions stay visible but are not selectable.
    Session {
        summary: SessionSummary,
        indented: bool,
        dimmed: bool,
    },
    /// Separator row introducing the currently open program's blocks (see
    /// [`SessionPickerRow::ProgramBlock`]). Only present for the `C-x b`
    /// switcher, and only when a program is open.
    ProgramHeader,
    /// A block from the currently open program. Selecting it closes the
    /// picker, brings the program view into focus, and scrolls to the block.
    ProgramBlock { text: String, start_line: usize },
}

impl SessionPickerRow {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            SessionPickerRow::Session { dimmed: false, .. } | SessionPickerRow::ProgramBlock { .. }
        )
    }
}

/// Resolve the first visible raw-row index for the dialog body.
///
/// `rows` is the full materialized row list, `sel_raw` the raw index of the
/// highlighted (selectable) row, `prev_scroll` the persisted scroll offset, and
/// `visible` how many body rows fit on screen. The result keeps the selected row
/// on screen, scrolling the window just far enough in either direction.
///
/// Crucially, headers (a project name, an "N archived" disclosure) are *not*
/// selectable, so the topmost selectable session can sit one or more rows below
/// the top of the list. Anchoring the scroll to that session's raw index alone
/// would clamp the leading header(s) — e.g. the very first project header at row
/// 0 — permanently off the top, so they could never be scrolled back into view.
/// To avoid that, after keeping the selection visible we pull the scroll up to
/// the start of the header run that introduces the selected session, capped so
/// the session itself stays on screen.
pub fn session_picker_scroll(
    rows: &[SessionPickerRow],
    sel_raw: Option<usize>,
    prev_scroll: usize,
    visible: usize,
) -> usize {
    let visible = visible.max(1);
    let total = rows.len();
    let mut scroll = prev_scroll;
    if let Some(sr) = sel_raw {
        if sr < scroll {
            scroll = sr;
        } else if sr >= scroll + visible {
            scroll = sr + 1 - visible;
        }
        // First non-selectable row of the contiguous header run directly above
        // the selection (0 when the selection is the list's first selectable
        // row). Reveal it without ever pushing the selection off the bottom.
        let header_top = rows[..sr]
            .iter()
            .rposition(SessionPickerRow::is_selectable)
            .map_or(0, |i| i + 1);
        scroll = scroll.min(header_top.max((sr + 1).saturating_sub(visible)));
    }
    scroll.min(total.saturating_sub(visible))
}

impl App {
    pub fn session_picker_active(&self) -> bool {
        self.session_picker.is_some()
    }

    /// Open the dialog. Returns silently (with a status note) when there are no
    /// user sessions to pick from. Any pending key chord is cleared so the
    /// dialog owns subsequent input cleanly.
    pub fn open_session_picker(&mut self, purpose: SessionPickerPurpose) {
        if self.user_sessions().is_empty() {
            self.set_status("no sessions".to_string());
            return;
        }
        self.chord_state = ChordState::default();
        self.chord_label.clear();
        self.session_picker = Some(SessionPickerDialog {
            purpose,
            query: String::new(),
            cursor: 0,
            selected: 0,
            scroll: 0,
        });
    }

    /// Open the dialog from the program view's `@`→session category. The
    /// program's smart-clip search is intentionally left active so confirming
    /// can replace the `@…` token with the chosen clip.
    pub(super) fn open_session_picker_for_program_clip(&mut self) {
        self.open_session_picker(SessionPickerPurpose::InsertProgramClip);
    }

    /// The query that drives dimming/auto-expand. For the `C-x b` switcher this
    /// is the dialog's own typeahead line. For the program `@`→session variant
    /// there is no search line in the dialog — the live `@<typeahead>` token in
    /// the program buffer is the query, so it stays in lock-step with what the
    /// user sees behind the anchored dialog.
    pub(crate) fn session_picker_effective_query(&self) -> String {
        let Some(dialog) = self.session_picker.as_ref() else {
            return String::new();
        };
        match dialog.purpose {
            SessionPickerPurpose::Switch => dialog.query.clone(),
            SessionPickerPurpose::InsertProgramClip => self
                .program_popup
                .as_ref()
                .and_then(|popup| {
                    let trigger_start = popup.smart_clip.as_ref()?.trigger_start;
                    program_smart_clip_query(popup, trigger_start)
                })
                .unwrap_or_default(),
        }
    }

    /// Materialize the dialog's rows for its effective query (see
    /// [`Self::session_picker_effective_query`]).
    pub(crate) fn session_picker_rows(&self) -> Vec<SessionPickerRow> {
        self.session_picker_rows_for_query(&self.session_picker_effective_query())
    }

    /// Materialize the dialog's rows for an explicit `query`. Mirrors the
    /// session-list ordering (ungrouped, then groups by position, members by
    /// position) but expands/collapses each group and archive section by
    /// whether it contains a query match. Rendering passes an empty query here
    /// to size the switcher to its full (unfiltered) list so the frame height
    /// stays stable as the live query narrows the results.
    pub(crate) fn session_picker_rows_for_query(&self, query: &str) -> Vec<SessionPickerRow> {
        if self.session_picker.is_none() {
            return Vec::new();
        }
        let has_query = !query.trim().is_empty();
        let matched = |s: &SessionSummary| switch_session_match_score(s, query).is_some();

        let mut out: Vec<SessionPickerRow> = Vec::new();
        let orch_id = self.orchestrator_id.as_deref();

        // Ungrouped sessions, ordered exactly like the list view.
        let mut ungrouped: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| s.group_id.is_none())
            .filter(|s| Some(s.id.as_str()) != orch_id)
            .filter(|s| is_user_list_session(s))
            .collect();
        ungrouped.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        let (ungrouped_active, ungrouped_archived): (Vec<&SessionSummary>, Vec<&SessionSummary>) =
            ungrouped.into_iter().partition(|s| !s.archived);
        for s in &ungrouped_active {
            out.push(SessionPickerRow::Session {
                summary: (*s).clone(),
                indented: false,
                dimmed: has_query && !matched(s),
            });
        }
        push_archive_section(&mut out, &ungrouped_archived, has_query, false, &matched);

        // Project groups, in position order.
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
            let active_matches = active.iter().filter(|s| matched(s)).count();
            let archived_matches = archived.iter().any(|s| matched(s));
            // Without a query every group is open (browse everything). With a
            // query a group opens iff one of its sessions matches; otherwise it
            // collapses to just its header.
            let expanded = !has_query || active_matches > 0 || archived_matches;
            out.push(SessionPickerRow::GroupHeader {
                name: g.name.clone(),
                expanded,
                matches: active_matches,
            });
            if expanded {
                for s in &active {
                    out.push(SessionPickerRow::Session {
                        summary: (*s).clone(),
                        indented: true,
                        dimmed: has_query && !matched(s),
                    });
                }
                push_archive_section(&mut out, &archived, has_query, true, &matched);
            }
        }

        // The currently open program's blocks, so `C-x b` can also jump
        // straight to a block. Only for the plain switcher — the `@`→session
        // clip variant expects only `Session` rows among the selectable set.
        if matches!(
            self.session_picker.as_ref().map(|d| &d.purpose),
            Some(SessionPickerPurpose::Switch)
        ) {
            if let Some(popup) = self.program_popup.as_ref().filter(|p| !p.closing) {
                let query_lower = query.to_ascii_lowercase();
                let block_rows: Vec<SessionPickerRow> = program_blocks(&popup.buffer)
                    .into_iter()
                    .filter_map(|b| {
                        let text = popup.buffer.lines().nth(b.start_line)?.trim();
                        (!text.is_empty()).then_some((text, b.start_line))
                    })
                    .filter(|(text, _)| {
                        !has_query || text.to_ascii_lowercase().contains(&query_lower)
                    })
                    .take(10)
                    .map(|(text, start_line)| SessionPickerRow::ProgramBlock {
                        text: truncate_block_text(text),
                        start_line,
                    })
                    .collect();
                if !block_rows.is_empty() {
                    out.push(SessionPickerRow::ProgramHeader);
                    out.extend(block_rows);
                }
            }
        }
        out
    }

    fn session_picker_selectable_count(&self) -> usize {
        self.session_picker_rows()
            .iter()
            .filter(|r| r.is_selectable())
            .count()
    }

    fn move_session_picker_selection(&mut self, delta: isize) {
        let count = self.session_picker_selectable_count();
        let Some(dialog) = self.session_picker.as_mut() else {
            return;
        };
        if count == 0 {
            dialog.selected = 0;
            return;
        }
        let selected = dialog.selected.min(count - 1);
        dialog.selected = if delta < 0 {
            selected
                .saturating_add(count)
                .saturating_sub(delta.unsigned_abs() % count)
                % count
        } else {
            (selected + delta as usize) % count
        };
    }

    fn session_picker_push_char(&mut self, c: char) {
        if let Some(dialog) = self.session_picker.as_mut() {
            let pos = byte_pos(&dialog.query, dialog.cursor);
            dialog.query.insert(pos, c);
            dialog.cursor += 1;
            // The match set just changed; snap back to the top match.
            dialog.selected = 0;
            dialog.scroll = 0;
        }
    }

    fn session_picker_backspace(&mut self) {
        if let Some(dialog) = self.session_picker.as_mut() {
            if dialog.cursor > 0 {
                let prev = dialog.cursor - 1;
                let pos = byte_pos(&dialog.query, prev);
                dialog.query.remove(pos);
                dialog.cursor = prev;
            }
            dialog.selected = 0;
            dialog.scroll = 0;
        }
    }

    /// `C-f` / `C-b`: move the search-line cursor by one char, clamped to the
    /// query's bounds. Only meaningful for the `C-x b` switcher's own search
    /// line (see [`SessionPickerDialog::cursor`]).
    fn session_picker_move_cursor(&mut self, delta: isize) {
        if let Some(dialog) = self.session_picker.as_mut() {
            let len = dialog.query.chars().count();
            dialog.cursor = if delta < 0 {
                dialog.cursor.saturating_sub(delta.unsigned_abs())
            } else {
                dialog.cursor.saturating_add(delta as usize).min(len)
            };
        }
    }

    /// `C-a` / `C-e`: jump the search-line cursor to the start or end of the
    /// query.
    fn session_picker_cursor_to_edge(&mut self, end: bool) {
        if let Some(dialog) = self.session_picker.as_mut() {
            dialog.cursor = if end { dialog.query.chars().count() } else { 0 };
        }
    }

    /// `C-k`: kill from the cursor to the end of the query. The cursor itself
    /// doesn't move — it's already at the (now shorter) end of the query.
    fn session_picker_kill_to_end(&mut self) {
        if let Some(dialog) = self.session_picker.as_mut() {
            let pos = byte_pos(&dialog.query, dialog.cursor);
            dialog.query.truncate(pos);
            dialog.cursor = dialog.cursor.min(dialog.query.chars().count());
        }
        self.session_picker_reset_selection();
    }

    /// Snap the highlight back to the top match. Called after the effective
    /// query changes (typing/backspace) so navigation resumes from the best hit.
    fn session_picker_reset_selection(&mut self) {
        if let Some(dialog) = self.session_picker.as_mut() {
            dialog.selected = 0;
            dialog.scroll = 0;
        }
    }

    /// `@`→session variant: there is no dialog search line, so a typed character
    /// extends the live `@<typeahead>` token in the program buffer. That token
    /// *is* the query, so the dialog re-filters as the visible text grows. A
    /// character that ends the token (e.g. whitespace) tears the `@` search down,
    /// which dismisses the picker just as the inline `@` menu would.
    fn session_picker_program_push_char(&mut self, c: char) {
        self.insert_program_text(&c.to_string());
        if self.program_smart_clip_active() {
            self.session_picker_reset_selection();
        } else {
            self.cancel_session_picker();
        }
    }

    /// `@`→session variant of backspace: delete the last character of the live
    /// `@<typeahead>` token. Backspacing over the `@` itself removes it and
    /// closes the whole picker (mirroring the inline `@` menu).
    fn session_picker_program_backspace(&mut self) {
        if self.program_smart_clip_backspace() {
            self.session_picker_reset_selection();
        } else {
            // The `@` trigger is gone (or there is no live search); nothing left
            // to pick against, so dismiss the dialog too.
            self.cancel_session_picker();
        }
    }

    /// Close the dialog without acting. When it was driving a program-clip
    /// insertion, also dismiss the underlying `@` smart-clip menu so it doesn't
    /// reappear behind the closed dialog.
    fn cancel_session_picker(&mut self) {
        let was_clip = self
            .session_picker
            .take()
            .map(|d| d.purpose == SessionPickerPurpose::InsertProgramClip)
            .unwrap_or(false);
        if was_clip {
            self.cancel_program_smart_clip();
        }
    }

    /// Left-arrow "go back": close the `@`→session dialog and return to the
    /// inline `@` context menu it was opened from, re-highlighting the "session"
    /// category so Left/Right are reversible (mirrors
    /// [`Self::program_smart_clip_collapse`] for the inline submenus). Unlike
    /// [`Self::cancel_session_picker`], the underlying smart-clip search is left
    /// alive so the inline menu re-renders in the same anchored position. A no-op
    /// for the `C-x b` switcher, which has no parent menu to return to.
    fn session_picker_back_to_menu(&mut self) {
        if !matches!(
            self.session_picker.as_ref().map(|d| &d.purpose),
            Some(SessionPickerPurpose::InsertProgramClip)
        ) {
            return;
        }
        // Find the "session" category among the menu's selectable rows so we can
        // re-highlight it. The dialog only ever opens from the root view (the
        // category lives there), so the current rows are the root rows. `None`
        // means there is no live `@` menu underneath (it should be there, but
        // guard rather than assume).
        let selected = self.program_popup.as_ref().and_then(|popup| {
            popup.smart_clip.as_ref()?;
            Some(
                self.program_smart_clip_rows(popup)
                    .iter()
                    .filter(|r| r.is_selectable())
                    .position(|r| {
                        matches!(
                            r,
                            ProgramSmartClipRow::Category {
                                group: ProgramSmartClipGroup::Session,
                                ..
                            }
                        )
                    })
                    .unwrap_or(0),
            )
        });
        // Close the dialog but keep the smart-clip search live so the inline `@`
        // menu re-appears where the dialog (and the menu before it) sat.
        self.session_picker = None;
        if let Some(selected) = selected {
            if let Some(search) = self
                .program_popup
                .as_mut()
                .and_then(|popup| popup.smart_clip.as_mut())
            {
                search.view = ProgramSmartClipView::Root;
                search.selected = selected;
            }
        }
    }

    /// Act on the highlighted row: switch focus to a session, insert its
    /// clip, or jump the program view to a picked block.
    fn confirm_session_picker(&mut self) {
        let Some(purpose) = self.session_picker.as_ref().map(|d| d.purpose.clone()) else {
            return;
        };
        let selected = self
            .session_picker
            .as_ref()
            .map(|d| d.selected)
            .unwrap_or(0);
        let chosen: Vec<SessionPickerRow> = self
            .session_picker_rows()
            .into_iter()
            .filter(SessionPickerRow::is_selectable)
            .collect();
        if chosen.is_empty() {
            self.cancel_session_picker();
            self.set_status("no session matches".to_string());
            return;
        }
        let row = chosen[selected.min(chosen.len() - 1)].clone();
        self.session_picker = None;
        match row {
            SessionPickerRow::Session { summary, .. } => match purpose {
                SessionPickerPurpose::Switch => {
                    let label = session_switch_label(&summary);
                    self.select_session(summary.id.clone());
                    self.sync_active_window_selection();
                    self.focus = PaneFocus::View;
                    self.set_status(format!("window → {label}"));
                }
                SessionPickerPurpose::InsertProgramClip => {
                    let candidate = Self::session_smart_clip_candidate(&summary);
                    self.insert_program_smart_clip_candidate(&candidate);
                }
            },
            SessionPickerRow::ProgramBlock { text, start_line } => {
                self.jump_to_program_block(start_line);
                self.set_status(format!("program → {text}"));
            }
            SessionPickerRow::GroupHeader { .. }
            | SessionPickerRow::ArchiveHeader { .. }
            | SessionPickerRow::ProgramHeader => {}
        }
    }

    /// Scroll the currently open program view to `start_line` (a source-line
    /// index into its markdown), e.g. after picking a block from the `C-x b`
    /// picker. The program is already open — its blocks are only listed in
    /// the picker while it is — so this just brings it into keyboard focus
    /// (undoing a terminal-focus slide) and moves the scroll to the block.
    fn jump_to_program_block(&mut self, start_line: usize) {
        let Some(inner) = self.layout.program_inner_area else {
            return;
        };
        let width = inner.width as usize;
        if width == 0 {
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let offset: usize = popup
            .buffer
            .lines()
            .take(start_line)
            .map(|line| line.chars().count() + 1)
            .sum();
        let visual_row =
            crate::ui::program_cursor_visual_row(Some(self), &popup.buffer, offset, width);
        self.focus = PaneFocus::View;
        self.set_program_terminal_focus(false);
        if let Some(popup) = self.program_popup.as_mut() {
            popup.scroll_offset = visual_row;
        }
    }

    /// Route a key while the dialog owns input. Captures everything: typing
    /// edits the query, arrows / `C-n` / `C-p` move the selection, Enter
    /// confirms, Esc / `C-g` cancels. Typing/backspace route to whichever query
    /// backs the dialog — the switcher's own line, or the program's live
    /// `@<typeahead>` token for the `@`→session variant.
    pub(super) fn handle_session_picker_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        let to_program = matches!(
            self.session_picker.as_ref().map(|d| &d.purpose),
            Some(SessionPickerPurpose::InsertProgramClip)
        );
        match key.code {
            KeyCode::Esc => self.cancel_session_picker(),
            KeyCode::Char('g') if ctrl => self.cancel_session_picker(),
            KeyCode::Enter => self.confirm_session_picker(),
            // Left backs out of the `@`→session dialog to the inline `@` menu it
            // was opened from (a no-op for the `C-x b` switcher, which has no
            // parent menu). Mirrors the inline submenu's Left/Right reversibility.
            KeyCode::Left => self.session_picker_back_to_menu(),
            KeyCode::Up => self.move_session_picker_selection(-1),
            KeyCode::Down => self.move_session_picker_selection(1),
            KeyCode::Char('p') if ctrl => self.move_session_picker_selection(-1),
            KeyCode::Char('n') if ctrl => self.move_session_picker_selection(1),
            // Emacs cursor motion on the switcher's own search line. Not
            // wired up for the `@`→session variant, whose "query" is the
            // program buffer's live token with its own cursor semantics.
            KeyCode::Char('f') if ctrl && !to_program => self.session_picker_move_cursor(1),
            KeyCode::Char('b') if ctrl && !to_program => self.session_picker_move_cursor(-1),
            KeyCode::Char('a') if ctrl && !to_program => self.session_picker_cursor_to_edge(false),
            KeyCode::Char('e') if ctrl && !to_program => self.session_picker_cursor_to_edge(true),
            KeyCode::Char('k') if ctrl && !to_program => self.session_picker_kill_to_end(),
            KeyCode::Backspace if to_program => self.session_picker_program_backspace(),
            KeyCode::Backspace => self.session_picker_backspace(),
            KeyCode::Char(c) if !ctrl && !alt && !super_mod && to_program => {
                self.session_picker_program_push_char(c)
            }
            KeyCode::Char(c) if !ctrl && !alt && !super_mod => self.session_picker_push_char(c),
            _ => {}
        }
    }
}

/// Max display characters for a [`SessionPickerRow::ProgramBlock`] label, so a
/// long block's first line can't blow out the dialog's fixed row width.
const PROGRAM_BLOCK_TEXT_MAX_CHARS: usize = 60;

/// Clip a program block's first line to [`PROGRAM_BLOCK_TEXT_MAX_CHARS`],
/// appending `…` when it didn't fit.
fn truncate_block_text(text: &str) -> String {
    if text.chars().count() <= PROGRAM_BLOCK_TEXT_MAX_CHARS {
        return text.to_string();
    }
    let mut out: String = text
        .chars()
        .take(PROGRAM_BLOCK_TEXT_MAX_CHARS.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

/// Append an "N archived" disclosure row (and, when it should be open, its
/// archived session rows) for a section. The section opens only when the query
/// is active and at least one archived session matches, so archived sessions
/// stay hidden during ordinary browsing.
fn push_archive_section(
    out: &mut Vec<SessionPickerRow>,
    archived: &[&SessionSummary],
    has_query: bool,
    indented: bool,
    matched: &impl Fn(&SessionSummary) -> bool,
) {
    if archived.is_empty() {
        return;
    }
    let expanded = has_query && archived.iter().any(|s| matched(s));
    out.push(SessionPickerRow::ArchiveHeader {
        count: archived.len(),
        expanded,
        indented,
    });
    if expanded {
        for s in archived {
            out.push(SessionPickerRow::Session {
                summary: (*s).clone(),
                indented,
                dimmed: !matched(s),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary() -> SessionSummary {
        SessionSummary {
            id: "s".into(),
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
            kind: agentd_protocol::SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
        }
    }

    fn header() -> SessionPickerRow {
        SessionPickerRow::GroupHeader {
            name: "proj".into(),
            expanded: true,
            matches: 0,
        }
    }

    fn session_row() -> SessionPickerRow {
        SessionPickerRow::Session {
            summary: summary(),
            indented: true,
            dimmed: false,
        }
    }

    /// `header`, then `n` selectable session rows — the layout that triggered
    /// the reported bug (a project header is the very first row).
    fn header_then_sessions(n: usize) -> Vec<SessionPickerRow> {
        let mut rows = vec![header()];
        rows.extend(std::iter::repeat_with(session_row).take(n));
        rows
    }

    #[test]
    fn scrolling_up_to_first_session_exposes_leading_header() {
        // Header at raw 0, sessions at raw 1..=20; a 5-row window scrolled to
        // the bottom. Selecting the first session (raw 1) must pull the window
        // all the way to row 0 so the project header is visible again — the old
        // logic clamped to the session's own index (1) and hid it forever.
        let rows = header_then_sessions(20);
        assert_eq!(session_picker_scroll(&rows, Some(1), 16, 5), 0);
    }

    #[test]
    fn first_open_keeps_header_visible() {
        // Fresh open: selection on the first session, scroll already at the top.
        let rows = header_then_sessions(20);
        assert_eq!(session_picker_scroll(&rows, Some(1), 0, 5), 0);
    }

    #[test]
    fn scrolling_down_anchors_selection_to_the_bottom_of_the_window() {
        // Last session (raw 20) with a 5-row window scrolled from the top: the
        // window follows down so the selection sits on the last visible line.
        let rows = header_then_sessions(20);
        assert_eq!(session_picker_scroll(&rows, Some(20), 0, 5), 16);
    }

    #[test]
    fn selection_already_in_view_leaves_scroll_untouched() {
        let rows = header_then_sessions(20);
        assert_eq!(session_picker_scroll(&rows, Some(10), 8, 5), 8);
    }

    #[test]
    fn scrolling_up_onto_a_groups_first_member_reveals_its_header() {
        // 10 ungrouped sessions (raw 0..=9), a header (raw 10), then 5 grouped
        // sessions (raw 11..=15). Scrolling up onto the group's first member
        // (raw 11) reveals the header just above it.
        let mut rows: Vec<SessionPickerRow> =
            std::iter::repeat_with(session_row).take(10).collect();
        rows.push(header());
        rows.extend(std::iter::repeat_with(session_row).take(5));
        assert_eq!(session_picker_scroll(&rows, Some(11), 12, 4), 10);
    }

    #[test]
    fn no_selection_clamps_persisted_scroll_to_the_list() {
        let rows = header_then_sessions(20);
        // total = 21, visible = 5 → max scroll is 16.
        assert_eq!(session_picker_scroll(&rows, None, 99, 5), 16);
    }

    #[test]
    fn empty_list_scrolls_to_zero() {
        assert_eq!(session_picker_scroll(&[], None, 7, 5), 0);
    }
}
