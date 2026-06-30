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
/// rows with `dimmed == false` are selectable; headers are decoration.
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
}

impl SessionPickerRow {
    pub fn is_selectable(&self) -> bool {
        matches!(
            self,
            SessionPickerRow::Session { dimmed: false, .. }
        )
    }
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

    /// Materialize the dialog's rows for the current query. Mirrors the
    /// session-list ordering (ungrouped, then groups by position, members by
    /// position) but expands/collapses each group and archive section by
    /// whether it contains a query match.
    pub(crate) fn session_picker_rows(&self) -> Vec<SessionPickerRow> {
        let Some(dialog) = self.session_picker.as_ref() else {
            return Vec::new();
        };
        let query = dialog.query.clone();
        let has_query = !query.trim().is_empty();
        let matched = |s: &SessionSummary| switch_session_match_score(s, &query).is_some();

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
            dialog.query.push(c);
            // The match set just changed; snap back to the top match.
            dialog.selected = 0;
            dialog.scroll = 0;
        }
    }

    fn session_picker_backspace(&mut self) {
        if let Some(dialog) = self.session_picker.as_mut() {
            dialog.query.pop();
            dialog.selected = 0;
            dialog.scroll = 0;
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

    /// Act on the highlighted session: switch focus to it, or insert its clip.
    fn confirm_session_picker(&mut self) {
        let Some(purpose) = self.session_picker.as_ref().map(|d| d.purpose.clone()) else {
            return;
        };
        let selected = self
            .session_picker
            .as_ref()
            .map(|d| d.selected)
            .unwrap_or(0);
        let chosen: Vec<SessionSummary> = self
            .session_picker_rows()
            .into_iter()
            .filter_map(|r| match r {
                SessionPickerRow::Session {
                    summary,
                    dimmed: false,
                    ..
                } => Some(summary),
                _ => None,
            })
            .collect();
        if chosen.is_empty() {
            self.cancel_session_picker();
            self.set_status("no session matches".to_string());
            return;
        }
        let summary = chosen[selected.min(chosen.len() - 1)].clone();
        self.session_picker = None;
        match purpose {
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
        }
    }

    /// Route a key while the dialog owns input. Captures everything: typing
    /// edits the query, arrows / `C-n` / `C-p` move the selection, Enter
    /// confirms, Esc / `C-g` cancels.
    pub(super) fn handle_session_picker_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => self.cancel_session_picker(),
            KeyCode::Char('g') if ctrl => self.cancel_session_picker(),
            KeyCode::Enter => self.confirm_session_picker(),
            KeyCode::Up => self.move_session_picker_selection(-1),
            KeyCode::Down => self.move_session_picker_selection(1),
            KeyCode::Char('p') if ctrl => self.move_session_picker_selection(-1),
            KeyCode::Char('n') if ctrl => self.move_session_picker_selection(1),
            KeyCode::Backspace => self.session_picker_backspace(),
            KeyCode::Char(c) if !ctrl && !alt && !super_mod => self.session_picker_push_char(c),
            _ => {}
        }
    }
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
