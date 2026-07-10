//! Anchored, per-session hover/pin/focus preview of a session's fork/subagent
//! lineage tree (spec 0080-lineage-preview-on-harness-label), triggered from
//! the harness label in that session's own pane title bar
//! (`apply_pane_title_right_cluster` in `ui.rs`) — a small, session-attached
//! surface layered on top of the existing label.
//!
//! This module used to sit alongside a second, architecturally distinct
//! surface: a full-screen `C-x q` / `q` modal (`app/lineage_popup.rs`, spec
//! 0079). That modal has been deleted — its tree-construction rules
//! (`crate::lineage`) are unchanged and still reused here, but its
//! interaction vocabulary (`j`/`k`/arrows/`C-n`/`C-p` navigation, `Enter`
//! jumps in, `m`/`d` merge/discard, `Esc` closes) has been ported onto this
//! preview itself: clicking inside a visible preview's body, or pressing
//! `C-x Tab` on the selected session, gives the preview keyboard focus and
//! the exact same key vocabulary the old modal offered. There is now exactly
//! one lineage UI, not two.
//!
//! This mirrors the shape of the (soon-to-be-removed, spec 0003) session
//! widget hover/pin system — `DynamicUiHover` +
//! `App::dynamic_ui_panel_visible` + `App::toggle_dynamic_ui_widget_pin`
//! (`dynamic_ui.rs`) — as independent state, rather than depending on that
//! system directly.

use super::*;
use crate::lineage::LineageRow;

/// A session's lineage preview shown transiently because the cursor is over
/// its harness label. `until` is the expiry; every render frame the pointer
/// still sits on the trigger (or the preview body itself) pushes it out —
/// see `crate::app::LINEAGE_PREVIEW_HOVER_GRACE_MS`. Cleared once it lapses
/// or hover moves to a different session's label. At most one across the
/// fleet, mirroring `DynamicUiHover`/`MatrixWidgetHover`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineagePreviewHover {
    pub session_id: String,
    pub until: Instant,
}

impl App {
    /// Whether `session_id`'s lineage preview should render this frame:
    /// pinned OR an unexpired hover — the same "pinned-OR-unexpired-hover"
    /// shape as `App::dynamic_ui_panel_visible`, kept as independent state.
    pub fn lineage_preview_visible(&self, session_id: &str) -> bool {
        if self.lineage_preview_pinned.contains(session_id) {
            return true;
        }
        self.lineage_preview_hover
            .as_ref()
            .is_some_and(|h| h.session_id == session_id && h.until > Instant::now())
    }

    /// Toggle a session's lineage preview pin from a harness-label click —
    /// the shape to mirror is `App::toggle_dynamic_ui_widget_pin`.
    pub fn toggle_lineage_preview_pin(&mut self, session_id: String) {
        if !self.lineage_preview_pinned.remove(&session_id) {
            self.lineage_preview_pinned.insert(session_id.clone());
        }
        // The click outcome is authoritative; drop any hover preview of this
        // session so the rendered state reflects the pin toggle immediately.
        if self
            .lineage_preview_hover
            .as_ref()
            .is_some_and(|h| h.session_id == session_id)
        {
            self.lineage_preview_hover = None;
        }
        // Un-pinning a session that currently owns keyboard focus also drops
        // that focus — a preview that's no longer pinned (and, absent a
        // fresh hover, about to stop rendering) can't sensibly keep owning
        // keystrokes.
        if !self.lineage_preview_pinned.contains(&session_id)
            && self.lineage_preview_focused.as_deref() == Some(session_id.as_str())
        {
            self.lineage_preview_focused = None;
        }
    }

    /// Whether `(col, row)` lands inside the last-rendered lineage preview
    /// box, if one is showing. Used to swallow clicks/drag-starts over the
    /// preview body so it doesn't act as a click-through onto the pane
    /// content underneath — mirrors `is_over_dynamic_ui_overlay`'s role for
    /// the widget popover, kept independent of that system.
    pub(super) fn is_over_lineage_preview(&self, col: u16, row: u16) -> bool {
        self.layout
            .lineage_preview_area
            .is_some_and(|area| Self::rect_contains(area, col, row))
    }

