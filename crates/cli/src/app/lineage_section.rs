//! The sidebar's lineage section (spec 0081): a collapsible region of the
//! session-list pane, between the session rows and the operator/matrix-rain
//! panel, that renders the SELECTED session's fork/subagent lineage tree.
//!
//! This replaces the floating hover/pin preview that used to anchor to the
//! pane title bar's harness label (spec 0080, superseded). The tree
//! construction (`crate::lineage`) and the row-level interaction vocabulary
//! (`j`/`k`/arrows/`C-n`/`C-p` navigation, `Enter` jumps in, `m`/`d`
//! merge/discard, `Esc` backs out) carry over unchanged; what changed is
//! where the surface lives and how it's entered:
//!
//! - The section renders whenever the selected session has lineage to show —
//!   no hover trigger, no pin. It follows the list selection like a detail
//!   panel (master–detail), and a click on its header collapses it to just
//!   that header row.
//! - `Tab`, while the list pane holds focus, moves keyboard focus between
//!   the session rows and the lineage section; `C-x Tab` toggles the
//!   section's focus from anywhere (the same chord that used to focus the
//!   floating preview).

use super::*;
use crate::lineage::LineageRow;

impl App {
    /// The session whose lineage the sidebar section shows: the selected
    /// session, when it actually has fork/subagent lineage to draw
    /// (`crate::lineage::has_lineage`) and the tree amounts to more than the
    /// session's own lone node (e.g. its one fork was just discarded and
    /// pruned). `None` hides the section entirely.
    pub fn lineage_section_session(&self) -> Option<String> {
        let id = self.selected_id()?;
        if !crate::lineage::has_lineage(&id, &self.sessions) {
            return None;
        }
        let rows = self.lineage_section_rows(&id);
        // More than the session's own node — or a collapsed subagent group
        // (which must stay reachable so it can be expanded at all).
        let has_content = rows.iter().filter(|r| r.is_selectable()).count() > 1
            || rows
                .iter()
                .flat_map(|r| r.spans.iter())
                .any(|sp| matches!(sp.role, crate::lineage::LineageSpan::SubagentsToggle { .. }));
        has_content.then_some(id)
    }

    /// Whether `(col, row)` lands inside the last-rendered lineage section
    /// (header row included). Used for wheel routing and to keep a click
    /// there from starting a text selection underneath.
    pub(super) fn is_over_lineage_section(&self, col: u16, row: u16) -> bool {
        self.layout
            .lineage_area
            .is_some_and(|area| Self::rect_contains(area, col, row))
    }

    /// Whether `(col, row)` lands on the horizontal scrollbar's row (the
    /// section's reserved bottom row) — a wheel there scrolls sideways.
    pub(super) fn is_over_lineage_hscrollbar(&self, col: u16, row: u16) -> bool {
        self.layout
            .lineage_hscroll_hit
            .is_some_and(|r| Self::rect_contains(r, col, row))
    }

    /// Whether `(col, row)` lands on the section header's bare bar — the
    /// height drag handle. The header's own buttons (collapse, mode toggle)
    /// are excluded so their clicks stay clicks, mirroring
    /// `is_on_matrix_rain_title_bar`.
    pub(super) fn is_on_lineage_header_bar(&self, col: u16, row: u16) -> bool {
        let Some(header) = self.layout.lineage_header_hit else {
            return false;
        };
        if !Self::rect_contains(header, col, row) {
            return false;
        }
        if self
            .layout
            .lineage_collapse_hit
            .is_some_and(|r| Self::rect_contains(r, col, row))
        {
            return false;
        }
        if self
            .layout
            .lineage_toggle_hit
            .is_some_and(|r| Self::rect_contains(r, col, row))
        {
            return false;
        }
        true
    }

    /// Whether the session ROWS (not the lineage section) should read as
    /// the keyboard-focused sidebar region. Exactly one of the two sidebar
    /// regions highlights at a time: focusing the lineage section takes the
    /// highlight off the sessions title bar, and vice versa.
    pub(crate) fn session_rows_focused(&self) -> bool {
        self.focus == PaneFocus::List && !self.lineage_focused
    }

