use std::time::Instant;

use construct_protocol;
use crossterm::event::{KeyCode, KeyEvent};

use super::App;
use super::{adjusted_scroll_offset, markdown_actions, markdown_display_rows, open_url};

impl App {
    pub(super) fn is_over_dynamic_ui_overlay(&self, col: u16, row: u16) -> bool {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(inline) = self.layout.dynamic_ui_inline_hit.as_ref() {
            return Self::rect_contains(inline.area, col, row);
        }
        self.layout
            .dynamic_ui_popover_area
            .is_some_and(|area| contains(area, col, row))
            || self
                .layout
                .dynamic_ui_dropdown_area
                .is_some_and(|area| contains(area, col, row))
    }

    fn focus_dynamic_ui_panel_at(&mut self, col: u16, row: u16) {
        let Some(session_id) = self.selected_id() else {
            self.dynamic_ui_focused = None;
            return;
        };
        let Some(panels) = self.ui_panels.get(&session_id) else {
            self.dynamic_ui_focused = None;
            return;
        };
        let Some(area) = self.layout.dynamic_ui_popover_area else {
            self.dynamic_ui_focused = None;
            return;
        };
        if !Self::rect_contains(area, col, row) {
            return;
        }
        let scroll = self
            .dynamic_ui_scroll_offsets
            .get(&session_id)
            .copied()
            .unwrap_or(0);
        let content_row = row.saturating_sub(area.y) as usize + scroll;
        let mut visible: Vec<_> = panels
            .values()
            .filter(|panel| self.dynamic_ui_panel_visible(&session_id, &panel.id))
            .collect();
        visible.sort_by(|a, b| a.id.cmp(&b.id));
        let mut cursor = 0usize;
        for (idx, panel) in visible.iter().enumerate() {
            if idx > 0 {
                cursor += 1;
            }
            let body_rows = markdown_display_rows(&panel.markdown);
            let panel_rows = 1usize.saturating_add(body_rows).saturating_add(1);
            if content_row >= cursor && content_row < cursor.saturating_add(panel_rows) {
                self.dynamic_ui_focused = Some((session_id, panel.id.clone()));
                return;
            }
            cursor = cursor.saturating_add(panel_rows);
        }
    }

