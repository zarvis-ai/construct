use crossterm::event::{MouseEvent, MouseEventKind};

use super::*;

impl App {
    /// True when `(col, row)` is a border that the current mouse dispatch
    /// treats as a resize handle. Kept alongside the hit-test helpers so the
    /// hover affordance cannot drift from the actual drag behavior.
    pub(crate) fn is_on_resize_handle(&self, col: u16, row: u16) -> bool {
        if self.is_on_list_divider(col, row) {
            return true;
        }

        // A program-toggle glyph on a horizontal split divider is clickable;
        // mouse-down deliberately lets it through instead of starting a drag.
        let on_pane_program_toggle = self.layout.main_window_areas.iter().any(|pane| {
            let (x_start, x_end, y) = crate::ui::view_program_toggle_button_range(pane.area);
            row == y && col >= x_start && col < x_end
        });
        if !on_pane_program_toggle
            && self
                .layout
                .main_window_dividers
                .iter()
                .any(|hit| Self::rect_contains(hit.area, col, row))
        {
            return true;
        }

        self.is_on_pin_strip_divider(col, row)
            || self.is_on_orchestrator_panel_divider(col, row)
            || self.is_on_matrix_rain_title_bar(col, row)
            || self
                .layout
                .program_resize_hit
                .is_some_and(|hit| Self::rect_contains(hit, col, row))
    }