    /// `C-x Tab`: toggle the lineage section's keyboard focus from anywhere —
    /// the same chord that used to focus the floating preview. A no-op when
    /// the selected session has no lineage section to focus.
    pub fn toggle_lineage_focus(&mut self) {
        // The section's own key handler owns subsequent keystrokes directly
        // (not the chord state machine), so reset it exactly like other
        // dialog-opening actions do (`open_configure_popup`, ...).
        self.chord_state = ChordState::default();
        self.chord_label.clear();
        if self.lineage_focused && self.focus == PaneFocus::List {
            self.lineage_focused = false;
            return;
        }
        self.activate_lineage_focus();
    }

    /// Give the lineage section keyboard focus: expand it if collapsed, seed
    /// the row selection on the selected session's own node (not the tree's
    /// root), and move pane focus to the list — the section lives in the
    /// sidebar, so focusing it should read as sidebar focus. Shared by the
    /// `C-x Tab` toggle, the bare-`Tab` sessions⇄lineage switch, and a click
    /// inside the section's body. Returns `false` (and changes nothing) when
    /// there is no section to focus.
    pub(super) fn activate_lineage_focus(&mut self) -> bool {
        let Some(id) = self.lineage_section_session() else {
            return false;
        };
        self.lineage_collapsed = false;
        let rows = self.lineage_section_rows(&id);
        let selectable = crate::lineage::selectable_indices(&rows);
        self.lineage_selected = selectable
            .iter()
            .position(|&idx| rows[idx].session_id() == Some(id.as_str()))
            .unwrap_or(0);
        self.lineage_scroll = 0;
        self.focus = PaneFocus::List;
        self.lineage_focused = true;
        self.lineage_follow_selection = true;
        true
    }

    /// Materialize the current flattened rows for `session_id`'s lineage
    /// tree — used both by keyboard navigation here and by the section
    /// renderer to draw them. Rebuilt from live `App::sessions` on every
    /// call, so there is no section-owned copy to go stale.
    pub(crate) fn lineage_section_rows(&self, session_id: &str) -> Vec<LineageRow> {
        self.lineage_section_diagram(session_id).0
    }

    /// Rows plus every box's canvas bounds — the renderer maps the bounds
    /// to screen rects for hover/click hit-testing. Draws whichever
    /// visualization `lineage_mode` selects; both modes share the same
    /// row/selection/hit model.
    pub(crate) fn lineage_section_diagram(
        &self,
        session_id: &str,
    ) -> (Vec<LineageRow>, Vec<crate::lineage::LineageBoxBounds>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        crate::lineage::build_tree_with_expansions(
            session_id,
            &self.sessions,
            Some(&self.lineage_subagents_expanded),
        )
        .map(|root| match self.lineage_mode {
            crate::lineage::LineageViewMode::Boxes => {
                crate::lineage::flatten_with_boxes(&root, &self.sessions, now_ms)
            }
            crate::lineage::LineageViewMode::Rails => {
                crate::lineage::flatten_rails(&root, &self.sessions, now_ms)
            }
        })
        .unwrap_or_default()
    }

    /// The session id of the currently-highlighted row in the focused
    /// section, if any (never a `More` marker row — those aren't selectable).
    fn lineage_selected_session_id(&self, session_id: &str) -> Option<String> {
        let rows = self.lineage_section_rows(session_id);
        let selectable = crate::lineage::selectable_indices(&rows);
        if selectable.is_empty() {
            return None;
        }
        let idx = selectable[self.lineage_selected.min(selectable.len() - 1)];
        rows[idx].session_id().map(|s| s.to_string())
    }

    /// `j`/`k`/arrows/`C-n`/`C-p`: move the focused section's selection.
    fn move_lineage_selection(&mut self, delta: isize) {
        let Some(session_id) = self.lineage_section_session() else {
            return;
        };
        let rows = self.lineage_section_rows(&session_id);
        let selectable = crate::lineage::selectable_indices(&rows);
        if selectable.is_empty() {
            self.lineage_selected = 0;
            return;
        }
        let count = selectable.len();
        let current = self.lineage_selected.min(count - 1);
        // Keyboard navigation re-anchors the viewport to the selection
        // (a wheel scroll may have roamed away from it).
        self.lineage_follow_selection = true;
        self.lineage_selected = if delta < 0 {
            current
                .saturating_add(count)
                .saturating_sub(delta.unsigned_abs() % count)
                % count
        } else {
            (current + delta as usize) % count
        };
    }

