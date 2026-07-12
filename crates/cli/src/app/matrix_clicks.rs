use super::{
    list_session_indent_cells, App, ListItem, MatrixWidgetHitKind, MinibufferChoiceAction,
    MinibufferIntent, PaneFocus, SESSION_LIST_H_MIN,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl App {
    pub(super) fn is_on_matrix_rain_title_bar(&self, col: u16, row: u16) -> bool {
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
        if let Some((xs, xe, y)) = self.layout.matrix_operator_title_hit {
            if row == y && col >= xs && col < xe {
                return false;
            }
        }
        if let Some((xs, xe, y)) = self.layout.matrix_operator_loop_hit {
            if row == y && col >= xs && col < xe {
                return false;
            }
        }
        if self
            .layout
            .matrix_widget_hits
            .iter()
            .any(|hit| hit.contains(col, row))
        {
            return false;
        }
        true
    }

    pub(super) fn matrix_rain_available_height(&self) -> Option<u16> {
        let list = self.layout.list_area?;
        let inner_h = list.height.saturating_sub(2);
        // The matrix panel is sticky and may shrink the visible item
        // window, but it's clamped so the list always keeps at least
        // SESSION_LIST_H_MIN rows when both are shown.
        Some(inner_h.saturating_sub(SESSION_LIST_H_MIN))
    }

    pub(super) async fn click_minibuffer(&mut self, mb_area: ratatui::layout::Rect, col: u16) {
        if let Some(mb) = self.minibuffer.as_mut() {
            // Harness picker: clicking an available name submits it
            // as if the user typed and pressed Enter. Unavailable
            // names are visually disabled (strikethrough); clicks
            // on them drop a status note rather than submitting —
            // the hover tooltip explains why.
            if matches!(
                mb.intent,
                MinibufferIntent::NewSessionHarness | MinibufferIntent::ForkSessionHarness { .. }
            ) {
                let hits = self.layout.minibuffer_harness_hits.clone();
                for hit in hits {
                    if hit.y == mb_area.y && col >= hit.x_start && col < hit.x_end {
                        if !hit.available {
                            let reason = hit.detail.as_deref().unwrap_or("not available");
                            self.set_status(format!("{}: {reason}", hit.name));
                            return;
                        }
                        let intent = mb.intent.clone();
                        self.minibuffer = None;
                        self.run_minibuffer_submit(intent, hit.name).await;
                        return;
                    }
                }
            }
            // Confirm/approval choice clusters (spec 0075: `y`/`N`,
            // `d`/`a`/`N`, `y=approve`/`n=deny`/..., ...). A click on a
            // rendered choice label dispatches exactly as the matching
            // keypress would, through whichever of the two keyboard
            // mechanisms the intent already uses — never a third,
            // click-only decision path. This replaces the previous
            // blanket no-op for `ApproveTool` with real per-choice
            // handling, and is the only place any of these intents gets
            // mouse support at all.
            let choice_hits = self.layout.minibuffer_choice_hits.clone();
            for hit in choice_hits {
                if hit.y == mb_area.y && col >= hit.x_start && col < hit.x_end {
                    match hit.action {
                        MinibufferChoiceAction::Key(c) => {
                            let key = KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
                            self.handle_minibuffer_key(key).await;
                        }
                        MinibufferChoiceAction::Submit(choice) => {
                            let intent = mb.intent.clone();
                            self.minibuffer = None;
                            self.run_minibuffer_submit(intent, choice).await;
                        }
                    }
                    return;
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
            self.run_action(crate::keymap::KeyAction::OpenCommandPalette)
                .await;
        }
    }

    pub(super) async fn click_list(&mut self, list: ratatui::layout::Rect, col: u16, row: u16) {
        // Matrix-rain title-bar controls are part of the Operator surface, not a
        // request to focus the session list. The title bar stays visible even
        // when the panel is collapsed (only the bar shows), so handle its
        // controls regardless of collapsed state, before the generic focus path.
        if let Some(rain) = self.layout.matrix_rain_area {
            if let Some((xs, xe, y)) = crate::ui::matrix_rain_close_button_range(rain) {
                if row == y && col >= xs && col < xe {
                    self.matrix_rain_hidden = !self.matrix_rain_hidden;
                    let status = if self.matrix_rain_hidden {
                        "matrix rain collapsed"
                    } else {
                        "matrix rain expanded"
                    };
                    self.set_status(status.into());
                    return;
                }
            }
            if let Some(hit) = self
                .layout
                .matrix_widget_hits
                .iter()
                .find(|hit| hit.contains(col, row))
                .cloned()
            {
                match hit.kind {
                    MatrixWidgetHitKind::Select { panel_id } => {
                        self.toggle_matrix_widget_panel(panel_id)
                    }
                }
                return;
            }
            if let Some((xs, xe, y)) = self.layout.matrix_operator_loop_hit {
                if row == y && col >= xs && col < xe {
                    if let Some(id) = self.orchestrator_id.clone() {
                        let cmd = if self.operator_loop_disabled() {
                            "/operator enable"
                        } else {
                            "/operator disable"
                        };
                        let _ = self.client.send_input(&id, cmd.to_string()).await;
                    }
                    return;
                }
            }
            if let Some((xs, xe, y)) = self.layout.matrix_operator_title_hit {
                if row == y && col >= xs && col < xe {
                    self.toggle_orchestrator_panel();
                    return;
                }
            }
        }
        // Lineage section (spec 0081): the header's mode toggle, the header
        // itself (collapse), a session box (jump), then anywhere else inside
        // the section (keyboard focus) — all before the generic focus/row
        // path so a section click never doubles as a row selection.
        if self
            .layout
            .lineage_toggle_hit
            .is_some_and(|r| Self::rect_contains(r, col, row))
        {
            self.lineage_mode = self.lineage_mode.toggled();
            // The two modes have different geometries — stale scroll
            // offsets from one would land nowhere in the other, including
            // cached viewports for lineages not currently selected.
            self.lineage_scroll = 0;
            self.lineage_scroll_x = 0;
            self.lineage_scroll_memory.clear();
            return;
        }
        if self
            .layout
            .lineage_collapse_hit
            .is_some_and(|r| Self::rect_contains(r, col, row))
        {
            self.lineage_collapsed = !self.lineage_collapsed;
            if self.lineage_collapsed {
                self.lineage_focused = false;
                self.lineage_h = None;
            }
            return;
        }
        if let Some(hit) = self
            .layout
            .lineage_subagent_toggle_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            if !self.lineage_subagents_expanded.remove(&hit.session_id) {
                self.lineage_subagents_expanded.insert(hit.session_id);
            }
            return;
        }
        if let Some(hit) = self
            .layout
            .lineage_box_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .cloned()
        {
            self.lineage_focused = false;
            self.jump_to_lineage_session(&hit.session_id);
            return;
        }
        if self.is_over_lineage_section(col, row) {
            self.activate_lineage_focus();
            return;
        }
        // A click anywhere inside the list pane focuses it, even on the
        // border or empty space past the last item — matching the
        // intuitive "click the pane to focus it" UX. Clicking the rows
        // region also settles the sidebar's sub-focus back on the rows
        // (the lineage-section arms above returned before this point).
        self.lineage_focused = false;
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
        // Session rows reserve disclosure before the 4-cell pin/status gutter.
        // Disclosure clicks toggle nested subagents/forks; the gutter toggles
        // pinning. Must stay in lockstep with `hovered_diamond` in ui.rs.
        if let ListItem::Session {
            summary,
            indented,
            has_children,
            ..
        } = &items[idx]
        {
            let indent = list_session_indent_cells(summary, *indented, *has_children);
            let disclosure_col = list.x + 1 + indent;
            if *has_children && col == disclosure_col {
                let id = summary.id.clone();
                if !self.children_collapsed.insert(id.clone()) {
                    self.children_collapsed.remove(&id);
                }
                return;
            }
            let zone_start = disclosure_col + u16::from(*has_children);
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
                self.sync_active_window_selection();
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
                    self.sync_active_window_selection();
                }
                if let Err(e) = self.client.set_project_collapsed(&id, next).await {
                    self.set_status(format!("collapse failed: {e}"));
                }
            }
            ListItem::ArchivedRow { section, .. } => {
                let section = section.clone();
                self.select_archive_row(section.clone());
                self.sync_active_window_selection();
                self.toggle_archive_section(&section);
            }
        }
    }

    pub(super) async fn click_pin_strip(
        &mut self,
        strip: ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) {
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
            // Body click: focus the pinned preview for input, but do not
            // replace the active main-window session. Main-window session
            // changes still use the normal glitch transition; clicking a live
            // pinned tile is only a focus handoff to that tile.
            self.select_session_without_transition(id.clone());
            self.collapse_orchestrator_panel_on_focus_change();
            self.focus = PaneFocus::View;
            return;
        }
    }
}