    pub(super) fn selection_bounds_at(&self, col: u16, row: u16) -> Option<ratatui::layout::Rect> {
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
    pub(super) fn is_on_pin_strip_divider(&self, col: u16, row: u16) -> bool {
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

    /// True if `(col, row)` sits on the orchestrator/operator panel's top border.
    /// That border is the visible horizontal title line when operator is focused
    /// and is used as a vertical resize handle.
    pub(super) fn is_on_orchestrator_panel_divider(&self, col: u16, row: u16) -> bool {
        if !self.is_orchestrator_panel_open() {
            return false;
        }
        let Some(area) = self.layout.minibuffer_area else {
            return false;
        };
        area.height > 1 && row == area.y && col >= area.x && col < area.x + area.width
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
    pub(super) fn is_on_list_divider(&self, col: u16, row: u16) -> bool {
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

    pub(super) fn url_hit_at(&self, col: u16, row: u16) -> Option<UrlHit> {
        let bounds = self.url_click_bounds(col, row)?;
        url_hit_in_frame(&self.frame_text, col, row, bounds)
    }

    fn url_click_bounds(&self, col: u16, row: u16) -> Option<ratatui::layout::Rect> {
        fn contains(r: ratatui::layout::Rect, c: u16, y: u16) -> bool {
            c >= r.x && c < r.x + r.width && y >= r.y && y < r.y + r.height
        }
        if let Some(view) = self.layout.view_area {
            // Zoomed mode renders edge-to-edge without any border; the view area
            // IS the content area.  Normal (split) mode wraps content in a 1-cell
            // border on all sides, so we shrink by 1 in that case only.
            let inner = if matches!(self.zoom, ZoomMode::None) {
                ratatui::layout::Rect {
                    x: view.x.saturating_add(1),
                    y: view.y.saturating_add(1),
                    width: view.width.saturating_sub(2),
                    height: view.height.saturating_sub(2),
                }
            } else {
                view
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

    pub(super) fn rect_contains(r: ratatui::layout::Rect, col: u16, row: u16) -> bool {
        col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
    }

    pub(super) fn begin_terminal_scrollbar_drag_or_jump(&mut self, col: u16, row: u16) -> bool {
        let Some(hit) = self.layout.terminal_scrollbar else {
            return false;
        };
        if !Self::rect_contains(hit.area, col, row) {
            return false;
        }
        let grab_offset = if Self::rect_contains(hit.thumb, col, row) {
            row.saturating_sub(hit.thumb.y)
        } else {
            hit.thumb.height / 2
        };
        self.dragging_terminal_scrollbar = Some((grab_offset, hit.max_scrollback));
        self.drag_terminal_scrollbar_to_row(row, grab_offset, hit.max_scrollback);
        true
    }

    pub(super) fn drag_terminal_scrollbar_to_row(
        &mut self,
        row: u16,
        grab_offset: u16,
        max_scrollback: usize,
    ) {
        let Some(hit) = self.layout.terminal_scrollbar else {
            return;
        };
        if hit.area.height == 0 || hit.thumb.height >= hit.area.height || max_scrollback == 0 {
            return;
        }
        let max_thumb_top = hit.area.height.saturating_sub(hit.thumb.height) as usize;
        if max_thumb_top == 0 {
            return;
        }
        let thumb_top = row
            .saturating_sub(grab_offset)
            .saturating_sub(hit.area.y)
            .min(hit.area.height.saturating_sub(hit.thumb.height)) as usize;
        let from_top = (thumb_top * max_scrollback + max_thumb_top / 2) / max_thumb_top;
        let active_window = Some(self.active_window_id);
        self.set_scrollback_for_window(active_window, max_scrollback.saturating_sub(from_top));
        self.show_terminal_scrollbar();
    }

    pub(super) fn adjust_mouse_list_scroll(&mut self, col: u16, row: u16, delta: i32) -> bool {
        let Some(area) = self.layout.list_items_area else {
            return false;
        };
        if col < area.x || col >= area.x + area.width || row < area.y || row >= area.y + area.height {
            return false;
        }
        self.adjust_list_scroll(delta);
        true
    }

    /// True when `(col, row)` lands inside the floating tutorial card
    /// rendered this frame. Used to keep the card's own click handling (and
    /// the URL-click intercept's sibling logic) from being shadowed by
    /// `forward_mouse_to_child` when the pane underneath has grabbed the
    /// mouse — see `LayoutSnapshot::tutorial_card_area`.
    pub(super) fn mouse_over_tutorial_card(&self, col: u16, row: u16) -> bool {
        self.layout
            .tutorial_card_area
            .is_some_and(|r| Self::rect_contains(r, col, row))
    }

    /// Forward a mouse event into the child PTY of the pane under the cursor,
    /// if that child has requested mouse tracking. Returns `true` when the
    /// event was consumed (encoded and queued for the child), `false` when no
    /// such pane is under the cursor or the child isn't tracking the mouse —
    /// in which case the caller handles the event with construct's own logic.
    pub(super) fn forward_mouse_to_child(&mut self, ev: &MouseEvent) -> bool {
        // Which pane's content area is the cursor over? Borders are excluded
        // (`inner_area`), so divider drags and frame clicks fall through.
        let Some(hit) = self
            .layout
            .main_window_areas
            .iter()
            .find(|h| Self::rect_contains(h.inner_area, ev.column, ev.row))
            .copied()
        else {
            return false;
        };
        // Which session is shown there?
        let Some(session_id) = self
            .main_windows
            .find_selection(hit.id)
            .and_then(|sel| sel.session_id())
            .map(str::to_string)
        else {
            return false;
        };
        // Has that session's child grabbed the mouse, and how does it want
        // the report framed?
        let Some(history) = self.histories.get(&session_id) else {
            return false;
        };
        let mode = history.mouse_protocol_mode();
        if mode == vt100::MouseProtocolMode::None {
            return false;
        }
        let encoding = history.mouse_protocol_encoding();
        // Translate to 1-based coordinates local to the child's screen.
        let col = ev.column.saturating_sub(hit.inner_area.x) + 1;
        let row = ev.row.saturating_sub(hit.inner_area.y) + 1;
        let Some(bytes) = crate::mouse_forward::encode(ev, col, row, mode, encoding) else {
            // Child tracks the mouse but doesn't report this event kind under
            // its mode (e.g. plain motion in press/release mode). Let construct
            // keep its own handling rather than swallowing it silently.
            return false;
        };
        // A button press is a deliberate intent to interact with this pane, so
        // move construct's keyboard focus here before forwarding — otherwise a
        // click inside a mouse-grabbing child (e.g. Claude Code in fullscreen)
        // reaches the child but never focuses the pane, and keystrokes keep
        // going elsewhere. Focus is construct-side only and leaves the report
        // sent down the PTY untouched, so the pass-through stays faithful. Wheel
        // and motion events forward without stealing focus.
        if matches!(ev.kind, MouseEventKind::Down(_)) {
            self.focus_main_window(hit.id);
        }
        self.queue_pty_input(session_id, bytes, "mouse");
        true
    }

    pub(super) fn adjust_mouse_scrollback(&mut self, col: u16, row: u16, delta: i32) {
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
        let target_window = self
            .layout
            .main_window_areas
            .iter()
            .find(|hit| Self::rect_contains(hit.inner_area, col, row))
            .map(|hit| hit.id);
        if let Some(window_id) = target_window {
            self.focus_main_window(window_id);
        }
        if self.view == ViewMode::Terminal && self.in_pty_session() {
            let scroll_window = Some(self.active_window_id);
            let next = adjusted_scrollback(self.scrollback_for_window(scroll_window), delta);
            self.set_scrollback_for_window(scroll_window, next);
            self.show_terminal_scrollbar();
        } else if self.view == ViewMode::Chat {
            self.adjust_chat_scroll(delta);
        }
    }
}