    /// Enter: jump into the highlighted session and hand keyboard focus back
    /// (leaving the section to go work in that session means it stops owning
    /// the keyboard; the section itself stays visible, tracking the new
    /// selection).
    fn confirm_lineage_selection(&mut self) {
        let Some(session_id) = self.lineage_section_session() else {
            self.lineage_focused = false;
            return;
        };
        let Some(target_id) = self.lineage_selected_session_id(&session_id) else {
            return;
        };
        // `lineage_focused` deliberately stays set: pane focus moves to the
        // view (making it dormant), and returning to the sidebar (`C-x l`,
        // `C-x o`, `C-1`) lands back in the section you left.
        self.jump_to_lineage_session(&target_id);
    }

    /// Switch to a session picked from the lineage diagram — shared by Enter
    /// on the keyboard selection and a mouse click on a box. A *merged*
    /// fork instead jumps to its parent — the merge point in the graph and
    /// the injected result message are the same transcript event (spec
    /// 0078), so this links to where that event actually lives instead of
    /// re-showing the now-archived fork.
    pub(super) fn jump_to_lineage_session(&mut self, target_id: &str) {
        let Some(summary) = self.sessions.iter().find(|s| s.id == target_id).cloned() else {
            self.set_status(format!("session {} no longer exists", short_id(target_id)));
            return;
        };
        let target = match (&summary.forked_from, &summary.merge) {
            (Some(f), Some(m)) if m.mode == construct_protocol::ForkMergeMode::Result => {
                f.session_id.clone()
            }
            _ => target_id.to_string(),
        };
        self.select_session(target);
        self.sync_active_window_selection();
        self.focus = PaneFocus::View;
    }

    /// `m` / `d`: merge or discard the focused section's highlighted fork,
    /// reusing the exact merge/discard path the `C-x m` minibuffer menu uses
    /// ([`App::apply_fork_merge`], spec 0078) — a direct-key shortcut for
    /// it, not a second implementation. A no-op with a status note when the
    /// highlighted row isn't an open (unmerged, undiscarded) fork.
    async fn lineage_merge_or_discard(&mut self, mode: construct_protocol::ForkMergeMode) {
        let Some(session_id) = self.lineage_section_session() else {
            return;
        };
        let Some(id) = self.lineage_selected_session_id(&session_id) else {
            return;
        };
        let is_open_fork = self
            .sessions
            .iter()
            .any(|s| s.id == id && s.forked_from.is_some() && s.merge.is_none());
        if !is_open_fork {
            self.set_status("merge: select an open fork".to_string());
            return;
        }
        self.apply_fork_merge(id, mode).await;
        // The section stays focused — its rows are rebuilt from live
        // `self.sessions` on the very next render/key, so the merged fork's
        // terminal-state styling appears immediately.
    }