    pub(super) async fn handle_dynamic_ui_overlay_click(&mut self, col: u16, row: u16) -> bool {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(inline) = self.layout.dynamic_ui_inline_hit.clone() {
            if let Some(hit) = self
                .layout
                .dynamic_ui_panel_close_hits
                .iter()
                .find(|hit| hit.contains(col, row))
                .cloned()
            {
                self.delete_dynamic_ui_panel(hit.session_id, hit.panel_id)
                    .await;
                return true;
            }
            if let Some(hit) = self
                .layout
                .dynamic_ui_action_hits
                .iter()
                .find(|hit| hit.contains(col, row))
                .cloned()
            {
                self.dynamic_ui_focused = Some((hit.session_id.clone(), hit.panel_id.clone()));
                self.dispatch_dynamic_ui_action(
                    hit.session_id.clone(),
                    Some(hit.panel_id.clone()),
                    hit.action.clone(),
                )
                .await;
                if hit.action.close {
                    self.delete_dynamic_ui_panel(hit.session_id, hit.panel_id)
                        .await;
                }
                return true;
            }
            if let Some(hit) = self
                .layout
                .dynamic_ui_url_hits
                .iter()
                .find(|hit| hit.contains(col, row))
                .cloned()
            {
                self.dynamic_ui_focused = Some((hit.session_id, hit.panel_id));
                open_url(&hit.url);
                return true;
            }
            if Self::rect_contains(inline.area, col, row) {
                return true;
            }
            return false;
        }
        if let Some(hit) = self
            .layout
            .dynamic_ui_panel_close_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            self.hide_dynamic_ui_panel(hit.session_id, hit.panel_id);
            return true;
        }
        if let Some(hit) = self
            .layout
            .dynamic_ui_action_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            self.dynamic_ui_focused = Some((hit.session_id.clone(), hit.panel_id.clone()));
            self.dispatch_dynamic_ui_action(
                hit.session_id.clone(),
                Some(hit.panel_id.clone()),
                hit.action.clone(),
            )
            .await;
            if hit.action.close {
                self.delete_dynamic_ui_panel(hit.session_id, hit.panel_id)
                    .await;
            }
            return true;
        }
        if let Some(hit) = self
            .layout
            .dynamic_ui_url_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            self.dynamic_ui_focused = Some((hit.session_id, hit.panel_id));
            open_url(&hit.url);
            return true;
        }
        if let Some(hit) = self
            .layout
            .dynamic_ui_widget_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            self.toggle_dynamic_ui_widget_pin(hit.session_id, hit.panel_id);
            return true;
        }
        for (x_start, x_end, y, session_id) in self.layout.dynamic_ui_triggers.clone() {
            if row == y && col >= x_start && col < x_end {
                self.dynamic_ui_popover_open =
                    if self.dynamic_ui_popover_open.as_deref() == Some(session_id.as_str()) {
                        None
                    } else {
                        Some(session_id)
                    };
                return true;
            }
        }
        if self.dynamic_ui_popover_open.is_some() {
            if let Some(dropdown) = self.layout.dynamic_ui_dropdown_area {
                if contains(dropdown, col, row) {
                    return true;
                }
            }
            if let Some(popover) = self.layout.dynamic_ui_popover_area {
                if contains(popover, col, row) {
                    self.focus_dynamic_ui_panel_at(col, row);
                    return true;
                }
                self.dynamic_ui_popover_open = None;
            }
        }
        if self.is_over_dynamic_ui_overlay(col, row) {
            self.focus_dynamic_ui_panel_at(col, row);
            return true;
        }
        false
    }

    pub(super) fn adjust_mouse_dynamic_ui_scroll(
        &mut self,
        col: u16,
        row: u16,
        delta: i32,
    ) -> bool {
        let Some(area) = self.layout.dynamic_ui_popover_area else {
            return false;
        };
        if !Self::rect_contains(area, col, row) {
            return false;
        }
        self.adjust_dynamic_ui_scroll(delta);
        true
    }

    fn adjust_dynamic_ui_scroll(&mut self, delta: i32) {
        let Some((session_id, content_rows, viewport_rows)) =
            self.layout.dynamic_ui_scroll_metrics.clone()
        else {
            return;
        };
        let max_scroll = content_rows.saturating_sub(viewport_rows);
        let current = self
            .dynamic_ui_scroll_offsets
            .get(&session_id)
            .copied()
            .unwrap_or(0);
        let next = adjusted_scroll_offset(current, delta, max_scroll);
        self.dynamic_ui_scroll_offsets.insert(session_id, next);
    }

    pub(super) async fn handle_inline_dynamic_ui_key(&mut self, key: KeyEvent) -> bool {
        let Some(inline) = self.layout.dynamic_ui_inline_hit.clone() else {
            return false;
        };
        if matches!(key.code, KeyCode::Esc) {
            self.delete_dynamic_ui_panel(inline.session_id, inline.panel_id)
                .await;
            return true;
        }
        if self.try_dynamic_ui_action_key(key).await {
            return true;
        }
        if let Some(action) = self.global_action_while_inline(key) {
            self.run_action(action).await;
            return true;
        }
        false
    }

    pub(super) async fn try_dynamic_ui_action_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != super::PaneFocus::View || !key.modifiers.is_empty() {
            return false;
        }
        let KeyCode::Char(c) = key.code else {
            return false;
        };
        if c.is_control() || c == '0' {
            return false;
        }
        let Some(session_id) = self.selected_id() else {
            return false;
        };
        let Some((focused_session, focused_panel)) = self.dynamic_ui_focused.clone() else {
            return false;
        };
        if focused_session != session_id {
            return false;
        }
        let inline_focused = self
            .layout
            .dynamic_ui_inline_hit
            .as_ref()
            .is_some_and(|hit| hit.session_id == session_id && hit.panel_id == focused_panel);
        if !inline_focused && !self.dynamic_ui_panel_visible(&session_id, &focused_panel) {
            return false;
        }
        let Some(action) = self.dynamic_ui_action_for_key(&session_id, c) else {
            return false;
        };
        self.dispatch_dynamic_ui_action(
            session_id.clone(),
            Some(focused_panel.clone()),
            action.clone(),
        )
        .await;
        if action.close {
            self.delete_dynamic_ui_panel(session_id, focused_panel)
                .await;
        }
        true
    }

    pub(super) fn try_dynamic_ui_scroll_key(&mut self, key: KeyEvent) -> bool {
        if self.focus != super::PaneFocus::View || self.dynamic_ui_focused.is_none() {
            return false;
        }
        let delta = match key.code {
            KeyCode::Up if key.modifiers.is_empty() => -1,
            KeyCode::Down if key.modifiers.is_empty() => 1,
            KeyCode::PageUp if key.modifiers.is_empty() => -10,
            KeyCode::PageDown if key.modifiers.is_empty() => 10,
            _ => return false,
        };
        self.adjust_dynamic_ui_scroll(delta);
        true
    }

    /// Deliver a UI action to `session_id` as user intent
    /// (`OBSERVATION: ui.action …`). `pub(super)` because the program editor's
    /// action-link click handler (spec 0074: action links are dialect-wide)
    /// dispatches through this same path, with `panel_id: None`.
    pub(super) async fn dispatch_dynamic_ui_action(
        &mut self,
        session_id: String,
        panel_id: Option<String>,
        action: construct_protocol::UiAction,
    ) {
        let label = action.label.clone();
        let action_id = action.id.clone();
        let text = if let Some(panel_id) = panel_id {
            format!(
                "OBSERVATION: ui.action {{\"panel_id\":\"{}\",\"action_id\":\"{}\",\"label\":\"{}\"}}",
                super::json_escape(&panel_id),
                super::json_escape(&action_id),
                super::json_escape(&label)
            )
        } else {
            format!(
                "OBSERVATION: ui.action {{\"action_id\":\"{}\",\"label\":\"{}\"}}",
                super::json_escape(&action_id),
                super::json_escape(&label)
            )
        };
        match self.client.send_input(&session_id, text).await {
            Ok(()) => self.set_status(format!("ui action: {label}")),
            Err(e) => self.set_status(format!("ui action failed: {e}")),
        }
    }

    /// Returns true if the widget is persistently pinned/selected or temporarily shown.
    /// Does NOT include hover preview state — use this for icon glyph decisions.
    pub fn dynamic_ui_panel_pinned(&self, session_id: &str, panel_id: &str) -> bool {
        let key = (session_id.to_string(), panel_id.to_string());
        if self.dynamic_ui_selected.contains(&key) {
            return true;
        }
        self.dynamic_ui_temporary_until
            .get(&key)
            .is_some_and(|until| *until > Instant::now())
    }

    /// Toggle a widget's pinned/selected state from a title-bar indicator
    /// click. Shared by the session pane title bar and the program title bar.
    pub fn toggle_dynamic_ui_widget_pin(&mut self, session_id: String, panel_id: String) {
        let key = (session_id, panel_id);
        if self.dynamic_ui_selected.contains(&key) {
            self.dynamic_ui_selected.remove(&key);
        } else {
            self.dynamic_ui_selected.insert(key.clone());
            self.dynamic_ui_temporary_until.remove(&key);
        }
        // The click outcome is authoritative; drop any hover preview of this
        // widget so the rendered state reflects the pin toggle immediately.
        if self
            .dynamic_ui_hover
            .as_ref()
            .is_some_and(|h| h.session_id == key.0 && h.panel_id == key.1)
        {
            self.dynamic_ui_hover = None;
        }
    }

    /// Returns true if the widget body should be rendered (pinned OR hover preview).
    pub fn dynamic_ui_panel_visible(&self, session_id: &str, panel_id: &str) -> bool {
        if self.dynamic_ui_panel_pinned(session_id, panel_id) {
            return true;
        }
        self.dynamic_ui_hover.as_ref().is_some_and(|h| {
            h.session_id == session_id && h.panel_id == panel_id && h.until > Instant::now()
        })
    }

    fn hide_dynamic_ui_panel(&mut self, session_id: String, panel_id: String) {
        let key = (session_id, panel_id);
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
    }

    async fn delete_dynamic_ui_panel(&mut self, session_id: String, panel_id: String) {
        self.hide_dynamic_ui_panel(session_id.clone(), panel_id.clone());
        if let Some(panels) = self.ui_panels.get_mut(&session_id) {
            panels.remove(&panel_id);
            if panels.is_empty() {
                self.ui_panels.remove(&session_id);
            }
        }
        if let Err(e) = self.client.delete_widget(&session_id, &panel_id).await {
            self.set_status(format!("widget close failed: {e}"));
        }
    }

    fn dynamic_ui_action_for_key(
        &self,
        session_id: &str,
        key: char,
    ) -> Option<construct_protocol::UiAction> {
        let panels = self.ui_panels.get(session_id)?;
        let focused_panel = self
            .dynamic_ui_focused
            .as_ref()
            .filter(|(focused_session, _)| focused_session == session_id)
            .map(|(_, panel_id)| panel_id.clone());
        let mut panel_ids: Vec<_> = if let Some(focused_panel) = focused_panel.as_ref() {
            vec![focused_panel]
        } else {
            panels.keys().collect()
        };
        panel_ids.sort();
        for panel_id in panel_ids {
            let panel = panels.get(panel_id)?;
            for action in markdown_actions(&panel.markdown) {
                if action.key.as_deref() == Some(&key.to_string()) {
                    return Some(action);
                }
            }
        }
        None
    }

    pub fn orchestrator_widget_panels(&self) -> Vec<construct_protocol::UiPanel> {
        let Some(orchestrator_id) = self.orchestrator_id.as_deref() else {
            return Vec::new();
        };
        let Some(panels) = self.ui_panels.get(orchestrator_id) else {
            return Vec::new();
        };
        let mut panels: Vec<_> = panels
            .values()
            .filter(|panel| panel.placement == construct_protocol::UiPlacement::Sticky)
            .cloned()
            .collect();
        panels.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        panels
    }

    /// Whether the operator widget viewport should render this frame.
    /// Side effect: expires a lapsed hover preview and clears all widget state
    /// when there's no orchestrator / no panels to show.
    pub fn matrix_widget_visible(&mut self, now: Instant) -> bool {
        if self.orchestrator_id.is_none() || self.orchestrator_widget_panels().is_empty() {
            self.matrix_widget_pinned = None;
            self.matrix_widget_hover = None;
            return false;
        }
        if self
            .matrix_widget_hover
            .as_ref()
            .is_some_and(|h| h.until <= now)
        {
            self.matrix_widget_hover = None;
        }
        self.matrix_widget_hover.is_some() || self.matrix_widget_pinned.is_some()
    }

    /// The operator widget to render in the rain viewport: a live hover preview
    /// takes precedence over the pinned widget; with neither, nothing shows.
    pub fn matrix_widget_shown(&self, now: Instant) -> Option<String> {
        if let Some(h) = self.matrix_widget_hover.as_ref() {
            if h.until > now {
                return Some(h.panel_id.clone());
            }
        }
        self.matrix_widget_pinned.clone()
    }

    pub fn toggle_matrix_widget_panel(&mut self, panel_id: String) {
        let panels = self.orchestrator_widget_panels();
        if !panels.iter().any(|panel| panel.id == panel_id) {
            self.matrix_widget_pinned = None;
            self.matrix_widget_hover = None;
            return;
        }
        if self.matrix_widget_pinned.as_deref() == Some(panel_id.as_str()) {
            self.matrix_widget_pinned = None;
        } else {
            self.matrix_widget_pinned = Some(panel_id);
        }
        // The click outcome is authoritative; drop any hover preview so the
        // rendered widget reflects the pin toggle immediately.
        self.matrix_widget_hover = None;
    }
}