    /// `C-x Tab`: toggle the selected session's lineage preview between
    /// closed and "pinned + keyboard-focused" in one keystroke — the
    /// keyboard-only entry point that replaces the deleted `C-x q` / `q`
    /// popup. A no-op when there's no selected session, or the selected
    /// session has no fork/subagent lineage to show — the same gate the
    /// harness label's own hover/click affordance uses
    /// (`crate::lineage::has_lineage`).
    pub fn toggle_lineage_preview_focus(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        if !crate::lineage::has_lineage(&id, &self.sessions) {
            return;
        }
        // The preview's own key handler now owns subsequent keystrokes
        // directly (not the chord state machine), so reset it exactly like
        // other dialog-opening actions do (`open_configure_popup`,
        // `open_session_picker`, ...).
        self.chord_state = ChordState::default();
        self.chord_label.clear();
        if self.lineage_preview_focused.as_deref() == Some(id.as_str()) {
            self.lineage_preview_pinned.remove(&id);
            self.lineage_preview_focused = None;
        } else {
            self.activate_lineage_preview_focus(id);
        }
    }

    /// Pin `session_id`'s preview open and give it keyboard focus. Shared
    /// by the `C-x Tab` keyboard toggle and a click inside the preview's
    /// body — either path implies the user wants to keep interacting with
    /// it, so a preview that's about to auto-hide from a hover timeout
    /// shouldn't vanish out from under active keyboard interaction.
    ///
    /// The selection starts ON the session the preview was opened from —
    /// opening lineage from a fork or subagent highlights that session's
    /// own box, not the tree's root.
    pub(super) fn activate_lineage_preview_focus(&mut self, session_id: String) {
        self.lineage_preview_pinned.insert(session_id.clone());
        let rows = self.lineage_preview_rows(&session_id);
        let selectable = crate::lineage::selectable_indices(&rows);
        self.lineage_preview_selected = selectable
            .iter()
            .position(|&idx| rows[idx].session_id() == Some(session_id.as_str()))
            .unwrap_or(0);
        self.lineage_preview_scroll = 0;
        self.lineage_preview_focused = Some(session_id);
    }

    /// Materialize the current flattened rows for `session_id`'s lineage
    /// tree — used both by keyboard navigation here and by
    /// `ui::render_lineage_preview` to draw them. Rebuilt from live
    /// `App::sessions` on every call (no popup-owned copy to go stale), same
    /// approach the deleted modal's `lineage_rows` used.
    pub(crate) fn lineage_preview_rows(&self, session_id: &str) -> Vec<LineageRow> {
        self.lineage_preview_diagram(session_id).0
    }