    /// Route a key while the lineage section owns keyboard focus.
    /// Navigation/merge/discard/jump keys return `true` (fully handled; the
    /// section stays focused unless it was Enter, Esc, or Tab). Anything
    /// else clears focus and returns `false`, telling the caller
    /// (`App::on_key`) to re-dispatch the SAME key through ordinary routing —
    /// the "a closing overlay never eats a live keystroke" rule, so e.g.
    /// `C-x C-c` still quits while the section is focused.
    ///
    /// Esc and bare Tab both hand focus back to the session rows: Esc as the
    /// universal "back out one level", Tab as the sessions⇄lineage focus
    /// switch (its `App::on_key` intercept handles the sessions→lineage
    /// direction).
    pub(super) async fn handle_lineage_focus_key(&mut self, key: KeyEvent) -> bool {
        if self.lineage_section_session().is_none() {
            // The section vanished under the focus (selection moved to a
            // session without lineage, the last fork was pruned, ...).
            self.lineage_focused = false;
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.lineage_focused = false;
                true
            }
            // `C-p`/`C-n` mirror the session-list's own emacs-style
            // NextSession/PrevSession bindings so navigation muscle memory
            // carries into this section; `key.code` alone (crossterm reports
            // Ctrl+letter as `Char` with a CONTROL modifier, not a distinct
            // code) means these arms also accept bare `p`/`n`, same as `k`/`j`
            // below accept bare Up/Down without checking modifiers.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                self.move_lineage_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                self.move_lineage_selection(1);
                true
            }
            KeyCode::Enter => {
                self.confirm_lineage_selection();
                true
            }
            KeyCode::Char('m') => {
                self.lineage_merge_or_discard(construct_protocol::ForkMergeMode::Result)
                    .await;
                true
            }
            KeyCode::Char('d') => {
                self.lineage_merge_or_discard(construct_protocol::ForkMergeMode::Discard)
                    .await;
                true
            }
            // `C-x Tab` (`ToggleLineageFocus`) is the from-anywhere focus
            // toggle — but it's a two-key chord, and its own prefix key
            // (`C-x`) isn't otherwise in this handler's vocabulary. Without
            // this arm, `C-x` would hit the fallback below, clear focus, and
            // fall through BEFORE `Tab` ever arrives — so by the time the
            // chord completes and `App::toggle_lineage_focus` runs, focus
            // already reads as "not focused" and it re-opens instead of
            // closing. Let the prefix fall through without touching focus so
            // the toggle handler sees the true prior state.
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => false,
            // Bare Tab hands focus back to the session rows (the
            // sessions⇄lineage switch). Mid-chord (`C-x` pending) it must
            // fall through instead, so the `C-x Tab` chord completes and the
            // toggle handler runs with focus still intact.
            KeyCode::Tab => {
                if self.chord_state.is_empty() {
                    self.lineage_focused = false;
                    true
                } else {
                    false
                }
            }
            _ => {
                // Mid-chord keys (`C-x o`, `C-x b`, ...) fall through intact
                // so the chord completes — the section's sub-focus survives
                // as the sidebar's memory. A bare unhandled key hands
                // sub-focus back to the session rows.
                if self.chord_state.is_empty() {
                    self.lineage_focused = false;
                }
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            harness: "smith".into(),
            cwd: "/tmp".into(),
            title: None,
            state: construct_protocol::SessionState::Running,
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
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind: construct_protocol::SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    async fn test_app_with_sessions(
        sessions: Vec<SessionSummary>,
    ) -> (App, tempfile::TempDir, tokio::task::JoinHandle<()>) {
        use tokio::net::UnixListener;
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            loop {
                let Ok(_conn) = listener.accept().await else {
                    break;
                };
            }
        });
        let client = construct_client::Client::connect(&sock)
            .await
            .expect("client connects");
        let app = crate::app::tests::test_app(client, sessions);
        (app, dir, server)
    }

    fn fork_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.forked_from = Some(construct_protocol::ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq: 0,
            at_ms: 0,
            parent_busy_ms: 0,
            parent_message_count: 0,
        });
        s
    }

    #[tokio::test]
    async fn toggle_focus_requires_a_selection() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![]).await;
        app.toggle_lineage_focus();
        assert!(!app.lineage_focused);
    }

    #[tokio::test]
    async fn toggle_focus_is_a_no_op_without_lineage() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        assert!(
            !app.lineage_focused,
            "an ordinary session with no lineage has no section to focus"
        );
    }

    #[tokio::test]
    async fn toggle_focus_focuses_then_closes_on_second_press() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());

        app.toggle_lineage_focus();
        assert!(app.lineage_focused);
        assert_eq!(
            app.focus,
            PaneFocus::List,
            "the section lives in the sidebar"
        );

        app.toggle_lineage_focus();
        assert!(!app.lineage_focused, "a second press must hand focus back");
    }

    #[tokio::test]
    async fn c_x_tab_keystrokes_toggle_open_then_closed() {
        // Regression test: `C-x` isn't in the section's own key vocabulary,
        // so on a second press it used to get treated as an "unhandled key"
        // that cleared focus and fell through BEFORE `Tab` arrived — so by
        // the time the chord completed, focus already read as "not focused"
        // and the toggle re-opened instead of closing.
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(
            app.lineage_focused,
            "first C-x Tab should focus the lineage section"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(
            !app.lineage_focused,
            "second C-x Tab (as real keystrokes through on_key) must close it, \
             not re-open it"
        );
    }

    #[tokio::test]
    async fn bare_tab_switches_sessions_and_lineage_focus_in_the_list_pane() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.focus = PaneFocus::List;

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(
            app.lineage_focused,
            "Tab with the list pane focused moves focus into the lineage section"
        );

        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(
            !app.lineage_focused,
            "Tab again hands focus back to the session rows"
        );
        assert_eq!(app.focus, PaneFocus::List);
    }

    #[tokio::test]
    async fn bare_tab_is_inert_without_a_lineage_section() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.focus = PaneFocus::List;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(!app.lineage_focused);
    }

    #[tokio::test]
    async fn ctrl_n_and_ctrl_p_navigate_like_j_and_k() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        assert_eq!(app.lineage_selected, 0);
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.lineage_selected, 1);
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.lineage_selected, 0);
    }

    #[tokio::test]
    async fn focusing_from_a_fork_starts_selection_on_that_fork_not_the_root() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("fork".to_string());
        app.toggle_lineage_focus();
        assert!(app.lineage_focused);
        let rows = app.lineage_section_rows("fork");
        let selectable = crate::lineage::selectable_indices(&rows);
        let selected_id = rows[selectable[app.lineage_selected]]
            .session_id()
            .map(str::to_string);
        assert_eq!(
            selected_id.as_deref(),
            Some("fork"),
            "focusing the section from a fork must land the selection on \
             the fork's own node, not the tree's root"
        );
    }

    #[tokio::test]
    async fn esc_hands_focus_back_to_the_session_rows() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();

        assert!(
            app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await
        );
        assert!(!app.lineage_focused);
    }

    #[tokio::test]
    async fn unhandled_key_clears_focus_and_reports_unhandled() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.lineage_focused = true;
        let handled = app
            .handle_lineage_focus_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .await;
        assert!(
            !handled,
            "an unbound key must fall through to ordinary routing"
        );
        assert!(!app.lineage_focused);
    }

    #[tokio::test]
    async fn focus_clears_when_the_section_vanishes_under_it() {
        // The focused selection moved to a session without lineage — the
        // very next key must fall through to ordinary routing.
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.lineage_focused = true;
        let handled = app
            .handle_lineage_focus_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        assert!(!handled);
        assert!(!app.lineage_focused);
    }

    #[tokio::test]
    async fn enter_jumps_into_the_selected_session_and_keeps_the_memory() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        // Move down onto the fork row.
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selected_id().as_deref(), Some("fork"));
        assert_eq!(app.focus, PaneFocus::View, "jumping in goes to work there");
        // The sub-focus survives as dormant memory: returning to the
        // sidebar lands back in the lineage section.
        assert!(app.lineage_focused, "memory retained while dormant");
        assert!(
            !app.session_rows_focused(),
            "dormant memory must not read as sidebar focus"
        );
    }

    #[tokio::test]
    async fn returning_to_the_sidebar_restores_its_sub_focus() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        assert!(app.lineage_focused);
        let selected_before = app.lineage_selected;

        // `C-x o` moves pane focus away; the chord's own keys must not
        // erase the sidebar's memory on their way through the handler.
        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE))
            .await;
        assert_ne!(app.focus, PaneFocus::List, "focus cycled away");
        assert!(app.lineage_focused, "memory survives leaving the sidebar");

        // While dormant, the section owns no keys.
        app.on_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.lineage_selected, selected_before,
            "a dormant section must not intercept keys meant for the view"
        );

        // Jumping back to the sidebar restores the lineage sub-focus.
        app.focus_pane_by_index(0);
        assert_eq!(app.focus, PaneFocus::List);
        assert!(
            app.lineage_focused,
            "C-x l / C-1 land back in the section that was focused last"
        );

        // Esc hands sub-focus to the rows; leaving and returning now lands
        // on the rows instead.
        app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await;
        assert!(!app.lineage_focused);
        app.focus = PaneFocus::View;
        app.focus_pane_by_index(0);
        assert!(app.session_rows_focused(), "rows memory restores as rows");
    }

    #[tokio::test]
    async fn enter_on_a_merged_fork_jumps_to_the_parent_instead() {
        let mut fork = fork_of(summary("fork"), "root");
        fork.merge = Some(construct_protocol::ForkMerge {
            mode: construct_protocol::ForkMergeMode::Result,
            at_ms: 0,
            merged_busy_ms: 0,
            merged_message_count: 0,
            merged_seq: 0,
        });
        fork.archived = true;
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selected_id().as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn merge_on_a_non_fork_row_is_a_status_only_no_op() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), {
            let mut sub = summary("sub");
            sub.kind = construct_protocol::SessionKind::Subagent;
            sub.parent_session_id = Some("root".to_string());
            sub
        }])
        .await;
        app.select_session("root".to_string());
        app.toggle_lineage_focus();
        assert!(
            app.handle_lineage_focus_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE))
                .await
        );
        // Still focused — nothing to merge, just a status note.
        assert!(app.lineage_focused);
    }
}