    /// Rows plus every box's canvas bounds — the renderer maps the bounds
    /// to screen rects for hover/click hit-testing. Draws whichever
    /// visualization `lineage_preview_mode` selects; both modes share the
    /// same row/selection/hit model.
    pub(crate) fn lineage_preview_diagram(
        &self,
        session_id: &str,
    ) -> (Vec<LineageRow>, Vec<crate::lineage::LineageBoxBounds>) {
        let now_ms = chrono::Utc::now().timestamp_millis();
        crate::lineage::build_tree(session_id, &self.sessions)
            .map(|root| match self.lineage_preview_mode {
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
    /// preview belonging to `session_id`, if any (never a `More` marker row
    /// — those aren't selectable).
    fn lineage_preview_selected_session_id(&self, session_id: &str) -> Option<String> {
        let rows = self.lineage_preview_rows(session_id);
        let selectable = crate::lineage::selectable_indices(&rows);
        if selectable.is_empty() {
            return None;
        }
        let idx = selectable[self.lineage_preview_selected.min(selectable.len() - 1)];
        rows[idx].session_id().map(|s| s.to_string())
    }

    /// `j`/`k`/arrows/`C-n`/`C-p`: move the focused preview's selection.
    fn move_lineage_preview_selection(&mut self, delta: isize) {
        let Some(session_id) = self.lineage_preview_focused.clone() else {
            return;
        };
        let rows = self.lineage_preview_rows(&session_id);
        let selectable = crate::lineage::selectable_indices(&rows);
        if selectable.is_empty() {
            self.lineage_preview_selected = 0;
            return;
        }
        let count = selectable.len();
        let current = self.lineage_preview_selected.min(count - 1);
        self.lineage_preview_selected = if delta < 0 {
            current
                .saturating_add(count)
                .saturating_sub(delta.unsigned_abs() % count)
                % count
        } else {
            (current + delta as usize) % count
        };
    }

    /// Enter: jump into the highlighted session, then clear both focus and
    /// pin for this preview — mirroring the deleted modal's
    /// enter-jumps-and-closes behavior (leaving the preview to go work in
    /// that session makes it stop owning the keyboard AND stop pinning
    /// itself open).
    fn confirm_lineage_preview_selection(&mut self) {
        let Some(session_id) = self.lineage_preview_focused.take() else {
            return;
        };
        self.lineage_preview_pinned.remove(&session_id);
        let Some(target_id) = self.lineage_preview_selected_session_id(&session_id) else {
            return;
        };
        self.jump_to_lineage_session(&target_id);
    }

    /// Switch to a session picked from a lineage diagram — shared by Enter
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
            (Some(f), Some(m)) if m.mode == agentd_protocol::ForkMergeMode::Result => {
                f.session_id.clone()
            }
            _ => target_id.to_string(),
        };
        self.select_session(target);
        self.sync_active_window_selection();
        self.focus = PaneFocus::View;
    }

    /// `m` / `d`: merge or discard the focused preview's highlighted fork,
    /// reusing the exact merge/discard path the `C-x m` minibuffer menu and
    /// the deleted modal both used ([`App::apply_fork_merge`], spec 0078) —
    /// a direct-key shortcut for it, not a second implementation. A no-op
    /// with a status note when the highlighted row isn't an open (unmerged,
    /// undiscarded) fork.
    async fn lineage_preview_merge_or_discard(&mut self, mode: agentd_protocol::ForkMergeMode) {
        let Some(session_id) = self.lineage_preview_focused.clone() else {
            return;
        };
        let Some(id) = self.lineage_preview_selected_session_id(&session_id) else {
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
        // The preview stays open (still focused) — its rows are rebuilt
        // from live `self.sessions` on the very next render/key, so the
        // merged fork's terminal-state styling appears immediately without
        // any extra bookkeeping here.
    }

    /// Route a key while a lineage preview owns keyboard focus.
    /// Navigation/merge/discard/jump keys return `true` (fully handled; the
    /// preview stays focused unless it was Enter or Esc). Anything else
    /// clears focus and returns `false`, telling the caller (`App::on_key`)
    /// to re-dispatch the SAME key through ordinary routing — the same "a
    /// closing overlay never eats a live keystroke" rule the deleted
    /// modal's `handle_lineage_popup_key` followed, so e.g. `C-x C-c` still
    /// quits and `C-x b` still switches sessions while a preview is focused.
    ///
    /// Esc is deliberately asymmetric with the fallback arm: it clears focus
    /// ONLY, leaving any existing pin alone, so a preview the user explicitly
    /// pinned stays visible after they're done navigating it. (Un-pinning on
    /// Esc would make focus-then-Esc indistinguishable from closing the
    /// preview outright, which isn't what Esc means everywhere else in this
    /// UI — it backs out one level, it doesn't dismiss unrelated state.)
    pub(super) async fn handle_lineage_preview_focus_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.lineage_preview_focused = None;
                true
            }
            // `C-p`/`C-n` mirror the session-list's own emacs-style
            // NextSession/PrevSession bindings so navigation muscle memory
            // carries into this preview; `key.code` alone (crossterm reports
            // Ctrl+letter as `Char` with a CONTROL modifier, not a distinct
            // code) means these arms also accept bare `p`/`n`, same as `k`/`j`
            // below accept bare Up/Down without checking modifiers.
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Char('p') => {
                self.move_lineage_preview_selection(-1);
                true
            }
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Char('n') => {
                self.move_lineage_preview_selection(1);
                true
            }
            KeyCode::Enter => {
                self.confirm_lineage_preview_selection();
                true
            }
            KeyCode::Char('m') => {
                self.lineage_preview_merge_or_discard(agentd_protocol::ForkMergeMode::Result)
                    .await;
                true
            }
            KeyCode::Char('d') => {
                self.lineage_preview_merge_or_discard(agentd_protocol::ForkMergeMode::Discard)
                    .await;
                true
            }
            // `C-x Tab` (`ToggleLineagePreviewFocus`) is itself the keyboard
            // toggle that closes a focused preview — but it's a two-key
            // chord, and its own prefix key (`C-x`) isn't otherwise in this
            // handler's vocabulary. Without this arm, `C-x` would hit the
            // fallback below, clear focus, and fall through BEFORE `Tab`
            // ever arrives — so by the time the chord completes and
            // `App::toggle_lineage_preview_focus` runs, focus already reads
            // as "not focused" and it re-opens instead of closing. Let both
            // keys of the chord fall through without touching focus so the
            // toggle handler sees the true prior state and can tell open
            // from close correctly. `Char('x')` is checked with the CONTROL
            // modifier specifically (not bare `x`, which isn't otherwise
            // bound here); the chord's second key arrives as a bare `Tab`
            // per its `key(KeyCode::Tab)` binding in `keymap.rs`, no
            // modifier to check.
            KeyCode::Char('x') if key.modifiers.contains(KeyModifiers::CONTROL) => false,
            KeyCode::Tab => false,
            _ => {
                self.lineage_preview_focused = None;
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
            busy_ms: 0,
            busy_running_since_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: agentd_protocol::SessionKind::User,
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
        let client = agentd_client::Client::connect(&sock)
            .await
            .expect("client connects");
        let app = crate::app::tests::test_app(client, sessions);
        (app, dir, server)
    }

    fn fork_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.forked_from = Some(agentd_protocol::ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq: 0,
            at_ms: 0,
            parent_busy_ms: 0,
        });
        s
    }

    #[tokio::test]
    async fn toggle_focus_requires_a_selection() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![]).await;
        app.toggle_lineage_preview_focus();
        assert!(app.lineage_preview_focused.is_none());
    }

    #[tokio::test]
    async fn toggle_focus_is_a_no_op_without_lineage() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        assert!(
            app.lineage_preview_focused.is_none(),
            "an ordinary session with no lineage must not gain keyboard focus"
        );
        assert!(!app.lineage_preview_pinned.contains("root"));
    }

    #[tokio::test]
    async fn toggle_focus_pins_and_focuses_then_closes_on_second_press() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());

        app.toggle_lineage_preview_focus();
        assert_eq!(app.lineage_preview_focused.as_deref(), Some("root"));
        assert!(app.lineage_preview_pinned.contains("root"));

        app.toggle_lineage_preview_focus();
        assert!(
            app.lineage_preview_focused.is_none(),
            "a second press on the same session must close it"
        );
        assert!(
            !app.lineage_preview_pinned.contains("root"),
            "closing via C-x Tab must also un-pin"
        );
    }

    #[tokio::test]
    async fn c_x_tab_keystrokes_toggle_open_then_closed() {
        // Regression test for a bug where `toggle_lineage_preview_focus`
        // toggling correctly in isolation (see the test above) did NOT mean
        // the actual `C-x Tab` KEYSTROKES toggled correctly through
        // `App::on_key`'s chord dispatch: `C-x` isn't in this preview's own
        // key vocabulary, so on a second press it used to get treated as an
        // "unhandled key" that cleared focus and fell through BEFORE `Tab`
        // arrived — so by the time the chord completed, focus already read
        // as "not focused" and the toggle re-opened instead of closing.
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert_eq!(
            app.lineage_preview_focused.as_deref(),
            Some("root"),
            "first C-x Tab should open and focus the preview"
        );

        app.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
            .await;
        app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .await;
        assert!(
            app.lineage_preview_focused.is_none(),
            "second C-x Tab (as real keystrokes through on_key) must close it, \
             not re-open it"
        );
        assert!(
            !app.lineage_preview_pinned.contains("root"),
            "closing via C-x Tab must also un-pin"
        );
    }

    #[tokio::test]
    async fn ctrl_n_and_ctrl_p_navigate_like_j_and_k() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        assert_eq!(app.lineage_preview_selected, 0);
        app.handle_lineage_preview_focus_key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        ))
        .await;
        assert_eq!(app.lineage_preview_selected, 1);
        app.handle_lineage_preview_focus_key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ))
        .await;
        assert_eq!(app.lineage_preview_selected, 0);
    }

    #[tokio::test]
    async fn opening_from_a_fork_starts_selection_on_that_fork_not_the_root() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("fork".to_string());
        app.toggle_lineage_preview_focus();
        assert_eq!(app.lineage_preview_focused.as_deref(), Some("fork"));
        let rows = app.lineage_preview_rows("fork");
        let selectable = crate::lineage::selectable_indices(&rows);
        let selected_id = rows[selectable[app.lineage_preview_selected]]
            .session_id()
            .map(str::to_string);
        assert_eq!(
            selected_id.as_deref(),
            Some("fork"),
            "opening the preview from a fork must land the selection on \
             the fork's own box, not the tree's root"
        );
    }

    #[tokio::test]
    async fn esc_clears_focus_without_unpinning() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        assert!(app.lineage_preview_pinned.contains("root"));

        assert!(
            app.handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
                .await
        );
        assert!(app.lineage_preview_focused.is_none());
        assert!(
            app.lineage_preview_pinned.contains("root"),
            "Esc must clear focus only — the preview stays pinned/visible"
        );
    }

    #[tokio::test]
    async fn unhandled_key_clears_focus_and_reports_unhandled() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root")]).await;
        app.select_session("root".to_string());
        app.lineage_preview_focused = Some("root".to_string());
        app.lineage_preview_pinned.insert("root".to_string());
        let handled = app
            .handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE))
            .await;
        assert!(
            !handled,
            "an unbound key must fall through to ordinary routing"
        );
        assert!(app.lineage_preview_focused.is_none());
    }

    #[tokio::test]
    async fn enter_jumps_into_the_selected_session_and_clears_focus_and_pin() {
        let fork = fork_of(summary("fork"), "root");
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        // Move down onto the fork row.
        app.handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert!(app.lineage_preview_focused.is_none());
        assert!(!app.lineage_preview_pinned.contains("root"));
        assert_eq!(app.selected_id().as_deref(), Some("fork"));
    }

    #[tokio::test]
    async fn enter_on_a_merged_fork_jumps_to_the_parent_instead() {
        let mut fork = fork_of(summary("fork"), "root");
        fork.merge = Some(agentd_protocol::ForkMerge {
            mode: agentd_protocol::ForkMergeMode::Result,
            at_ms: 0,
            merged_busy_ms: 0,
            merged_seq: 0,
        });
        fork.archived = true;
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), fork]).await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        app.handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .await;
        app.handle_lineage_preview_focus_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.selected_id().as_deref(), Some("root"));
    }

    #[tokio::test]
    async fn merge_on_a_non_fork_row_is_a_status_only_no_op() {
        let (mut app, _dir, _server) = test_app_with_sessions(vec![summary("root"), {
            let mut sub = summary("sub");
            sub.kind = agentd_protocol::SessionKind::Subagent;
            sub.parent_session_id = Some("root".to_string());
            sub
        }])
        .await;
        app.select_session("root".to_string());
        app.toggle_lineage_preview_focus();
        assert!(
            app.handle_lineage_preview_focus_key(KeyEvent::new(
                KeyCode::Char('m'),
                KeyModifiers::NONE
            ))
            .await
        );
        // Still focused — nothing to merge, just a status note.
        assert!(app.lineage_preview_focused.is_some());
    }
}
