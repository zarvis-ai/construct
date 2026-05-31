//! Ratatui rendering for the TUI.

use crate::app::{
    App, HarnessHit, HintZone, ListItem as AppListItem, MainWindowTree, Minibuffer,
    MinibufferIntent, PaneFocus, ScreenPoint, Selection, TextSelectionRange, ViewMode,
    WindowDividerHit, WindowPaneHit, WindowSplitDirection, ZoomMode,
};
use crate::keymap::KeyAction;
use crate::theme::Theme;
use agentd_protocol::{MessageRole, SessionEvent, SessionState, SessionSummary, TimestampedEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

const MATRIX_RAIN_RAMP_UP_SECS: f32 = 5.0;
const MATRIX_RAIN_DECAY_SECS: f32 = 20.0;
/// Brightness multiplier for the browser-preview wallpaper behind the
/// matrix rain — kept very dim so the green rain clearly stays the
/// foreground and the image reads as a faint backdrop.
const MATRIX_WALLPAPER_DIM: f32 = 0.22;
/// Seconds for a browser preview's top-to-bottom "dial-up" reveal on
/// appear, and the top-to-bottom erase on disappear. Applies to both the
/// terminal-view overlay and the matrix-rain wallpaper.
const PREVIEW_REVEAL_SECS: f32 = 1.0;

/// Row-fraction range `[start, end)` of a preview image to paint this
/// frame. On appear the image fills from the top over `PREVIEW_REVEAL_SECS`
/// (range `(0, a)`); while shown it's full (`(0, 1)`); on disappear it
/// erases from the top over the last `PREVIEW_REVEAL_SECS` before
/// `hide_after` (range `(d, 1)` — only the bottom remains). The same
/// curve drives the overlay and the wallpaper so they stay in sync.
fn preview_reveal_range(
    revealed_at: std::time::Instant,
    hide_after: std::time::Instant,
    now: std::time::Instant,
    hovered: bool,
) -> (f32, f32) {
    let appear = (now.saturating_duration_since(revealed_at).as_secs_f32() / PREVIEW_REVEAL_SECS)
        .clamp(0.0, 1.0);
    let remaining = hide_after.saturating_duration_since(now).as_secs_f32();
    // Hovering pins the preview (the expiry timer is frozen), so don't
    // play the disappear erase while the cursor is over it.
    let disappear = if !hovered && remaining < PREVIEW_REVEAL_SECS {
        (1.0 - remaining / PREVIEW_REVEAL_SECS).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (disappear, appear)
}
const MATRIX_RAIN_TAIL_MIN: u16 = 5;
const MATRIX_RAIN_TAIL_MAX: u16 = 9;

fn clear_pane_side_borders(f: &mut Frame, area: Rect, app: &App) {
    if !app.hide_pane_side_borders || area.width == 0 || area.height <= 1 {
        return;
    }
    let side_y = area.y.saturating_add(1);
    let side_h = area.height.saturating_sub(1);
    f.render_widget(
        Clear,
        Rect {
            x: area.x,
            y: side_y,
            width: 1,
            height: side_h,
        },
    );
    if area.width > 1 {
        f.render_widget(
            Clear,
            Rect {
                x: area.x + area.width - 1,
                y: side_y,
                width: 1,
                height: side_h,
            },
        );
    }
    f.render_widget(
        Clear,
        Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        },
    );
}

pub fn render(f: &mut Frame, app: &mut App) {
    app.layout.browser_preview_area = None;
    app.layout.browser_preview_close = None;
    app.layout.terminal_scrollbar = None;
    app.layout.dynamic_ui_action_hits.clear();
    app.layout.dynamic_ui_url_hits.clear();
    app.layout.dynamic_ui_widget_hits.clear();
    app.layout.dynamic_ui_panel_close_hits.clear();
    app.layout.dynamic_ui_inline_hit = None;
    app.layout.dynamic_ui_trigger = None;
    app.layout.dynamic_ui_triggers.clear();
    app.layout.shortcut_hints.clear();
    app.layout.main_window_areas.clear();
    app.layout.main_window_dividers.clear();
    app.window_pane_sizes.clear();
    app.layout.dynamic_ui_popover_area = None;
    app.layout.dynamic_ui_scroll_metrics = None;
    let area = f.area();
    match app.zoom {
        ZoomMode::View => {
            render_zoomed_view(f, area, app);
            finish_frame(f, app);
            return;
        }
        ZoomMode::List => {
            render_zoomed_list(f, area, app);
            finish_frame(f, app);
            return;
        }
        ZoomMode::None => {}
    }
    let footer_h = compute_minibuffer_height(app, area.height);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(footer_h),
        ])
        .split(area);
    let main_area = vertical[0];
    let modeline_area = vertical[1];
    let minibuffer_area = vertical[2];

    // Clear the main area and chrome regions so any stale cells from prior
    // larger editor popups don't persist visually. This forces a full
    // repaint of the content region and avoids relying on a terminal
    // resize to flush artifacts.
    f.render_widget(Clear, main_area);
    f.render_widget(Clear, modeline_area);
    f.render_widget(Clear, minibuffer_area);

    // Clamp the user-adjusted list width to leave room for the view
    // pane on narrow terminals. The drag handler stores the raw width
    // (so the user's intent is preserved when the terminal grows
    // again), and we just clamp at render time. When the user has
    // collapsed the list AND it's not currently focused, render it
    // at the minimal `LIST_PANEL_W_COLLAPSED` (3 cells) instead — a
    // small strip with an expand affordance. Focus on the list
    // temporarily expands so the user can interact with it.
    let effective_collapsed = app.list_collapsed && app.focus != PaneFocus::List;
    let list_w = if effective_collapsed {
        crate::app::LIST_PANEL_W_COLLAPSED
    } else {
        let max_list_w = main_area
            .width
            .saturating_sub(crate::app::LIST_PANEL_W_VIEW_MIN)
            .max(crate::app::LIST_PANEL_W_MIN);
        app.list_panel_w
            .clamp(crate::app::LIST_PANEL_W_MIN, max_list_w)
    };
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(list_w), Constraint::Min(0)])
        .split(main_area);
    let right_area = cols[1];

    // Split the right area into main + pin strip if any sessions are pinned.
    // Walk the materialized list so the pin strip's order matches what the
    // user sees in the left list (including groups and within-group order).
    let pinned_ids: Vec<String> = app
        .list_items()
        .into_iter()
        .filter_map(|it| match it {
            AppListItem::Session { summary, .. } if summary.pinned => Some(summary.id),
            _ => None,
        })
        .collect();
    let (detail_area, pin_strip_area) = if pinned_ids.is_empty() {
        (right_area, None)
    } else {
        // Honor the user's persisted preference when present; clamp
        // against the right pane so we never starve the main view on
        // a small terminal regardless of what was saved.
        let upper = right_area
            .height
            .saturating_sub(10)
            .max(crate::app::PIN_STRIP_H_MIN);
        let strip_h = app
            .pin_strip_h
            .map(|h| h.clamp(crate::app::PIN_STRIP_H_MIN, upper))
            .unwrap_or_else(|| pin_strip_height(right_area.height));
        let vsplit = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(strip_h)])
            .split(right_area);
        (vsplit[0], Some(vsplit[1]))
    };

    // The PTY pane size tracks the *main* view's inner area; pinned tiles
    // are passive tails reading from the same parser. Resize all parsers
    // before drawing so the current frame's parser screen geometry matches
    // the area we're rendering into (otherwise zoom-in shows blank rows at
    // the bottom and zoom-out clips content).
    let inner_cols = detail_area.width.saturating_sub(2);
    let inner_rows = detail_area.height.saturating_sub(2);
    app.terminal_pane_size = (inner_cols, inner_rows);
    // No need to pre-size per-session vt100 parsers — the items
    // model rebuilds a fresh parser at the current pane size on
    // every render. Scroll offset is applied inside `replay`.

    // Record the frame's pane geometry so the mouse-click handler can
    // map terminal coordinates back to a region.
    app.layout.list_area = Some(cols[0]);
    app.layout.view_area = Some(detail_area);
    app.layout.pin_strip_area = pin_strip_area;
    app.layout.matrix_rain_area = None;
    app.layout.minibuffer_area = Some(minibuffer_area);
    app.layout.modal_area = None;
    app.layout.list_row_count = app.list_items().len();
    app.layout.list_items_area = None;
    app.layout.list_scroll_offset = 0;

    if list_w > 0 {
        render_sessions(f, cols[0], app);
    }
    render_main_windows(f, detail_area, app);
    if let Some(strip) = pin_strip_area {
        render_pin_strip(f, strip, app, &pinned_ids);
    }
    // When the list is collapsed, overlay a `›` uncollapse glyph
    // on the main view's left border so the user can recover the
    // hidden pane without a key chord. Painted AFTER `render_detail`
    // so it sits on top of the block's border. Click handler in
    // `app.rs` mirrors the geometry via `view_uncollapse_glyph_pos`.
    if effective_collapsed {
        render_view_uncollapse_glyph(f, app, detail_area);
    }
    render_modeline(f, modeline_area, app);
    render_minibuffer(f, minibuffer_area, app);
    render_diamond_tooltip(f, app);
    render_pin_diamond_tooltip(f, app, &pinned_ids);
    render_view_close_tooltip(f, app);
    render_browser_preview_close_tooltip(f, app);
    render_list_title_button_tooltips(f, app);
    render_view_uncollapse_tooltip(f, app);
    render_harness_unavailable_tooltip(f, app);
    render_tasks_popup(f, app);
    render_remote_control_popup(f, app);
    if app.help_visible {
        app.layout.modal_area = Some(render_help(f, area, &app.theme));
    }
    finish_frame(f, app);
}

fn finish_frame(f: &mut Frame, app: &mut App) {
    capture_frame_text(f, app);
    render_hovered_url(f, app);
    render_text_selection(f, app);
}

fn capture_frame_text(f: &mut Frame, app: &mut App) {
    let area = *f.buffer_mut().area();
    let mut rows = Vec::with_capacity(area.height as usize);
    for y in area.top()..area.bottom() {
        let mut line = String::new();
        for x in area.left()..area.right() {
            let symbol = f
                .buffer_mut()
                .cell(Position { x, y })
                .map(|c| c.symbol())
                .unwrap_or(" ");
            if symbol.is_empty() {
                line.push(' ');
            } else {
                line.push_str(symbol);
            }
        }
        rows.push(line);
    }
    app.frame_text = rows;
}

fn render_text_selection(f: &mut Frame, app: &App) {
    let area = *f.buffer_mut().area();
    if area.width == 0 || area.height == 0 {
        return;
    }
    let style = Style::default()
        .bg(app.theme.highlight_bg)
        .fg(app.theme.highlight_fg);
    if let Some(sel) = &app.text_selection {
        if sel.dragged {
            let (start, end) = normalized_points(sel.anchor, sel.head);
            render_selection_rect(f, sel.bounds.unwrap_or(area), start, end, style);
        }
        return;
    }
    if let Some(text) = &app.selected_text {
        for (row, start_col, end_col) in find_text_ranges(
            &app.frame_text,
            text,
            app.selected_text_bounds,
            app.selected_text_range,
        ) {
            let start = ScreenPoint {
                col: start_col,
                row,
            };
            let end = ScreenPoint { col: end_col, row };
            render_selection_rect(
                f,
                app.selected_text_bounds.unwrap_or(area),
                start,
                end,
                style,
            );
        }
    }
}

fn render_hovered_url(f: &mut Frame, app: &App) {
    let Some(hit) = app.hovered_url() else {
        return;
    };
    let area = *f.buffer_mut().area();
    for range in hit.ranges {
        if range.row < area.top() || range.row >= area.bottom() {
            continue;
        }
        let start = range.start_col.max(area.left());
        let end = range.end_col.min(area.right());
        if start >= end {
            continue;
        }
        for x in start..end {
            if let Some(cell) = f.buffer_mut().cell_mut(Position { x, y: range.row }) {
                cell.set_style(cell.style().add_modifier(Modifier::UNDERLINED));
            }
        }
    }
}

fn render_selection_rect(
    f: &mut Frame,
    area: Rect,
    start: ScreenPoint,
    end: ScreenPoint,
    style: Style,
) {
    let max_x = area.right().saturating_sub(1);
    for row in start.row..=end.row {
        if row < area.top() || row >= area.bottom() {
            continue;
        }
        let x_start = if row == start.row {
            start.col
        } else {
            area.left()
        }
        .clamp(area.left(), max_x);
        let x_end = if row == end.row { end.col } else { max_x }.clamp(area.left(), max_x);
        if x_end < x_start {
            continue;
        }
        for x in x_start..=x_end {
            if let Some(cell) = f.buffer_mut().cell_mut(Position { x, y: row }) {
                cell.set_style(style);
            }
        }
    }
}

fn find_text_ranges(
    frame_text: &[String],
    selected: &str,
    bounds: Option<Rect>,
    original: Option<TextSelectionRange>,
) -> Vec<(u16, u16, u16)> {
    let selected_lines: Vec<&str> = selected.lines().collect();
    if selected_lines.is_empty() {
        return Vec::new();
    }
    let first_row = bounds.map(|b| b.top() as usize).unwrap_or(0);
    let end_row = bounds
        .map(|b| b.bottom() as usize)
        .unwrap_or(frame_text.len())
        .min(frame_text.len());
    let left_col = bounds.map(|b| b.left() as usize).unwrap_or(0);
    let right_col = bounds.map(|b| b.right() as usize);
    let mut matches = Vec::new();
    'row: for row in first_row..end_row {
        if row + selected_lines.len() > end_row {
            break;
        }
        let mut ranges = Vec::with_capacity(selected_lines.len());
        for (offset, wanted) in selected_lines.iter().enumerate() {
            if wanted.is_empty() {
                ranges.push(((row + offset) as u16, left_col as u16, left_col as u16));
                continue;
            }
            let line = &frame_text[row + offset];
            let line_cols = line.chars().count();
            let search_left = left_col.min(line_cols);
            let search_right = right_col.unwrap_or(line_cols).min(line_cols);
            if search_left >= search_right {
                continue 'row;
            };
            let prefix_bytes = byte_index_for_col(line, search_left);
            let suffix_bytes = byte_index_for_col(line, search_right);
            let haystack = &line[prefix_bytes..suffix_bytes];
            let Some(byte_col) = haystack.find(wanted) else {
                continue 'row;
            };
            let start_col = search_left + haystack[..byte_col].chars().count();
            let end_col = start_col + wanted.chars().count().saturating_sub(1);
            ranges.push(((row + offset) as u16, start_col as u16, end_col as u16));
        }
        matches.push(ranges);
    }
    let Some(original) = original else {
        return matches.into_iter().next().unwrap_or_default();
    };
    matches
        .into_iter()
        .min_by_key(|ranges| {
            ranges.first().map_or(u32::MAX, |(row, col, _)| {
                original.start.row.abs_diff(*row) as u32 * 1024
                    + original.start.col.abs_diff(*col) as u32
            })
        })
        .unwrap_or_default()
}

fn byte_index_for_col(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

fn normalized_points(a: ScreenPoint, b: ScreenPoint) -> (ScreenPoint, ScreenPoint) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Hover hit-test: if the mouse cursor is currently sitting on the
/// pin-diamond cell of a session row, return that row's info. Returns
/// `None` on terminals that don't forward motion events (Terminal.app),
/// since `app.mouse_pos` stays at the last click/scroll position there.
fn hovered_diamond(app: &App) -> Option<(u16, u16, &SessionSummary)> {
    let (mx, my) = app.mouse_pos?;
    let list_area = app.layout.list_area?;
    if mx <= list_area.x
        || mx + 1 >= list_area.x + list_area.width
        || my <= list_area.y
        || my + 1 >= list_area.y + list_area.height
    {
        return None;
    }
    let row = (my - list_area.y - 1) as usize;
    let items = app.list_items();
    let item = items.into_iter().nth(row)?;
    let (summary, indented, has_children) = match item {
        AppListItem::Session {
            summary,
            indented,
            has_children,
            ..
        } => (summary, indented, has_children),
        _ => return None,
    };
    let indent = crate::app::list_session_indent_cells(&summary, indented, has_children);
    // Hit zone is the 4-cell gutter to the left of the session name, after
    // the disclosure column when this row has one:
    //   [disclosure][diamond][ ][status-circle][ ]   ← then the name starts
    // Wider than the bare diamond glyph so it's easier to click —
    // the visual overlay still anchors on the diamond cell itself.
    let zone_start = list_area.x + 1 + indent + u16::from(has_children);
    let zone_end = zone_start + 4; // exclusive
    if mx < zone_start || mx >= zone_end {
        return None;
    }
    // Walk the live summary list so the caller sees up-to-date `pinned`
    // state (the materialized item from list_items() is a snapshot).
    let s = app.sessions.iter().find(|s| s.id == summary.id)?;
    Some((zone_start, my, s))
}

/// Hit zone for the `+` button on the session-list pane's title
/// (`" + sessions "`). Returns `(x_start, x_end_exclusive, y)`.
/// Anchored after the top-left border corner — cells `list.x + 1`
/// and `list.x + 2` cover `[ ][+]` for a forgiving click target.
pub fn list_plus_button_range(list_area: Rect) -> Option<(u16, u16, u16)> {
    if list_area.width < 4 {
        return None;
    }
    Some((list_area.x + 1, list_area.x + 3, list_area.y))
}

/// Hit zone for the right-aligned `−` button that collapses the
/// session list. Returns `(x_start, x_end_exclusive, y)`. Sits one
/// cell inset from the right corner so the corner glyph stays
/// visible.
pub fn list_collapse_button_range(list_area: Rect) -> Option<(u16, u16, u16)> {
    if list_area.width < 5 {
        return None;
    }
    let close_w: u16 = 3;
    let x_start = list_area.x + list_area.width.saturating_sub(close_w + 1);
    let x_end = list_area.x + list_area.width.saturating_sub(1);
    Some((x_start, x_end, list_area.y))
}

/// Hit zone for the Matrix-rain panel close button.
pub fn matrix_rain_close_button_range(rain_area: Rect) -> Option<(u16, u16, u16)> {
    if rain_area.width < 8 || rain_area.height < 4 {
        return None;
    }
    let x_start = rain_area.x + rain_area.width.saturating_sub(4);
    let x_end = rain_area.x + rain_area.width.saturating_sub(1);
    Some((x_start, x_end, rain_area.y))
}

/// Cell where the `›` uncollapse glyph is painted on the main
/// view's left border when the session list is collapsed. Anchored
/// to the top-left corner so the affordance reads as the "header"
/// of the would-be list pane. Returns `(x, y)`.
pub fn view_uncollapse_glyph_pos(view_area: Rect) -> (u16, u16) {
    (view_area.x, view_area.y)
}

/// True when `(col, row)` lies on the main view's left border AND
/// the list is collapsed — the entire left border column acts as
/// the uncollapse hit zone, so clicks are forgiving.
pub fn is_on_view_uncollapse_handle(app: &super::app::App, col: u16, row: u16) -> bool {
    if !(app.list_collapsed && app.focus != crate::app::PaneFocus::List) {
        return false;
    }
    let Some(view) = app.layout.view_area else {
        return false;
    };
    col == view.x && row >= view.y && row < view.y + view.height
}

/// Float a small one-line tooltip with `label` (padded with single
/// spaces) anchored near the cell `(anchor_x, anchor_y)`. Default
/// placement: just to the right of the anchor, vertically centered
/// on the anchor row; falls back inward when the tooltip would
/// overflow the screen edges. Mirrors the layout used by
/// `render_diamond_tooltip` / `render_view_close_tooltip` so all
/// tooltips look uniform.
fn render_button_tooltip(f: &mut Frame, theme: &Theme, label: &str, anchor_x: u16, anchor_y: u16) {
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2;
    let h: u16 = 3;
    let mut tx = anchor_x.saturating_add(2);
    let mut ty = anchor_y.saturating_sub(1);
    if tx + w > total.x + total.width {
        tx = total.x + total.width.saturating_sub(w);
    }
    if ty + h > total.y + total.height {
        ty = total.y + total.height.saturating_sub(h);
    }
    let rect = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(theme.text));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn render_list_title_button_tooltips(f: &mut Frame, app: &App) {
    let Some(list) = app.layout.list_area else {
        return;
    };
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    // Only when expanded — collapsed list has no `+` / `−`.
    if app.list_collapsed && app.focus != PaneFocus::List {
        return;
    }
    if let Some((xs, xe, y)) = list_plus_button_range(list) {
        if my == y && mx >= xs && mx < xe {
            render_button_tooltip(f, &app.theme, " New session ", xs, y);
            return;
        }
    }
    if let Some((xs, xe, y)) = list_collapse_button_range(list) {
        if my == y && mx >= xs && mx < xe {
            render_button_tooltip(f, &app.theme, " Collapse list ", xs, y);
            return;
        }
    }
    if !app.matrix_rain_hidden {
        if let Some(rain) = app.layout.matrix_rain_area {
            if let Some((xs, xe, y)) = matrix_rain_close_button_range(rain) {
                if my == y && mx >= xs && mx < xe {
                    render_button_tooltip(f, &app.theme, " Hide matrix ", xs, y);
                }
            }
        }
    }
}

fn render_view_uncollapse_tooltip(f: &mut Frame, app: &App) {
    if !(app.list_collapsed && app.focus != PaneFocus::List) {
        return;
    }
    let Some(view) = app.layout.view_area else {
        return;
    };
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    if mx == view.x && my >= view.y && my < view.y + view.height {
        let (gx, gy) = view_uncollapse_glyph_pos(view);
        render_button_tooltip(f, &app.theme, " Expand list ", gx, gy);
    }
}

fn render_view_uncollapse_glyph(f: &mut Frame, app: &App, view_area: Rect) {
    let (gx, gy) = view_uncollapse_glyph_pos(view_area);
    let style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    f.buffer_mut().set_string(gx, gy, "›", style);
}

/// Top-row close-button geometry for the session view's right edge.
/// Returns `(x_start, x_end_exclusive, y)`. Same 3-cell shape the pin
/// strip uses (` x `), one column inset from the right corner.
pub fn view_close_button_range(view_area: Rect) -> (u16, u16, u16) {
    let close_w: u16 = 3;
    let x_start = view_area.x + view_area.width.saturating_sub(close_w + 1);
    let x_end = view_area.x + view_area.width.saturating_sub(1);
    (x_start, x_end, view_area.y)
}

pub fn dynamic_ui_trigger_range(
    view_area: Rect,
    close_shown: bool,
    label_width: u16,
    reserved_right_width: u16,
) -> (u16, u16, u16) {
    let right = view_area
        .x
        .saturating_add(view_area.width)
        .saturating_sub(if close_shown { 4 } else { 1 })
        .saturating_sub(reserved_right_width);
    (right.saturating_sub(label_width), right, view_area.y)
}

fn hovered_view_close_button(app: &App, view_area: Rect) -> bool {
    let Some((mx, my)) = app.mouse_pos else {
        return false;
    };
    let (x_start, x_end, y) = view_close_button_range(view_area);
    my == y && mx >= x_start && mx < x_end
}

/// Hit zone for the pin-tile unpin diamond: 4 cells on the top
/// border, starting after the corner. Title shape is ` ★ <status>
/// <label> <harness> `, so cells `tile.x + 1 ..= tile.x + 4`
/// (inclusive) cover `[ ][★][ ][status]` — the same 4-cell zone
/// idiom as the list-view diamond. Returns `(diamond_x,
/// tile_top_y)` so the tooltip can anchor on the diamond cell.
fn pin_tile_diamond_zone(tile: Rect) -> (u16, u16) {
    (tile.x + 1, tile.x + 5)
}

fn hovered_pin_diamond<'a>(
    app: &'a App,
    pinned_ids: &[String],
) -> Option<(u16, u16, &'a SessionSummary)> {
    let (mx, my) = app.mouse_pos?;
    let strip = app.layout.pin_strip_area?;
    if pinned_ids.is_empty() {
        return None;
    }
    let tiles = pin_tile_layout(strip, pinned_ids.len());
    for (tile, id) in tiles.iter().zip(pinned_ids.iter()) {
        let (zone_start, zone_end) = pin_tile_diamond_zone(*tile);
        if my == tile.y && mx >= zone_start && mx < zone_end {
            // Diamond glyph itself sits at offset +1 in the title
            // (after the leading space).
            let diamond_x = tile.x + 2;
            let summary = app.sessions.iter().find(|s| &s.id == id)?;
            return Some((diamond_x, tile.y, summary));
        }
    }
    None
}

fn render_pin_diamond_tooltip(f: &mut Frame, app: &App, pinned_ids: &[String]) {
    let Some((dx, dy, _summary)) = hovered_pin_diamond(app, pinned_ids) else {
        return;
    };

    // Overlay the diamond cell in red+bold — same "about to unpin"
    // affordance the list-view diamond uses for pinned rows.
    let overlay_style = Style::default()
        .fg(app.theme.danger)
        .add_modifier(Modifier::BOLD);
    f.buffer_mut().set_string(dx, dy, "★", overlay_style);

    let label = " Unpin session ";
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2; // borders
    let h: u16 = 3;
    // Default: place tooltip just right of the diamond, vertically
    // centered on the row. Fall back leftward / upward if it would
    // overflow the screen. Mirrors `render_diamond_tooltip`.
    let mut tx = dx + 2;
    let mut ty = dy.saturating_sub(1);
    if tx + w > total.x + total.width {
        tx = total.x + total.width.saturating_sub(w);
    }
    if ty + h > total.y + total.height {
        ty = total.y + total.height.saturating_sub(h);
    }
    let rect = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(app.theme.text));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn render_view_close_tooltip(f: &mut Frame, app: &App) {
    let Some(view_area) = app.layout.view_area else {
        return;
    };
    if !hovered_view_close_button(app, view_area) {
        return;
    }
    let (cx, _, cy) = view_close_button_range(view_area);
    let label = " Close session ";
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2;
    let h: u16 = 3;
    let mut tx = cx.saturating_sub(w.saturating_sub(3));
    let mut ty = cy + 1;
    if tx + w > total.x + total.width {
        tx = total.x + total.width.saturating_sub(w);
    }
    if ty + h > total.y + total.height {
        ty = total.y + total.height.saturating_sub(h);
    }
    let rect = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(app.theme.text));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn render_browser_preview_close_tooltip(f: &mut Frame, app: &App) {
    let Some((x_start, x_end, y)) = app.layout.browser_preview_close else {
        return;
    };
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    if my == y && mx >= x_start && mx < x_end {
        render_button_tooltip(f, &app.theme, " Close preview ", x_start, y);
    }
}

/// Tooltip that appears when the cursor is hovering an
/// **unavailable** harness name in the picker — explains why the
/// click did nothing. Available harnesses don't get one; the
/// underline + click-submit affordance is self-explanatory.
fn render_harness_unavailable_tooltip(f: &mut Frame, app: &App) {
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    let hits = &app.layout.minibuffer_harness_hits;
    let hit = hits
        .iter()
        .find(|h| h.y == my && mx >= h.x_start && mx < h.x_end && !h.available);
    let Some(hit) = hit else { return };
    let label = format!(" {} — not installed ", hit.name);
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label.as_str()) as u16;
    let w = inner_w + 2;
    let h: u16 = 3;
    // Place above the picker row (room there since the minibuffer
    // is at the bottom of the screen).
    let mut tx = hit.x_start;
    if tx + w > total.x + total.width {
        tx = total.x + total.width.saturating_sub(w);
    }
    let ty = hit.y.saturating_sub(h);
    let rect = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(app.theme.text));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

/// Render the new-session harness picker with each name as a
/// clickable span. Records per-name column ranges in
/// `app.layout.minibuffer_harness_hits` so the click handler can
/// submit the picked name without the user having to type it.
fn render_harness_picker(f: &mut Frame, area: Rect, app: &mut App, mb: &Minibuffer) {
    // Show every registered harness plus the synthetic `project` op.
    // Unavailable harnesses (binary not on PATH) render dimmed and
    // strike-through; clicking them no-ops + drops a status note;
    // hover surfaces a "not installed" tooltip.
    let mut entries: Vec<(String, bool)> = app
        .harnesses
        .iter()
        .map(|h| (h.name.clone(), h.available))
        .collect();
    entries.push(("project".to_string(), true));

    let (hovered_x, hovered_y) = app.mouse_pos.unwrap_or((u16::MAX, u16::MAX));
    let base_available = Style::default()
        .fg(app.theme.info)
        .add_modifier(Modifier::UNDERLINED);
    let hover_available = Style::default()
        .fg(app.theme.text)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let base_disabled = Style::default()
        .fg(app.theme.dim)
        .add_modifier(Modifier::CROSSED_OUT);
    let hover_disabled = Style::default()
        .fg(app.theme.danger)
        .add_modifier(Modifier::CROSSED_OUT | Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(entries.len() * 2 + 8);
    let mut col: u16 = area.x;

    let push_raw = |spans: &mut Vec<Span<'static>>, col: &mut u16, s: &str| {
        *col += UnicodeWidthStr::width(s) as u16;
        spans.push(Span::raw(s.to_string()));
    };

    push_raw(&mut spans, &mut col, "New [");
    for (i, (name, available)) in entries.iter().enumerate() {
        if i > 0 {
            push_raw(&mut spans, &mut col, "|");
        }
        let w = UnicodeWidthStr::width(name.as_str()) as u16;
        let x_start = col;
        let x_end = col + w;
        let hovered = hovered_y == area.y && hovered_x >= x_start && hovered_x < x_end;
        let style = match (*available, hovered) {
            (true, true) => hover_available,
            (true, false) => base_available,
            (false, true) => hover_disabled,
            (false, false) => base_disabled,
        };
        spans.push(Span::styled(name.clone(), style));
        app.layout.minibuffer_harness_hits.push(HarnessHit {
            name: name.clone(),
            x_start,
            x_end,
            y: area.y,
            available: *available,
        });
        col = x_end;
    }
    push_raw(&mut spans, &mut col, "] ");
    // Hint suffix — kept short so the prompt fits in a typical
    // terminal width even with several adapters available.
    push_raw(&mut spans, &mut col, "(Tab completes, click to pick): ");
    let input_x = col;
    spans.push(Span::raw(mb.input.clone()));
    if let Some(err) = &mb.error {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            err.clone(),
            Style::default().fg(app.theme.danger),
        ));
    }
    let para = Paragraph::new(Line::from(spans));
    f.render_widget(para, area);
    // Cursor on the input — same shape as the default minibuffer
    // render uses.
    let cursor_x = input_x + mb.cursor as u16;
    f.set_cursor_position(Position {
        x: cursor_x,
        y: area.y,
    });
}

fn render_diamond_tooltip(f: &mut Frame, app: &App) {
    let Some((dx, dy, summary)) = hovered_diamond(app) else {
        return;
    };

    // Shadow / highlight diamond on the hover cell. Pinned → dimmed
    // red (about to remove); unpinned → faint yellow (preview pin).
    let overlay_style = if summary.pinned {
        Style::default()
            .fg(app.theme.danger)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(app.theme.warning)
            .add_modifier(Modifier::DIM)
    };
    f.buffer_mut().set_string(dx, dy, "★", overlay_style);

    let label = if summary.pinned {
        " Unpin session "
    } else {
        " Pin session "
    };
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2; // borders
    let h: u16 = 3;
    // Default: place tooltip just right of the diamond, vertically
    // centered on the row. Fall back leftward / upward if it would
    // overflow the screen.
    let mut tx = dx + 2;
    let mut ty = dy.saturating_sub(1);
    if tx + w > total.x + total.width {
        tx = total.x + total.width.saturating_sub(w);
    }
    if ty + h > total.y + total.height {
        ty = total.y + total.height.saturating_sub(h);
    }
    let rect = Rect {
        x: tx,
        y: ty,
        width: w,
        height: h,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(app.theme.text));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn pin_strip_height(total_h: u16) -> u16 {
    (total_h / 3).clamp(7, 18)
}

pub fn matrix_rain_panel_height(preferred: Option<u16>, available_h: u16) -> u16 {
    if available_h < crate::app::MATRIX_RAIN_H_MIN {
        return available_h;
    }
    preferred
        .unwrap_or(crate::app::MATRIX_RAIN_H_DEFAULT)
        .clamp(crate::app::MATRIX_RAIN_H_MIN, available_h)
}

/// Zoom layout: the session view takes the entire screen except for the
/// minibuffer line at the bottom. No list, no pin strip, no modeline, no
/// borders — edge-to-edge so the underlying TUI (vim / claude / htop /
/// whatever is running) gets the most real estate possible. Matches
/// tmux's `prefix z` zoomed-pane behavior.
fn render_zoomed_view(f: &mut Frame, area: Rect, app: &mut App) {
    let footer_h = compute_minibuffer_height(app, area.height);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(footer_h)])
        .split(area);
    let main_area = vertical[0];
    let minibuffer_area = vertical[1];

    app.terminal_pane_size = (main_area.width, main_area.height);
    // Items model rebuilds parsers per-frame at the current size —
    // nothing to pre-size here.
    // Zoomed layout snapshot: only the view + minibuffer exist.
    app.layout.list_area = None;
    app.layout.view_area = Some(main_area);
    app.layout.pin_strip_area = None;
    app.layout.matrix_rain_area = None;
    app.layout.minibuffer_area = Some(minibuffer_area);
    app.layout.modal_area = None;
    app.layout.list_items_area = None;
    app.layout.list_scroll_offset = 0;

    if let Some(diff) = &app.last_diff {
        let para = Paragraph::new(diff.clone()).wrap(Wrap { trim: false });
        f.render_widget(para, main_area);
    } else {
        match app.view {
            ViewMode::Terminal => render_terminal(f, main_area, app),
            ViewMode::Transcript => render_transcript(f, main_area, app),
        }
    }
    render_minibuffer(f, minibuffer_area, app);
    if app.help_visible {
        app.layout.modal_area = Some(render_help(f, area, &app.theme));
    }
}

/// Zoom-list layout: the session list fills the screen above the
/// minibuffer line. `C-x o` from here flips to the view-zoom layout
/// for the selected session, matching tmux's pane-cycling feel.
fn render_zoomed_list(f: &mut Frame, area: Rect, app: &mut App) {
    let footer_h = compute_minibuffer_height(app, area.height);
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(footer_h)])
        .split(area);
    let main_area = vertical[0];
    let minibuffer_area = vertical[1];

    // Zoomed-list layout snapshot: only the list + minibuffer exist.
    app.layout.list_area = Some(main_area);
    app.layout.view_area = None;
    app.layout.pin_strip_area = None;
    app.layout.matrix_rain_area = None;
    app.layout.minibuffer_area = Some(minibuffer_area);
    app.layout.modal_area = None;
    app.layout.list_row_count = app.list_items().len();
    app.layout.list_items_area = None;
    app.layout.list_scroll_offset = 0;

    render_sessions(f, main_area, app);
    render_minibuffer(f, minibuffer_area, app);
    if app.help_visible {
        app.layout.modal_area = Some(render_help(f, area, &app.theme));
    }
}

fn render_sessions(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == PaneFocus::List;
    // Collapsed render path: a thin column with a `>` expand glyph
    // on the top border. Anywhere inside the pane click-expands.
    let effective_collapsed = app.list_collapsed && !focused;
    if effective_collapsed {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(&app.theme, focused))
            .title(Line::from(Span::styled(
                "›",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
        f.render_widget(block, area);
        clear_pane_side_borders(f, area, app);
        return;
    }
    // Expanded render path: title is ` + sessions ` with a
    // right-aligned ` − ` for collapse. Both are clickable; the
    // click handler in `App::click_list` consults
    // `list_title_button_hit` for the geometry.
    let plus_style = Style::default()
        .fg(app.theme.accent)
        .add_modifier(Modifier::BOLD);
    let title_line = Line::from(vec![
        Span::raw(" "),
        Span::styled("+", plus_style),
        Span::raw(" sessions "),
    ]);
    let minus_hovered = match app.mouse_pos {
        Some((mx, my)) => list_collapse_button_range(area)
            .map(|(xs, xe, y)| my == y && mx >= xs && mx < xe)
            .unwrap_or(false),
        None => false,
    };
    let minus_style = if minus_hovered {
        Style::default()
            .fg(app.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.muted)
    };
    let collapse_line =
        Line::from(Span::styled(" − ", minus_style)).alignment(ratatui::layout::Alignment::Right);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(&app.theme, focused))
        .title(title_line)
        .title(collapse_line);
    let inner = block.inner(area);

    // Total cells available inside the bordered pane.
    let row_w = (area.width as usize).saturating_sub(2);
    let app_items = app.list_items();
    let mut selected_idx: Option<usize> = None;
    let items: Vec<ListItem> = app_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let is_selected = item.matches(&app.selection);
            if is_selected {
                selected_idx = Some(i);
            }
            match item {
                AppListItem::Session {
                    summary: s,
                    indented,
                    has_children,
                    children_expanded,
                } => {
                    let expand_glyph = if *has_children {
                        Some(if *children_expanded { "▼" } else { "▶" })
                    } else {
                        None
                    };
                    let pin_glyph = if s.pinned { "★" } else { " " };
                    let indent_prefix = " ".repeat(crate::app::list_session_indent_cells(
                        s,
                        *indented,
                        *has_children,
                    ) as usize);
                    // Fixed-width left side: indent + optional disclosure (1)
                    // + pin (1) + " glyph " (3).
                    let prefix_w =
                        indent_prefix.chars().count() + usize::from(expand_glyph.is_some()) + 1 + 3;
                    let harness = harness_label(s);
                    let harness_w = harness.chars().count();
                    // Always leave at least one cell of gap between the name
                    // and the right-aligned harness.
                    let name_avail = row_w.saturating_sub(prefix_w + 1 + harness_w);
                    let raw_name = primary_label(s);
                    let scroll = if is_selected && focused {
                        // ~6 chars/sec (was 5; +20% per user feedback).
                        Some((app.start_instant.elapsed().as_millis() / 167) as usize)
                    } else {
                        None
                    };
                    let name_display = fit_name(&raw_name, name_avail, scroll);
                    let name_display_w = name_display.chars().count();
                    let gap = row_w.saturating_sub(prefix_w + name_display_w + harness_w);
                    let gap_str: String = " ".repeat(gap);
                    let mut spans = vec![Span::raw(indent_prefix.to_string())];
                    if let Some(expand_glyph) = expand_glyph {
                        spans.push(Span::styled(
                            expand_glyph.to_string(),
                            Style::default().fg(app.theme.group),
                        ));
                    }
                    spans.extend([
                        Span::styled(pin_glyph.to_string(), Style::default().fg(app.theme.info)),
                        Span::styled(
                            format!(" {} ", session_status_glyph(app, s)),
                            state_style(&app.theme, s.state),
                        ),
                        Span::styled(name_display, Style::default().fg(app.theme.text)),
                        Span::raw(gap_str),
                        Span::styled(harness, harness_style(&app.theme)),
                    ]);
                    ListItem::new(Line::from(spans))
                }
                AppListItem::GroupHeader {
                    group,
                    member_count,
                } => {
                    let glyph = if group.collapsed { "▶" } else { "▼" };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(app.theme.group)),
                        Span::styled(group.name.clone(), group_name_style(&app.theme)),
                        Span::raw("  "),
                        Span::styled(
                            format!("({member_count})"),
                            Style::default().fg(app.theme.dim),
                        ),
                    ]))
                }
            }
        })
        .collect();

    let highlight_style = if focused {
        Style::default()
            .bg(app.theme.highlight_bg)
            .fg(app.theme.highlight_fg)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .bg(app.theme.inactive_highlight_bg)
            .fg(app.theme.text)
    };
    // Split the bordered pane's inner area into a top "list items"
    // region and a bottom "matrix rain" region so the rain panel
    // stays anchored at the bottom even when the session list grows
    // beyond the visible height. The list rows are sliced by
    // `app.list_scroll_offset` before rendering so mouse-wheel scrolling
    // can move the list viewport independently from the current selection.
    let (list_items_area, matrix_area) =
        split_list_pane(inner, app.matrix_rain_hidden, app.matrix_rain_h);
    let max_scroll = app_items
        .len()
        .saturating_sub(list_items_area.height as usize);
    app.list_scroll_offset = app.list_scroll_offset.min(max_scroll);
    let visible_start = app.list_scroll_offset;
    let visible_end = visible_start
        .saturating_add(list_items_area.height as usize)
        .min(items.len());
    let selected_visible_idx = selected_idx.and_then(|idx| {
        if idx >= visible_start && idx < visible_end {
            Some(idx - visible_start)
        } else {
            None
        }
    });
    let items: Vec<ListItem> = items
        .into_iter()
        .skip(visible_start)
        .take(list_items_area.height as usize)
        .collect();
    let mut state = ListState::default();
    state.select(if matches!(app.selection, Selection::None) {
        None
    } else {
        selected_visible_idx
    });
    f.render_widget(block, area);
    let list = List::new(items).highlight_style(highlight_style);
    f.render_stateful_widget(list, list_items_area, &mut state);
    app.layout.list_items_area = Some(list_items_area);
    app.layout.list_scroll_offset = visible_start + state.offset();
    app.list_scroll_offset = app.layout.list_scroll_offset;
    clear_pane_side_borders(f, area, app);
    render_matrix_rain(f, matrix_area, app);
}

/// Split the list pane's inner area (the rect inside the borders)
/// into a top region for session rows and a bottom region for the
/// matrix-rain panel.
///
/// The matrix panel is "sticky": it always claims its preferred
/// height at the bottom whenever there is room. The list shrinks to
/// the remaining rows and scrolls when items overflow. Below
/// `SESSION_LIST_H_MIN + MATRIX_RAIN_H_MIN` of total inner height
/// (or when the user hid the rain), the list takes the entire pane
/// and the rain area is reported as zero-height — i.e., the rain
/// effectively goes "out of view" when the terminal is too short.
fn split_list_pane(
    inner: Rect,
    matrix_rain_hidden: bool,
    matrix_rain_preferred_h: Option<u16>,
) -> (Rect, Rect) {
    let empty_matrix = Rect {
        x: inner.x,
        y: inner.y.saturating_add(inner.height),
        width: inner.width,
        height: 0,
    };
    if matrix_rain_hidden {
        return (inner, empty_matrix);
    }
    let max_matrix_h = inner.height.saturating_sub(crate::app::SESSION_LIST_H_MIN);
    if max_matrix_h < crate::app::MATRIX_RAIN_H_MIN {
        return (inner, empty_matrix);
    }
    let matrix_h = matrix_rain_panel_height(matrix_rain_preferred_h, max_matrix_h);
    if matrix_h == 0 {
        return (inner, empty_matrix);
    }
    let list_h = inner.height.saturating_sub(matrix_h);
    let list = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: list_h,
    };
    let matrix = Rect {
        x: inner.x,
        y: inner.y.saturating_add(list_h),
        width: inner.width,
        height: matrix_h,
    };
    (list, matrix)
}

fn render_matrix_rain(f: &mut Frame, rain_area: Rect, app: &mut App) {
    app.layout.matrix_rain_area = None;
    // Reset hover/click targets every frame, including the early-return
    // paths below — otherwise a hidden/too-small panel would leave stale
    // hits from a prior frame clickable.
    app.matrix_reveal_hits.clear();
    if app.matrix_rain_hidden {
        return;
    }
    if rain_area.width < 8 || rain_area.height < 4 {
        return;
    }
    app.layout.matrix_rain_area = Some(rain_area);
    render_matrix_rain_header(f, rain_area, &app.theme);
    let rain_area = Rect {
        x: rain_area.x,
        y: rain_area.y + 1,
        width: rain_area.width,
        height: rain_area.height.saturating_sub(1),
    };
    if rain_area.height < 3 {
        return;
    }

    let now = Instant::now();

    // Wallpaper: paint the most recent browser preview from ANY session
    // (cross-session — the matrix rain is a fleet visualization, so the
    // backdrop reflects the whole fleet, not just the focused session,
    // unlike the per-session terminal overlay) dimmed and cropped-to-fill
    // as a backdrop. The rain loop below draws over it: cells with a
    // drop/letter overwrite the image, empty cells keep it, so the
    // animation runs uninterrupted on top of the wallpaper.
    //
    // Dial-up nostalgia: the image draws in top-to-bottom when the
    // preview arrives and erases top-to-bottom when it's about to hide,
    // like a JPEG over a slow modem. The matrix tick (~8fps) already
    // redraws each frame, so the animation advances on its own.
    let wallpaper = app
        .browser_previews
        .values()
        .max_by_key(|state| state.revealed_at)
        .and_then(|state| {
            state.decoded.clone().map(|img| {
                (
                    img,
                    state.revealed_at,
                    state.hide_after,
                    state.hover_started.is_some(),
                )
            })
        });
    if let Some((img, revealed_at, hide_after, hovered)) = &wallpaper {
        let row_frac = preview_reveal_range(*revealed_at, *hide_after, now, *hovered);
        if row_frac.1 > row_frac.0 {
            // 2 sub-pixels per cell in each axis for quadrant rendering:
            // `oh` is already 2*rows (half-cell tall), so only the width
            // doubles here.
            let (ow, oh) = blit_scale_dims(img.dimensions(), rain_area, true);
            let resized = resized_image(&mut app.image_resize_cache, img, ow * 2, oh);
            paint_resized_quadrants(f, rain_area, &resized, MATRIX_WALLPAPER_DIM, row_frac);
        }
    }

    let activity = update_matrix_rain_intensity(app, now);
    let elapsed = app.start_instant.elapsed().as_millis() as u64;
    let cycle = rain_area.height + MATRIX_RAIN_TAIL_MAX + 1;
    let charset = b"01:|/\\{}[]<>+$#@*=-zrvshcodxgit";
    let mut current_drop_keys = HashSet::with_capacity(rain_area.width as usize);
    // Per-column current head position for active foreground drops —
    // captured here so the horizontal reveal pass can pin letters
    // live the instant a drop's head reaches the letter's row.
    let mut drop_heads: Vec<Option<i16>> = vec![None; rain_area.width as usize];

    // Resolve any not-yet-placed vertical reveals, then build a
    // per-column row→letter overlay. The column loop below uses this
    // to swap the random drop-body glyph for the word's letter where
    // they line up, so a vertical reveal looks like a normal drop
    // that happens to be spelling a word as it falls.
    resolve_vertical_reveal_positions(&mut app.matrix_rain, rain_area, now);
    let vertical_overlay = build_vertical_letter_overlay(&app.matrix_rain, rain_area, now);

    for col in 0..rain_area.width {
        let seed = hash64(col as u64 ^ ((rain_area.width as u64) << 24));
        let speed = 2 + (seed % 7);
        let frame =
            foreground_rain_frame(now, app.matrix_rain_foreground_epoch, seed, speed, cycle);
        current_drop_keys.insert(frame.key);
        // Register a fresh drop only at the *top* of its cycle, and
        // only if a per-cycle random roll comes in under the current
        // activity. The roll is keyed on `frame.key` (which already
        // includes the column seed + cycle index), so it's STABLE
        // within a single cycle — no flicker — but scatters across
        // cycles and columns. Every column gets a chance every
        // cycle, instead of the old fixed per-column threshold that
        // left some columns permanently dark at low intensity.
        if frame.head <= MATRIX_RAIN_REGISTRATION_TOP_ROW as i16 {
            let roll = unit_f32(hash64(frame.key ^ 0xc3d2_e1f0_a574_8b96));
            if roll < activity {
                app.matrix_rain_active_drops
                    .entry(frame.key)
                    .or_insert_with(|| matrix_rain_tail_for_key(frame.key));
            }
        }
        let active = app
            .matrix_rain_active_drops
            .get(&frame.key)
            .copied()
            .map(|tail| (frame.head, tail));
        if active.is_some() {
            drop_heads[col as usize] = Some(frame.head);
        }
        let col_overlay = vertical_overlay.get(col as usize);
        for row in 0..rain_area.height {
            let dist = active.map(|(head, _)| head).unwrap_or(-1) - row as i16;
            let mut style = None;
            let mut in_drop_body = false;
            if let Some((_, tail)) = active {
                if dist >= 0 && dist < tail as i16 {
                    let shade = 1.0 - (dist as f32 / tail.max(1) as f32);
                    style = Some(rain_style(&app.theme, shade, activity));
                    in_drop_body = true;
                }
            }
            if style.is_none() {
                let sparkle = hash64(seed ^ row as u64 ^ (elapsed / 260));
                let faint_threshold = (2.0 + activity * 3.0).round() as u64;
                if sparkle % 100 < faint_threshold {
                    style = Some(Style::default().fg(app.theme.matrix_dim));
                }
            }
            if let Some(style) = style {
                // Vertical reveals override the random drop-body
                // glyph with the word's actual letter at this row,
                // and shift the cell's color from the default rain
                // green toward `theme.accent_alt` (teal). Same
                // head→tail shading, no bold — distinct enough to
                // pick the word out, still firmly in the matrix
                // palette.
                let (ch, style) = if in_drop_body {
                    match col_overlay.and_then(|map| map.get(&(row as i16)).copied()) {
                        Some(letter) => {
                            // The frame the drop *head* is exactly
                            // on the letter cell, flash it to the
                            // brightest matrix-flash green — that's
                            // the "moment of impact" the eye latches
                            // onto. As the head moves on, the cell
                            // falls back to the slow-fade letter
                            // style (≈ 2× slower than the random
                            // tail chars around it).
                            let dist_from_head =
                                active.map(|(h, _)| h - row as i16).unwrap_or(i16::MAX);
                            if dist_from_head == 0 {
                                (letter, rain_head_flash_style(&app.theme))
                            } else {
                                let raw_shade = compute_drop_shade(active, row);
                                let shade = 0.5 + raw_shade * 0.5;
                                (letter, rain_letter_style(&app.theme, shade, activity))
                            }
                        }
                        None => {
                            let glyph_seed = hash64(seed ^ row as u64 ^ (elapsed / 180));
                            (
                                charset[(glyph_seed as usize) % charset.len()] as char,
                                style,
                            )
                        }
                    }
                } else {
                    let glyph_seed = hash64(seed ^ row as u64 ^ (elapsed / 180));
                    (
                        charset[(glyph_seed as usize) % charset.len()] as char,
                        style,
                    )
                };
                f.buffer_mut().set_string(
                    rain_area.x + col,
                    rain_area.y + row,
                    ch.to_string(),
                    style,
                );
            }
        }
    }
    app.matrix_rain_active_drops
        .retain(|key, _| current_drop_keys.contains(key));

    let theme = app.theme.clone();
    let mut hits: Vec<crate::app::MatrixRevealHit> = Vec::new();
    for reveal in app.matrix_rain.active_reveals_mut(now) {
        if let crate::matrix_rain::RevealOrientation::Horizontal = reveal.orientation {
            if let Some(hit) =
                render_matrix_reveal_horizontal(f, rain_area, &theme, reveal, elapsed, &drop_heads)
            {
                hits.push(hit);
            }
        }
        // Vertical reveals are rendered inline above as a drop-body
        // letter overlay — no separate pass / no pin-and-hold.
    }
    app.matrix_reveal_hits = hits;
    // Hover tooltip: if the cursor is over a horizontal word, name the
    // session it came from. Drawn last so it sits on top of the rain.
    render_matrix_reveal_tooltip(f, rain_area, app);
}

/// If the mouse is hovering a matrix-rain horizontal reveal word, draw a
/// one-line tooltip on an adjacent row naming the source session.
fn render_matrix_reveal_tooltip(f: &mut Frame, rain_area: Rect, app: &App) {
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    let Some(hit) = app.matrix_reveal_hits.iter().find(|h| h.contains(mx, my)) else {
        return;
    };
    let label = match app.sessions.iter().find(|s| s.id == hit.session_id) {
        Some(s) => {
            let harness = harness_label(s);
            // Title if the session has a distinct one, else just the
            // harness; append harness too when a title exists so the
            // tooltip says both (e.g. "fix auth · zarvis").
            let title = s.title.as_deref().filter(|t| !t.is_empty());
            match title {
                Some(t) => format!(" {t} · {harness} "),
                None => format!(" {harness} "),
            }
        }
        None => format!(" {} · session ended ", hit.text),
    };
    let label: String = label
        .chars()
        .take(rain_area.width.saturating_sub(1) as usize)
        .collect();
    let w = label.chars().count() as u16;
    // Prefer the row above the word; fall back to below at the panel top.
    let ty = if hit.row > rain_area.y {
        hit.row - 1
    } else {
        (hit.row + 1).min(rain_area.y + rain_area.height.saturating_sub(1))
    };
    let max_x = rain_area.x + rain_area.width.saturating_sub(w);
    let tx = hit.col_start.min(max_x).max(rain_area.x);
    let area = Rect {
        x: tx,
        y: ty,
        width: w,
        height: 1,
    };
    let style = Style::default()
        .fg(app.theme.highlight_fg)
        .bg(app.theme.highlight_bg)
        .add_modifier(Modifier::BOLD);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), area);
}

/// First-frame placement for vertical reveals: pick the absolute
/// (col, row) for any reveal that doesn't have a resolved position
/// yet. Idempotent: calling on subsequent frames is a no-op because
/// `set_resolved_position` only sets when unset.
fn resolve_vertical_reveal_positions(
    matrix_rain: &mut crate::matrix_rain::MatrixRain,
    area: Rect,
    now: Instant,
) {
    if area.width == 0 || area.height < 2 {
        return;
    }
    for reveal in matrix_rain.active_reveals_mut(now) {
        if !matches!(
            reveal.orientation,
            crate::matrix_rain::RevealOrientation::Vertical
        ) {
            continue;
        }
        if reveal.resolved_position().is_some() {
            continue;
        }
        let text_len = reveal.text.chars().count() as u16;
        if text_len == 0 || text_len > area.height {
            continue;
        }
        let col_rel = ((area.width.saturating_sub(1)) as f32 * reveal.x)
            .round()
            .clamp(0.0, area.width.saturating_sub(1) as f32) as u16;
        let row_rel = ((area.height.saturating_sub(text_len)) as f32 * reveal.y)
            .round()
            .clamp(0.0, area.height.saturating_sub(text_len) as f32) as u16;
        reveal.set_resolved_position(area.x + col_rel, area.y + row_rel);
    }
}

/// Build a per-column `row → letter` map for all currently-active
/// vertical reveals. The column loop consults this when rendering
/// drop-body cells; a cell that matches an overlay entry shows the
/// word's letter instead of a random charset character.
fn build_vertical_letter_overlay(
    matrix_rain: &crate::matrix_rain::MatrixRain,
    area: Rect,
    now: Instant,
) -> Vec<HashMap<i16, char>> {
    let mut overlay = vec![HashMap::<i16, char>::new(); area.width as usize];
    for reveal in matrix_rain.active_reveals(now) {
        if !matches!(
            reveal.orientation,
            crate::matrix_rain::RevealOrientation::Vertical
        ) {
            continue;
        }
        let Some((col_abs, row_abs)) = reveal.resolved_position() else {
            continue;
        };
        if col_abs < area.x || col_abs >= area.x + area.width {
            continue;
        }
        let col_idx = (col_abs - area.x) as usize;
        let row_start_rel = row_abs.saturating_sub(area.y) as i16;
        for (i, ch) in reveal.text.chars().enumerate() {
            let row_rel = row_start_rel + i as i16;
            if row_rel < 0 || row_rel >= area.height as i16 {
                continue;
            }
            overlay[col_idx].insert(row_rel, ch);
        }
    }
    overlay
}

/// A column registers a new drop only when its `head` is at one of
/// the top few rows of the cycle (i.e. about to start falling from
/// the very top of the visible area). This avoids drops popping
/// into existence mid-screen the moment activity rises above the
/// column's threshold.
const MATRIX_RAIN_REGISTRATION_TOP_ROW: u16 = 1;

fn render_matrix_rain_header(f: &mut Frame, area: Rect, theme: &Theme) {
    let line_style = Style::default().fg(theme.matrix_line);
    let close_style = Style::default()
        .fg(theme.matrix_close)
        .add_modifier(Modifier::BOLD);
    for x in area.x..area.x + area.width {
        f.buffer_mut().set_string(x, area.y, "─", line_style);
    }
    let x = area.x + area.width.saturating_sub(3);
    f.buffer_mut().set_string(x, area.y, " x ", close_style);
}

fn update_matrix_rain_intensity(app: &mut App, now: Instant) -> f32 {
    let target = fleet_activity_target(app, now);
    let elapsed = now
        .checked_duration_since(app.matrix_rain_intensity_updated_at)
        .unwrap_or(Duration::ZERO);
    app.matrix_rain_intensity =
        eased_matrix_rain_intensity(app.matrix_rain_intensity, target, elapsed);
    app.matrix_rain_intensity_updated_at = now;
    // NOTE: `matrix_rain_foreground_epoch` is intentionally left
    // alone here. It's set once at app start and never moved — each
    // column's head is `((now - epoch) / cell_ms + phase) % cycle`,
    // so shifting the epoch would teleport every drop on screen.
    // Activity gates *which* columns register a fresh drop at the
    // top of each cycle (see `render_matrix_rain`), not the clock
    // that drops fall on.
    app.matrix_rain_intensity
}

fn eased_matrix_rain_intensity(current: f32, target: f32, elapsed: Duration) -> f32 {
    let current = current.clamp(0.0, 1.0);
    let target = target.clamp(0.0, 1.0);
    if (current - target).abs() <= f32::EPSILON {
        return target;
    }
    let ramp = if target > current {
        MATRIX_RAIN_RAMP_UP_SECS
    } else {
        MATRIX_RAIN_DECAY_SECS
    };
    let step = elapsed.as_secs_f32() / ramp;
    if target > current {
        (current + step).min(target)
    } else {
        (current - step).max(target)
    }
}

fn matrix_rain_tail_for_key(key: u64) -> u16 {
    let span = MATRIX_RAIN_TAIL_MAX - MATRIX_RAIN_TAIL_MIN + 1;
    MATRIX_RAIN_TAIL_MIN + (hash64(key ^ 0x8d12_6f43_b9e0_5c7a) % span as u64) as u16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MatrixRainFrame {
    key: u64,
    head: i16,
}

/// Where this column's foreground drop is *right now*, on a clock
/// that never moves once `foreground_epoch` is set at app start.
///
/// Each column has a fixed phase offset (from `seed`) so columns
/// don't all start at head 0 in unison. The position is purely a
/// function of wall time + seed + speed — activity / threshold do
/// not appear here, so a change in fleet intensity cannot teleport
/// drops mid-fall. Activity gating lives one level up in
/// `render_matrix_rain`, which decides whether each cycle's drop is
/// rendered at all (registered at head ≈ 0 if activity ≥ threshold).
fn foreground_rain_frame(
    now: Instant,
    foreground_epoch: Instant,
    seed: u64,
    speed: u64,
    cycle: u16,
) -> MatrixRainFrame {
    let age = now
        .checked_duration_since(foreground_epoch)
        .unwrap_or(Duration::ZERO);
    let cell_ms = 58 + speed * 19;
    let cycle = cycle.max(1) as u64;
    let step = age.as_millis() as u64 / cell_ms;
    // Stable per-column phase so columns are out of sync from frame
    // 0 — gives the staggered look without depending on threshold or
    // a shifting epoch.
    let phase = (hash64(seed ^ 0x5a17_30c8_d3e1_4f29) % cycle) as u64;
    let total = step + phase;
    let cycle_index = total / cycle;
    MatrixRainFrame {
        key: hash64(seed ^ ((speed & 0xff) << 56) ^ ((cycle & 0xffff) << 40) ^ cycle_index),
        head: (total % cycle) as i16,
    }
}

fn fleet_activity_target(app: &App, now: Instant) -> f32 {
    let mut active_count = 0u16;
    for s in app
        .sessions
        .iter()
        .filter(|s| s.kind == agentd_protocol::SessionKind::User)
    {
        let active_agent = app
            .agent_statuses
            .get(&s.id)
            .map(|status| status.active)
            .unwrap_or(false);
        let recent_live_pty = app
            .pty_activity
            .get(&s.id)
            .and_then(|at| now.checked_duration_since(*at))
            .map(|d| d.as_millis() < 900)
            .unwrap_or(false);
        if active_agent || recent_live_pty {
            active_count = active_count.saturating_add(1);
        }
    }
    rain_activity_for_active_sessions(active_count)
}

fn rain_activity_for_active_sessions(active_count: u16) -> f32 {
    (active_count as f32 * 0.25).clamp(0.0, 1.0)
}

fn rain_style(theme: &Theme, shade: f32, activity: f32) -> Style {
    let shade = shade.clamp(0.0, 1.0);
    let boost = activity.clamp(0.0, 1.0);
    let style = Style::default().fg(blend_color(
        theme.matrix_dim,
        theme.accent,
        (shade * 0.86 + boost * 0.14).clamp(0.0, 1.0),
    ));
    if shade > 0.72 {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Brightest-green flash for the single frame a drop head is
/// directly over a reveal letter — applies to both vertical and
/// horizontal reveals. The cell pops out of the rain palette for
/// that one frame, then falls back to the regular slow-fade letter
/// style as the head moves on.
fn rain_head_flash_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.matrix_flash_good)
        .add_modifier(Modifier::BOLD)
}

/// Same head→tail shading as `rain_style`, but the bright endpoint
/// is `theme.text` (near-white pale green in the default theme)
/// instead of `theme.accent` (the rain's bright green). The vertical
/// reveal letter ends up *brighter* than the brightest rain head —
/// clearly readable, still in the matrix palette since the dim end
/// is the same `matrix_dim` green as the rain.
fn rain_letter_style(theme: &Theme, shade: f32, activity: f32) -> Style {
    let shade = shade.clamp(0.0, 1.0);
    let boost = activity.clamp(0.0, 1.0);
    let style = Style::default().fg(blend_color(
        theme.matrix_dim,
        theme.text,
        (shade * 0.86 + boost * 0.14).clamp(0.0, 1.0),
    ));
    if shade > 0.72 {
        style.add_modifier(Modifier::BOLD)
    } else {
        style
    }
}

/// Recompute the per-cell shade used by `rain_style` /
/// `rain_letter_style`: `1.0` at the drop head, fading linearly to
/// `0` over `tail` rows. Returns `0.0` if the cell isn't in the
/// drop body (caller should normally only call this with a body
/// cell — `0.0` is a safe fallback that maps to the dim end of the
/// palette).
fn compute_drop_shade(active: Option<(i16, u16)>, row: u16) -> f32 {
    match active {
        Some((head, tail)) => {
            let dist = head - row as i16;
            if dist < 0 || dist >= tail as i16 {
                0.0
            } else {
                1.0 - (dist as f32 / tail.max(1) as f32)
            }
        }
        None => 0.0,
    }
}

/// Render a horizontal reveal word (letters laid left-to-right at a
/// single row). Each letter pins the first frame a real foreground
/// drop's body is currently *covering* its cell — checked against
/// `drop_heads` from the rain pass, not predicted analytically. The
/// pin window is the maximum randomized drop body width; tail lengths
/// are intentionally independent from activity, and activity only
/// gates whether a fresh top-of-cycle drop is registered.
///
/// Once every letter is pinned the reveal enters a short hold and
/// then fades. If some letters never pin (their column is
/// consistently below activity threshold), the reveal simply expires
/// when its `duration` elapses.
fn render_matrix_reveal_horizontal(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    reveal: &mut crate::matrix_rain::RevealWord,
    elapsed_ms: u64,
    drop_heads: &[Option<i16>],
) -> Option<crate::app::MatrixRevealHit> {
    if area.width < 4 || area.height == 0 {
        return None;
    }
    let chars: Vec<char> = reveal.text.chars().collect();
    let text_w = chars.len() as u16;
    if text_w == 0 || text_w + 2 > area.width {
        return None;
    }
    // Lock the absolute (col, row) on the first frame so already-
    // pinned letters don't drift if the area resizes mid-reveal.
    let (target_x, target_y) = match reveal.resolved_position() {
        Some((c, r)) => (c, r),
        None => {
            let cx = area.x
                + ((area.width.saturating_sub(text_w) as f32) * reveal.x)
                    .round()
                    .clamp(0.0, area.width.saturating_sub(text_w) as f32) as u16;
            let ry = area.y
                + ((area.height.saturating_sub(1) as f32) * reveal.y)
                    .round()
                    .clamp(0.0, area.height.saturating_sub(1) as f32) as u16;
            reveal.set_resolved_position(cx, ry);
            (cx, ry)
        }
    };

    let target_rel_y = target_y.saturating_sub(area.y) as i16;
    let base_col = target_x.saturating_sub(area.x) as usize;

    let letter_count = reveal.pin_state().len();
    // Per-letter "is the drop head sitting exactly on this letter's
    // cell *right now*?" — driving the brightest-green flash one
    // letter at a time as the drop falls through each column.
    let mut head_on_letter = vec![false; letter_count];
    for i in 0..letter_count {
        let col = base_col + i;
        if let Some(Some(head)) = drop_heads.get(col) {
            let delta = *head - target_rel_y;
            // Pin only while a drop body could plausibly be
            // covering the cell. Individual drop tails are
            // randomized and not stored in `drop_heads`, so use the
            // maximum tail as a conservative window; without an
            // upper bound, a letter could latch on a head that's
            // already far past and appear with no drop nearby.
            if delta >= 0 && delta < MATRIX_RAIN_TAIL_MAX as i16 {
                reveal.pin_letter(i, elapsed_ms);
                if delta == 0 {
                    head_on_letter[i] = true;
                }
            }
        }
    }

    render_pinned_letters_at(
        f,
        theme,
        reveal,
        elapsed_ms,
        &chars,
        &head_on_letter,
        |i| target_x + i as u16,
        |_| target_y,
    );

    // The whole word span is a hover/click target — even while letters
    // are still pinning in — so the user can always reach the source
    // session. Only words tagged with a session are interactive.
    reveal.session_id().map(|sid| crate::app::MatrixRevealHit {
        col_start: target_x,
        col_end: target_x + text_w - 1,
        row: target_y,
        text: reveal.text.clone(),
        session_id: sid.to_string(),
    })
}

/// Brightness / hold / fade pipeline for horizontal reveals. Once
/// every letter is pinned the word holds briefly then fades; if
/// some letters never get pinned (their column never fires a drop
/// through `target_y`) the reveal just expires.
fn render_pinned_letters_at(
    f: &mut Frame,
    theme: &Theme,
    reveal: &crate::matrix_rain::RevealWord,
    elapsed_ms: u64,
    chars: &[char],
    head_on_letter: &[bool],
    xs: impl Fn(usize) -> u16,
    ys: impl Fn(usize) -> u16,
) {
    let pin_state = reveal.pin_state();
    let all_pinned_at = if !pin_state.is_empty() && pin_state.iter().all(Option::is_some) {
        pin_state.iter().filter_map(|x| *x).max()
    } else {
        None
    };

    let complete_hold_ms = 400;
    let fade_ms = 200;
    let (fade_start, fade_end) = match all_pinned_at {
        Some(t) => (t + complete_hold_ms, t + complete_hold_ms + fade_ms),
        None => (u64::MAX, u64::MAX),
    };
    let fade_level = if elapsed_ms < fade_start {
        1.0
    } else {
        let elapsed_fade = elapsed_ms.saturating_sub(fade_start);
        (1.0 - elapsed_fade as f32 / fade_ms.max(1) as f32).clamp(0.0, 1.0)
    };

    for (i, ch) in chars.iter().copied().enumerate() {
        let Some(pinned_at) = pin_state[i] else {
            continue;
        };
        if elapsed_ms >= fade_end {
            continue;
        }
        // Frame the drop head is sitting on the letter: brightest-
        // green flash. Beats the hold/fade brightness for that one
        // frame, then the cell goes back to the normal pinned look.
        let style = if head_on_letter.get(i).copied().unwrap_or(false) {
            rain_head_flash_style(theme)
        } else {
            let since_pin_ms = elapsed_ms.saturating_sub(pinned_at);
            let brightness = if elapsed_ms < fade_start {
                if since_pin_ms < 220 {
                    1.0
                } else {
                    0.76
                }
            } else {
                (0.12 + fade_level * 0.64).clamp(0.0, 1.0)
            };
            matrix_reveal_style(theme, brightness, elapsed_ms < fade_start)
        };
        f.buffer_mut()
            .set_string(xs(i), ys(i), ch.to_string(), style);
    }
}

fn matrix_reveal_style(theme: &Theme, brightness: f32, bold: bool) -> Style {
    let brightness = brightness.clamp(0.0, 1.0);
    let color = blend_color(theme.matrix_dim, theme.text, brightness);
    let style = Style::default().fg(color);
    if bold || brightness > 0.72 {
        style.add_modifier(Modifier::BOLD)
    } else if brightness < 0.35 {
        style.add_modifier(Modifier::DIM)
    } else {
        style
    }
}

fn blend_color(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    match (color_rgb(a), color_rgb(b)) {
        (Some((ar, ag, ab)), Some((br, bg, bb))) => {
            Color::Rgb(lerp_u8(ar, br, t), lerp_u8(ag, bg, t), lerp_u8(ab, bb, t))
        }
        _ if t >= 0.5 => b,
        _ => a,
    }
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

fn color_rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Black => Some((0, 0, 0)),
        Color::Red => Some((205, 49, 49)),
        Color::Green => Some((13, 188, 121)),
        Color::Yellow => Some((229, 229, 16)),
        Color::Blue => Some((36, 114, 200)),
        Color::Magenta => Some((188, 63, 188)),
        Color::Cyan => Some((17, 168, 205)),
        Color::Gray => Some((229, 229, 229)),
        Color::DarkGray => Some((102, 102, 102)),
        Color::LightRed => Some((241, 76, 76)),
        Color::LightGreen => Some((35, 209, 139)),
        Color::LightYellow => Some((245, 245, 67)),
        Color::LightBlue => Some((59, 142, 234)),
        Color::LightMagenta => Some((214, 112, 214)),
        Color::LightCyan => Some((41, 184, 219)),
        Color::White => Some((255, 255, 255)),
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}

fn hash_str(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn unit_f32(seed: u64) -> f32 {
    ((seed >> 11) as f64 / ((1u64 << 53) as f64)) as f32
}

fn transition_amount(started_at: Instant) -> Option<f32> {
    let elapsed = started_at.elapsed().as_millis();
    if elapsed >= crate::app::SESSION_TRANSITION_MS {
        return None;
    }
    Some(1.0 - (elapsed as f32 / crate::app::SESSION_TRANSITION_MS as f32))
}

fn glitch_style(theme: &Theme, row: u16) -> Style {
    let color = match row % 4 {
        0 => theme.matrix_flash_work,
        1 => theme.matrix_flash_good,
        2 => theme.accent,
        _ => theme.matrix_glow,
    };
    Style::default().fg(color)
}

fn render_glitch_overlay(f: &mut Frame, area: Rect, theme: &Theme, seed: u64, amount: f32) {
    if area.width == 0 || area.height == 0 || amount <= 0.0 {
        return;
    }
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64 / crate::app::SPINNER_FRAME_MS as u64)
        .unwrap_or(0);
    let density = 0.18 + 0.48 * amount;
    let charset = b"01#@%+-/\\_|";
    for row in 0..area.height {
        let row_seed = hash64(seed ^ frame.wrapping_mul(97) ^ row as u64);
        if unit_f32(row_seed) > density {
            continue;
        }
        let mut text = String::with_capacity(area.width as usize);
        let shift = (unit_f32(hash64(row_seed ^ 0x51)) * 6.0 * amount) as usize;
        for col in 0..area.width as usize {
            let cell_seed = hash64(row_seed ^ (col as u64).wrapping_mul(0x9e37));
            let noise = unit_f32(cell_seed);
            if col < shift || noise < 0.20 + 0.42 * amount {
                let idx = (hash64(cell_seed ^ 0xa11ce) as usize) % charset.len();
                text.push(charset[idx] as char);
            } else {
                text.push(' ');
            }
        }
        let style = glitch_style(theme, row);
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(text, style))),
            Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            },
        );
    }
}

fn render_main_transition(f: &mut Frame, area: Rect, app: &App, window_id: Option<u64>) {
    let Some(window_id) = window_id else {
        return;
    };
    let Some(t) = app.session_transitions.get(&window_id) else {
        return;
    };
    let Some(amount) = transition_amount(t.started_at) else {
        return;
    };
    let seed = match &app.selection {
        Selection::Session(id) => hash_str(id),
        Selection::Group(id) => hash_str(id) ^ 0x67726f7570,
        Selection::None => 0x5e5510,
    };
    render_glitch_overlay(f, area, &app.theme, seed, amount);
}

fn render_pin_transition(f: &mut Frame, area: Rect, app: &App, session_id: &str) {
    let Some(started_at) = app.pin_transitions.get(session_id).copied() else {
        return;
    };
    let Some(amount) = transition_amount(started_at) else {
        return;
    };
    render_glitch_overlay(f, area, &app.theme, hash_str(session_id) ^ 0x70696e, amount);
}

fn render_main_windows(f: &mut Frame, area: Rect, app: &mut App) {
    fn render_node(
        f: &mut Frame,
        area: Rect,
        app: &mut App,
        node: &MainWindowTree,
        next_split_id: &mut u64,
    ) {
        match node {
            MainWindowTree::Leaf { id, selection } => {
                let old_selection = app.selection.clone();
                app.selection = selection.clone();
                let inner = area.inner(Margin {
                    horizontal: 1,
                    vertical: 1,
                });
                app.window_pane_sizes
                    .insert(*id, (inner.width, inner.height));
                app.layout.main_window_areas.push(WindowPaneHit {
                    id: *id,
                    area,
                    inner_area: area.inner(Margin {
                        horizontal: 1,
                        vertical: 1,
                    }),
                });
                render_detail(f, area, app, Some(*id));
                app.selection = old_selection;
            }
            MainWindowTree::Split {
                direction,
                ratio_percent,
                first,
                second,
            } => {
                let split_id = *next_split_id;
                *next_split_id += 1;
                let first_pct = (*ratio_percent).clamp(10, 90);
                let chunks = Layout::default()
                    .direction(match direction {
                        WindowSplitDirection::Below => Direction::Vertical,
                        WindowSplitDirection::Right => Direction::Horizontal,
                    })
                    .constraints([
                        Constraint::Percentage(first_pct),
                        Constraint::Percentage(100 - first_pct),
                    ])
                    .split(area);
                let divider = match direction {
                    WindowSplitDirection::Right => Rect::new(
                        chunks[0]
                            .x
                            .saturating_add(chunks[0].width)
                            .saturating_sub(1),
                        area.y,
                        2.min(area.width),
                        area.height,
                    ),
                    WindowSplitDirection::Below => Rect::new(
                        area.x,
                        chunks[0]
                            .y
                            .saturating_add(chunks[0].height)
                            .saturating_sub(1),
                        area.width,
                        2.min(area.height),
                    ),
                };
                app.layout.main_window_dividers.push(WindowDividerHit {
                    parent: split_id,
                    direction: *direction,
                    area: divider,
                    parent_area: area,
                    ratio_percent: first_pct,
                });
                render_node(f, chunks[0], app, first, next_split_id);
                render_node(f, chunks[1], app, second, next_split_id);
            }
        }
    }
    let tree = app.main_windows.clone();
    let mut next_split_id = 1;
    render_node(f, area, app, &tree, &mut next_split_id);
}

fn render_detail(f: &mut Frame, area: Rect, app: &mut App, window_id: Option<u64>) {
    let focused =
        app.focus == PaneFocus::View && window_id.is_none_or(|id| id == app.active_window_id);
    if let Some(diff) = &app.last_diff {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(&app.theme, focused))
            .title(" diff (ESC clears; press d to refresh) ");
        let para = Paragraph::new(diff.clone())
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
        clear_pane_side_borders(f, area, app);
        return;
    }
    let summary = app.selected_session().cloned();
    let group = app.selected_group().cloned();
    // Width budgets for fitting the title onto the top border.
    // Layout: `<corner> <glyph> <label>  …  <harness>  x <corner>`.
    let total = area.width as usize;
    let close_w: usize = if summary.is_some() { 3 } else { 0 };
    let harness_w: usize = summary
        .as_ref()
        .map(|s| 2 + UnicodeWidthStr::width(harness_label(s).as_str()))
        .unwrap_or(0);
    // Label budget = total − 2 corners − right-side blocks − fixed
    // title scaffolding (` <glyph> <label> ` is 3 spaces + glyph
    // width + label).
    let glyph_w = summary
        .as_ref()
        .map(|s| UnicodeWidthStr::width(session_status_glyph(app, s)))
        .unwrap_or(0);
    let label_budget = total
        .saturating_sub(2)
        .saturating_sub(harness_w)
        .saturating_sub(close_w)
        .saturating_sub(3 + glyph_w);
    let title = match (summary.as_ref(), group.as_ref()) {
        (Some(s), _) => format!(
            " {} {} ",
            session_status_glyph(app, s),
            truncate_to_width(&primary_label(s), label_budget),
        ),
        (None, Some(g)) => format!(" project: {} ", g.name),
        (None, None) => " no session ".to_string(),
    };
    // Harness name right-aligned on the top border so it visually
    // detaches from the session-name title. Sits just left of the
    // close button (or at the right edge when no close is shown).
    // Color matches the border so harness reads as part of the
    // title bar's frame, not as a separately-styled badge.
    let harness_label_text = summary.as_ref().map(|s| format!(" {} ", harness_label(s)));
    let harness_width = harness_label_text
        .as_deref()
        .map(UnicodeWidthStr::width)
        .unwrap_or(0) as u16;
    let harness_right = harness_label_text.as_ref().map(|text| {
        Line::from(Span::styled(
            text.clone(),
            pane_border_style(&app.theme, focused),
        ))
        .alignment(ratatui::layout::Alignment::Right)
    });
    let show_close = summary.is_some();
    let ui_session_id = summary.as_ref().and_then(|s| {
        app.ui_panels
            .get(&s.id)
            .filter(|panels| !panels.is_empty())
            .map(|_| s.id.clone())
    });
    let ui_trigger_label = " widgets ".to_string();
    let ui_trigger_width = ui_trigger_label.width() as u16;
    let ui_trigger_hovered = ui_session_id.as_ref().is_some_and(|_| {
        let (x_start, x_end, y) =
            dynamic_ui_trigger_range(area, show_close, ui_trigger_width, harness_width);
        app.mouse_pos
            .map(|(mx, my)| my == y && mx >= x_start && mx < x_end)
            .unwrap_or(false)
    });
    let ui_trigger_style = if ui_trigger_hovered {
        Style::default()
            .fg(app.theme.matrix_flash_good)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(app.theme.text)
    };
    let ui_trigger = ui_session_id.as_ref().map(|session_id| {
        let (x_start, x_end, y) =
            dynamic_ui_trigger_range(area, show_close, ui_trigger_width, harness_width);
        (
            session_id.clone(),
            x_start,
            x_end,
            y,
            Line::from(Span::styled(ui_trigger_label.clone(), ui_trigger_style))
                .alignment(ratatui::layout::Alignment::Right),
        )
    });
    // Right-aligned close button on the top border. Hover is
    // hit-tested against `app.mouse_pos` so the glyph bolds when the
    // cursor is over it — the click handler in `app.rs` mirrors the
    // same geometry to dispatch `OpenDeleteConfirm`. Only shown when
    // a session is actually selected (groups, "no session", and the
    // diff-overlay branch don't need it).
    let close_hovered = show_close && hovered_view_close_button(app, area);
    let close_style = if close_hovered {
        Style::default()
            .fg(app.theme.text)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.matrix_close)
    };
    let close =
        Line::from(Span::styled(" x ", close_style)).alignment(ratatui::layout::Alignment::Right);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(&app.theme, focused))
        .title(title);
    // Order matters: ratatui stacks right-aligned titles left-to-right
    // in the order they're added. Add the harness FIRST so it sits
    // to the left, then the close button SECOND so it lands at the
    // rightmost edge (matching `view_close_button_range`, which
    // hit-tests the last 3 cells of the top border).
    if let Some((session_id, x_start, x_end, y, ui)) = ui_trigger {
        app.layout.dynamic_ui_trigger = Some((x_start, x_end, y, session_id.clone()));
        app.layout
            .dynamic_ui_triggers
            .push((x_start, x_end, y, session_id));
        block = block.title(ui);
    }
    if let Some(h) = harness_right {
        block = block.title(h);
    }
    if show_close {
        block = block.title(close);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    clear_pane_side_borders(f, area, app);

    if summary.is_none() && group.is_none() {
        render_empty_session_state(f, inner, app);
        return;
    }
    if let Some(g) = app.selected_group() {
        render_group_overview(f, inner, app, g);
        render_main_transition(f, inner, app, window_id);
        return;
    }
    match app.view {
        ViewMode::Terminal => render_terminal_for_window(f, inner, app, window_id),
        ViewMode::Transcript => render_transcript(f, inner, app),
    }
    render_main_transition(f, inner, app, window_id);
}

fn render_empty_session_state(f: &mut Frame, area: Rect, app: &mut App) {
    let card = centered_rect(area, 72, 9);
    let label_style = Style::default().fg(app.theme.accent);
    let hover_style = label_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mouse = app.mouse_pos;
    let shortcut_rows = [
        (4_u16, 2_u16, "C-x C-f", KeyAction::OpenNewSession),
        (5_u16, 2_u16, "C-x x", KeyAction::OpenCommandPalette),
        (6_u16, 2_u16, "?", KeyAction::ToggleHelp),
    ];
    let mut hovered = [false; 3];
    for (i, (row, col, label, action)) in shortcut_rows.iter().enumerate() {
        let x_start = card.x + *col;
        let y = card.y + *row;
        let w = UnicodeWidthStr::width(*label) as u16;
        let x_end = x_start + w;
        hovered[i] = mouse
            .map(|(mx, my)| my == y && mx >= x_start && mx < x_end)
            .unwrap_or(false);
        app.layout.shortcut_hints.push(HintZone {
            x_start,
            x_end,
            y,
            action: *action,
        });
    }

    let lines = vec![
        Line::from(Span::styled(
            "Welcome to agentd",
            Style::default()
                .fg(app.theme.text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "Start with a session. Sessions are the live terminals agentd tracks.",
            Style::default().fg(app.theme.dim),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "C-x C-f",
                if hovered[0] { hover_style } else { label_style },
            ),
            Span::raw("  create a session"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("C-x x", if hovered[1] { hover_style } else { label_style }),
            Span::raw("    open the Operator"),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("?", if hovered[2] { hover_style } else { label_style }),
            Span::raw("        show shortcuts and concepts"),
        ]),
    ];
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, card);
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

fn render_group_overview(
    f: &mut Frame,
    area: Rect,
    app: &App,
    group: &agentd_protocol::GroupSummary,
) {
    let members: Vec<&agentd_protocol::SessionSummary> = app
        .sessions
        .iter()
        .filter(|s| s.group_id.as_deref() == Some(group.id.as_str()))
        .collect();
    let mut lines: Vec<Line> = Vec::with_capacity(members.len() + 3);
    lines.push(Line::from(vec![Span::styled(
        format!("Project: {}", group.name),
        group_name_style(&app.theme),
    )]));
    lines.push(Line::from(format!(
        "  {} member(s){}",
        members.len(),
        if group.collapsed { ", collapsed" } else { "" }
    )));
    lines.push(Line::from(""));
    if members.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (empty - move sessions into this project)",
            Style::default().fg(app.theme.dim),
        )));
    } else {
        for s in &members {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", session_status_glyph(app, s)),
                    state_style(&app.theme, s.state),
                ),
                Span::styled(primary_label(s), Style::default().fg(app.theme.text)),
                Span::raw("  "),
                Span::styled(harness_label(s), harness_style(&app.theme)),
            ]));
        }
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_terminal(f: &mut Frame, area: Rect, app: &mut App) {
    render_terminal_for_window(f, area, app, None)
}

fn render_terminal_for_window(f: &mut Frame, area: Rect, app: &mut App, window_id: Option<u64>) {
    let Some(id) = app.selected_id() else {
        return;
    };
    let panels: Vec<agentd_protocol::UiPanel> = app
        .ui_panels
        .get(&id)
        .map(|m| {
            let mut ids: Vec<_> = m.keys().collect();
            ids.sort();
            ids.into_iter()
                .filter_map(|panel_id| m.get(panel_id).cloned())
                .collect()
        })
        .unwrap_or_default();
    let inline_panel = latest_inline_panel(&panels);
    let sticky_panels: Vec<_> = panels
        .iter()
        .filter(|panel| panel.placement != agentd_protocol::UiPlacement::Inline)
        .cloned()
        .collect();
    if let Some(panel) = inline_panel.as_ref() {
        app.dynamic_ui_focused = Some((id.clone(), panel.id.clone()));
    }
    let scroll = app.scrollback_for_window(window_id);
    // Only adapters that publish `SessionEvent::EditorState` (currently
    // zarvis interactive) get the fixed editor pane at the bottom.
    // claude / codex / shell render their own input prompt inside the
    // PTY, so a second editor pane would just look like a duplicate.
    let editor_state = app.editor_states.get(&id).cloned();
    let agent_status = app.agent_statuses.get(&id).cloned();
    let inline_rows = inline_panel
        .as_ref()
        .map(|panel| inline_widget_rows(panel, area.width, area.height, &app.theme))
        .unwrap_or(0);
    let base_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(inline_rows),
    };
    let inline_area = inline_panel.as_ref().map(|_| Rect {
        x: area.x,
        y: area.y.saturating_add(base_area.height),
        width: area.width,
        height: inline_rows,
    });
    let (chat_area, editor_area) = if inline_panel.is_some() {
        (base_area, None)
    } else if editor_state.is_some() || agent_status.is_some() {
        let raw_rows = editor_pane_rows(editor_state.as_ref(), agent_status.as_ref(), area.width);
        let editor_rows: u16 = (raw_rows as u16).min(base_area.height.saturating_sub(1));
        let chat_height = base_area.height.saturating_sub(editor_rows);
        (
            Rect {
                x: base_area.x,
                y: base_area.y,
                width: base_area.width,
                height: chat_height,
            },
            Some(Rect {
                x: base_area.x,
                y: base_area.y + chat_height,
                width: base_area.width,
                height: editor_rows,
            }),
        )
    } else {
        (base_area, None)
    };
    let history = match app.histories.get_mut(&id) {
        Some(h) => h,
        None => {
            let label = if app.hydrating_sessions.contains(&id) {
                "Loading terminal history…"
            } else {
                "(no PTY history yet — interact to populate)"
            };
            let hint = Paragraph::new(label).style(Style::default().fg(app.theme.dim));
            f.render_widget(hint, chat_area);
            if let Some(area) = editor_area {
                render_editor_pane(
                    f,
                    area,
                    editor_state.as_ref(),
                    agent_status.as_ref(),
                    &app.theme,
                    true,
                );
            }
            if let (Some(area), Some(panel)) = (inline_area, inline_panel.as_ref()) {
                render_inline_dynamic_ui_panel(f, area, app, &id, panel);
            }
            return;
        }
    };
    // Render the chat at the FULL pane height, not `chat_area.height`.
    // The zarvis editor pane below grows/shrinks on nearly every
    // keystroke; sizing the parser to the shrinking chat area forced an
    // O(history) vt100 rebuild each time (the typing lag). Keeping the
    // parser at the stable `area.height` means editor growth never
    // resizes it — we just show its bottom `chat_area.height` rows.
    let preview = app.browser_previews.get(&id).cloned();
    app.layout.browser_preview_area = None;
    app.layout.browser_preview_close = None;
    app.layout.terminal_scrollbar = None;
    let row_offset = area.height.saturating_sub(chat_area.height);
    let out = history.replay(area.width, area.height, scroll);
    let clamped_scrollback = out.screen.scrollback();
    // Hide the chat pane's cursor block if we have our own editor pane
    // — otherwise the chat's vt100 cursor would render as a stray
    // block. For non-editor-pane sessions (claude / codex / shell)
    // keep the cursor visible so users see where their typing lands.
    // Clear the chat area before painting so any rows uncovered when the
    // editor pane shrinks are not left stale. This prevents visual gaps
    // that only disappear on terminal resize.
    f.render_widget(Clear, chat_area);
    // Extra defensive clear: fill the chat area with blank rows so some
    // terminals that don't honor Clear promptly still see overwritten cells.
    for row in 0..chat_area.height {
        let blank = " ".repeat(chat_area.width as usize);
        let r = Rect {
            x: chat_area.x,
            y: chat_area.y + row,
            width: chat_area.width,
            height: 1,
        };
        f.render_widget(Paragraph::new(Line::from(vec![Span::raw(blank)])), r);
    }
    render_pty_screen(
        f,
        chat_area,
        out.screen,
        &app.theme,
        editor_area.is_none(),
        row_offset,
    );
    app.block_hits.insert(
        id.clone(),
        translate_block_hits(out.blocks, row_offset, chat_area.height),
    );
    let terminal_scrollbar = render_terminal_scrollbar(
        f,
        chat_area,
        &app.theme,
        app.terminal_scrollbar_visible_until,
        clamped_scrollback,
        out.max_scrollback,
    );
    app.set_scrollback_for_window(window_id, clamped_scrollback);
    app.layout.terminal_scrollbar = terminal_scrollbar;
    render_visible_dynamic_ui_panels(f, area, app, &sticky_panels);
    if app.dynamic_ui_popover_open.as_deref() == Some(id.as_str()) && !sticky_panels.is_empty() {
        render_dynamic_ui_dropdown(f, area, app, &sticky_panels);
    }
    let (preview_area, preview_close) = render_browser_preview_overlay(
        f,
        chat_area,
        &app.theme,
        app.mouse_pos,
        preview.as_ref(),
        &mut app.image_resize_cache,
    );
    app.layout.browser_preview_area = preview_area;
    app.layout.browser_preview_close = preview_close;

    if let Some(area) = editor_area {
        // Also clear the editor area (defensive)
        f.render_widget(Clear, area);
        for row in 0..area.height {
            let blank = " ".repeat(area.width as usize);
            let r = Rect {
                x: area.x,
                y: area.y + row,
                width: area.width,
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw(blank)])), r);
        }
        render_editor_pane(
            f,
            area,
            editor_state.as_ref(),
            agent_status.as_ref(),
            &app.theme,
            true,
        );
    }
    if let (Some(area), Some(panel)) = (inline_area, inline_panel.as_ref()) {
        render_inline_dynamic_ui_panel(f, area, app, &id, panel);
    }
}

fn latest_inline_panel(panels: &[agentd_protocol::UiPanel]) -> Option<agentd_protocol::UiPanel> {
    panels
        .iter()
        .filter(|panel| panel.placement == agentd_protocol::UiPlacement::Inline)
        .max_by(|a, b| a.id.cmp(&b.id))
        .cloned()
}

/// Height (in terminal rows) the inline widget panel needs to display
/// `panel`'s markdown without truncation, capped at `available_height`.
///
/// We measure the *rendered* lines (after markdown parse and wrapping at the
/// panel's content width) rather than the raw source line count. The source
/// heuristic this replaced under-counted any line long enough to wrap, so
/// wide widgets used to clip. To keep the function pure we route hit
/// registrations into a throwaway buffer; the real render later does the
/// same parse against the real panel area and pushes hits into
/// `app.layout.dynamic_ui_action_hits`.
fn inline_widget_rows(
    panel: &agentd_protocol::UiPanel,
    width: u16,
    available_height: u16,
    theme: &Theme,
) -> u16 {
    if width == 0 || available_height == 0 {
        return 0;
    }
    // Mirror render_inline_dynamic_ui_panel's content_area: block borders
    // consume 2 cols (left+right) and the inner pad another 2.
    let content_width = width.saturating_sub(4);
    if content_width == 0 {
        return 3.min(available_height);
    }
    let measure_area = Rect {
        x: 0,
        y: 0,
        width: content_width,
        height: u16::MAX,
    };
    let suppress_first_heading = leading_markdown_heading(&panel.markdown).is_some();
    let mut throwaway_hits = Vec::new();
    let mut throwaway_url_hits = Vec::new();
    let lines = render_agentd_markdown_lines(
        &panel.markdown,
        theme,
        None,
        measure_area,
        None,
        None,
        &mut throwaway_hits,
        &mut throwaway_url_hits,
        suppress_first_heading,
    );
    let body_rows = visual_line_count(lines.iter(), content_width) as u16;
    let wanted = body_rows.saturating_add(2).max(3);
    wanted.min(available_height)
}

fn render_inline_dynamic_ui_panel(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    session_id: &str,
    panel: &agentd_protocol::UiPanel,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    app.layout.dynamic_ui_inline_hit = Some(crate::app::DynamicUiInlineHit {
        session_id: session_id.to_string(),
        panel_id: panel.id.clone(),
        area,
    });
    let title = dynamic_ui_panel_title(panel).unwrap_or_else(|| panel.id.clone());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::styled(
            title.clone(),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(app.theme.border_focused));
    let inner = block.inner(area);
    f.render_widget(block, area);
    render_inline_widget_header_affordances(f, area, app, session_id, &panel.id);
    let content_area = Rect {
        x: inner.x.saturating_add(1),
        y: inner.y,
        width: inner.width.saturating_sub(2),
        height: inner.height,
    };
    let suppress_first_heading = leading_markdown_heading(&panel.markdown).is_some();
    let lines = render_agentd_markdown_lines(
        &panel.markdown,
        &app.theme,
        app.mouse_pos,
        content_area,
        Some(session_id),
        Some(panel.id.as_str()),
        &mut app.layout.dynamic_ui_action_hits,
        &mut app.layout.dynamic_ui_url_hits,
        suppress_first_heading,
    );
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }),
        content_area,
    );
}

fn render_inline_widget_header_affordances(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    session_id: &str,
    panel_id: &str,
) {
    if area.width < 16 || area.height == 0 {
        return;
    }
    let hint = " Esc closes ";
    let close = " x ";
    let close_w = UnicodeWidthStr::width(close) as u16;
    let hint_w = UnicodeWidthStr::width(hint) as u16;
    let close_x = area
        .x
        .saturating_add(area.width.saturating_sub(close_w + 1));
    let hint_x = close_x.saturating_sub(hint_w);
    let y = area.y;
    f.buffer_mut()
        .set_string(hint_x, y, hint, Style::default().fg(app.theme.dim));
    f.buffer_mut().set_string(
        close_x,
        y,
        close,
        Style::default()
            .fg(app.theme.matrix_close)
            .add_modifier(Modifier::BOLD),
    );
    app.layout
        .dynamic_ui_panel_close_hits
        .push(crate::app::DynamicUiPanelCloseHit {
            session_id: session_id.to_string(),
            panel_id: panel_id.to_string(),
            row: y,
            start_col: close_x,
            end_col: close_x.saturating_add(close_w),
        });
}

fn render_dynamic_ui_dropdown(
    f: &mut Frame,
    session_area: Rect,
    app: &mut App,
    panels: &[agentd_protocol::UiPanel],
) {
    let width = panels
        .iter()
        .filter_map(dynamic_ui_panel_title)
        .map(|t| t.chars().count() as u16 + 6)
        .max()
        .unwrap_or(16)
        .clamp(16, session_area.width.saturating_sub(2).max(16));
    let height = (panels.len() as u16).saturating_add(3).max(4);
    let (trigger_start, trigger_end, trigger_y) = app
        .layout
        .dynamic_ui_trigger
        .as_ref()
        .map(|(start, end, y, _)| (*start, *end, *y))
        .unwrap_or((
            session_area.x + session_area.width.saturating_sub(width + 1),
            session_area.x + session_area.width.saturating_sub(1),
            session_area.y,
        ));
    let trigger_width = trigger_end.saturating_sub(trigger_start).max(1);
    let width = width
        .max(trigger_width)
        .min(session_area.width.saturating_sub(2).max(1));
    let x = trigger_start
        .min(session_area.x + session_area.width.saturating_sub(width + 1))
        .max(session_area.x.saturating_add(1));
    let area = Rect {
        x,
        y: trigger_y.saturating_add(1),
        width,
        height: height.min(session_area.height.saturating_sub(1).max(1)),
    };
    app.layout.dynamic_ui_dropdown_area = Some(area);
    f.render_widget(Clear, area);
    let session_id = app.selected_id().unwrap_or_default();
    let mut lines = vec![Line::raw("")];
    for panel in panels.iter().take(area.height.saturating_sub(2) as usize) {
        let selected = app
            .dynamic_ui_selected
            .contains(&(session_id.clone(), panel.id.clone()));
        let mark = if selected { "✓" } else { "·" };
        let title = dynamic_ui_panel_title(panel).unwrap_or_else(|| panel.id.clone());
        let row = area.y + lines.len() as u16;
        app.layout
            .dynamic_ui_widget_hits
            .push(crate::app::DynamicUiWidgetHit {
                session_id: session_id.clone(),
                panel_id: panel.id.clone(),
                row,
                start_col: area.x + 1,
                end_col: area.x + area.width.saturating_sub(1),
            });
        lines.push(Line::from(vec![
            Span::styled(format!(" {mark} "), Style::default().fg(app.theme.text)),
            Span::raw(title),
        ]));
    }
    let block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(app.theme.text));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_visible_dynamic_ui_panels(
    f: &mut Frame,
    session_area: Rect,
    app: &mut App,
    panels: &[agentd_protocol::UiPanel],
) {
    let Some(session_id) = app.selected_id() else {
        return;
    };
    let now = std::time::Instant::now();
    app.dynamic_ui_temporary_until
        .retain(|_, until| *until > now);
    let mut visible: Vec<_> = panels
        .iter()
        .filter(|panel| app.dynamic_ui_panel_visible(&session_id, &panel.id))
        .cloned()
        .collect();
    visible.sort_by(|a, b| a.id.cmp(&b.id));
    if visible.is_empty() {
        return;
    }
    let Some(area) = dynamic_ui_stack_area(session_area) else {
        return;
    };
    if let Some((mx, my)) = app.mouse_pos {
        if contains_rect(area, mx, my) {
            for panel in &visible {
                let key = (session_id.clone(), panel.id.clone());
                if app.dynamic_ui_temporary_until.contains_key(&key) {
                    app.dynamic_ui_temporary_until.insert(
                        key,
                        now + std::time::Duration::from_secs(crate::app::DYNAMIC_UI_AUTOHIDE_SECS),
                    );
                }
            }
        }
    }

    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    let mut rendered = render_dynamic_ui_stack_lines(inner, app, &session_id, &visible);
    let content_rows = rendered.len();
    let viewport_rows = inner.height as usize;
    let max_scroll = content_rows.saturating_sub(viewport_rows);
    let offset = app
        .dynamic_ui_scroll_offsets
        .entry(session_id.clone())
        .or_insert(0);
    *offset = (*offset).min(max_scroll);
    let scroll = *offset;
    translate_dynamic_ui_hits_for_scroll(app, inner, scroll, viewport_rows);
    rendered.extend(std::iter::repeat(Line::raw("")).take(viewport_rows));
    let visible_lines: Vec<_> = rendered
        .into_iter()
        .skip(scroll)
        .take(viewport_rows)
        .collect();

    f.render_widget(Clear, area);
    let focused = app.dynamic_ui_focused.is_some();
    let border_color = if focused {
        app.theme.text
    } else {
        app.theme.dim
    };
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color)),
        area,
    );
    f.render_widget(
        Paragraph::new(visible_lines).wrap(Wrap { trim: false }),
        inner,
    );
    render_dynamic_ui_stack_scrollbar(f.buffer_mut(), inner, scroll, content_rows);
    app.layout.dynamic_ui_popover_area = Some(area);
    app.layout.dynamic_ui_scroll_metrics = Some((session_id, content_rows, viewport_rows));
}

fn dynamic_ui_stack_area(session_area: Rect) -> Option<Rect> {
    if session_area.width < 4 || session_area.height < 3 {
        return None;
    }
    let max_w = ((session_area.width as f32) * 0.70).round() as u16;
    let width = max_w.clamp(32, session_area.width.saturating_sub(2).max(1));
    let height = ((session_area.height as f32) * 0.80).round().max(3.0) as u16;
    Some(Rect {
        x: session_area.x + session_area.width.saturating_sub(width + 1),
        y: session_area.y,
        width,
        height: height.min(session_area.height),
    })
}

fn translate_dynamic_ui_hits_for_scroll(
    app: &mut App,
    area: Rect,
    scroll: usize,
    viewport_rows: usize,
) {
    let top = area.y;
    let bottom = area.y.saturating_add(viewport_rows as u16);
    app.layout.dynamic_ui_action_hits.retain_mut(|hit| {
        let Some(row) = hit.row.checked_sub(scroll as u16) else {
            return false;
        };
        hit.row = row;
        hit.row >= top && hit.row < bottom
    });
    app.layout.dynamic_ui_url_hits.retain_mut(|hit| {
        let Some(row) = hit.row.checked_sub(scroll as u16) else {
            return false;
        };
        hit.row = row;
        hit.row >= top && hit.row < bottom
    });
    app.layout.dynamic_ui_panel_close_hits.retain_mut(|hit| {
        let Some(row) = hit.row.checked_sub(scroll as u16) else {
            return false;
        };
        hit.row = row;
        hit.row >= top && hit.row < bottom
    });
}

fn render_dynamic_ui_stack_lines(
    area: Rect,
    app: &mut App,
    session_id: &str,
    panels: &[agentd_protocol::UiPanel],
) -> Vec<Line<'static>> {
    let hover = app.mouse_pos;
    let mut rows = Vec::new();
    let row_w = area.width.saturating_sub(1);
    for (idx, panel) in panels.iter().enumerate() {
        if idx > 0 {
            rows.push(Line::from(Span::styled(
                "─".repeat(row_w as usize),
                Style::default().fg(app.theme.dim),
            )));
        }
        let title = dynamic_ui_panel_title(panel).unwrap_or_else(|| "widget".to_string());
        let focused =
            app.dynamic_ui_focused.as_ref() == Some(&(session_id.to_string(), panel.id.clone()));
        let title_style = if focused {
            Style::default()
                .fg(app.theme.highlight_fg)
                .bg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        };
        let close_style = if focused {
            Style::default()
                .fg(app.theme.highlight_fg)
                .bg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.dim)
        };
        let title_w = UnicodeWidthStr::width(title.as_str()) as u16;
        let close_w = UnicodeWidthStr::width("[-]") as u16;
        let title_pad = row_w.saturating_sub(title_w + close_w + 1) as usize;
        let close_start_col = area.x.saturating_add(row_w.saturating_sub(close_w));
        rows.push(Line::from(vec![
            Span::styled(title, title_style),
            Span::styled(" ".repeat(title_pad.saturating_add(1)), title_style),
            Span::styled("[-]", close_style),
        ]));
        app.layout
            .dynamic_ui_panel_close_hits
            .push(crate::app::DynamicUiPanelCloseHit {
                session_id: session_id.to_string(),
                panel_id: panel.id.clone(),
                row: area.y.saturating_add(rows.len().saturating_sub(1) as u16),
                start_col: close_start_col,
                end_col: close_start_col.saturating_add(close_w),
            });
        let content_area = Rect {
            x: area.x,
            y: area.y.saturating_add(rows.len() as u16),
            width: row_w,
            height: area.height,
        };
        let suppress_first_heading = leading_markdown_heading(&panel.markdown).is_some();
        let lines = render_agentd_markdown_lines(
            &panel.markdown,
            &app.theme,
            hover,
            content_area,
            Some(session_id),
            Some(panel.id.as_str()),
            &mut app.layout.dynamic_ui_action_hits,
            &mut app.layout.dynamic_ui_url_hits,
            suppress_first_heading,
        );
        rows.extend(lines);
        rows.push(Line::raw(""));
    }
    rows
}

fn render_dynamic_ui_stack_scrollbar(
    buf: &mut Buffer,
    area: Rect,
    scroll: usize,
    content_rows: usize,
) {
    let viewport_rows = area.height as usize;
    if area.width == 0 || content_rows <= viewport_rows || viewport_rows == 0 {
        return;
    }
    let track_x = area.x.saturating_add(area.width.saturating_sub(1));
    let thumb_h = ((viewport_rows * viewport_rows + content_rows / 2) / content_rows)
        .max(1)
        .min(viewport_rows) as u16;
    let max_top = area.height.saturating_sub(thumb_h) as usize;
    let max_scroll = content_rows.saturating_sub(viewport_rows).max(1);
    let thumb_top = ((scroll * max_top + max_scroll / 2) / max_scroll) as u16;
    for y in
        area.y.saturating_add(thumb_top)..area.y.saturating_add(thumb_top).saturating_add(thumb_h)
    {
        buf.set_string(track_x, y, "█", Style::default().fg(Color::Gray));
    }
}

fn contains_rect(area: Rect, col: u16, row: u16) -> bool {
    col >= area.x && col < area.x + area.width && row >= area.y && row < area.y + area.height
}

fn dynamic_ui_panel_title(panel: &agentd_protocol::UiPanel) -> Option<String> {
    first_markdown_heading(&panel.markdown)
        .or_else(|| {
            panel
                .source
                .as_deref()
                .or(Some(panel.id.as_str()))
                .map(widget_title_from_filename)
                .filter(|s| !s.is_empty())
        })
        .or_else(|| {
            panel
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

fn widget_title_from_filename(name: &str) -> String {
    name.strip_suffix(".md")
        .unwrap_or(name)
        .replace(['-', '_'], " ")
        .trim()
        .to_string()
}

fn first_markdown_heading(markdown: &str) -> Option<String> {
    markdown.lines().find_map(parse_markdown_heading)
}

fn leading_markdown_heading(markdown: &str) -> Option<String> {
    for line in markdown.lines() {
        if line.trim().is_empty() {
            continue;
        }
        return parse_markdown_heading(line);
    }
    None
}

fn parse_markdown_heading(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = trimmed.get(hashes..)?.trim_start();
    if rest.is_empty() || rest.starts_with('#') {
        return None;
    }
    Some(strip_markdown_emphasis(rest).trim().to_string()).filter(|s| !s.is_empty())
}

fn render_agentd_markdown_lines(
    markdown: &str,
    theme: &Theme,
    hover: Option<(u16, u16)>,
    panel_area: Rect,
    session_id: Option<&str>,
    panel_id: Option<&str>,
    hits: &mut Vec<crate::app::DynamicUiActionHit>,
    url_hits: &mut Vec<crate::app::DynamicUiUrlHit>,
    suppress_first_heading: bool,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut pending_action_spans: Vec<Span<'static>> = Vec::new();
    let mut pending_action_row = 0usize;
    let mut rendered_rows = 0usize;
    let mut skipped_first_heading = false;
    let mut in_timeline: Option<TimelineBlock> = None;
    for raw in markdown.lines() {
        let line = raw.trim_end();
        if let Some(timeline) = in_timeline.as_mut() {
            if line.trim() == ":::" {
                let rendered = render_timeline_block(
                    timeline,
                    theme,
                    hover,
                    panel_area,
                    rendered_rows,
                    session_id,
                    panel_id,
                    hits,
                    url_hits,
                );
                rendered_rows += visual_line_count(rendered.iter(), panel_area.width);
                lines.extend(rendered);
                in_timeline = None;
            } else {
                timeline.items.push(line.to_string());
            }
            continue;
        }
        if is_timeline_open(line) {
            if !pending_action_spans.is_empty() {
                let line = Line::from(std::mem::take(&mut pending_action_spans));
                rendered_rows += visual_line_count(std::iter::once(&line), panel_area.width);
                lines.push(line);
            }
            in_timeline = Some(TimelineBlock { items: Vec::new() });
            continue;
        }
        let action_links = parse_agentd_action_links(line);
        if !action_links.is_empty() && !is_checkline(line) {
            if pending_action_spans.is_empty() {
                pending_action_row = rendered_rows;
            }
            for (label, id, key, close) in action_links {
                if !pending_action_spans.is_empty() {
                    pending_action_spans.push(Span::raw("  "));
                }
                let text = key
                    .as_ref()
                    .map(|key| format!("[{key}] {label}"))
                    .unwrap_or_else(|| label.clone());
                let start_col = panel_area.x.saturating_add(1).saturating_add(
                    pending_action_spans
                        .iter()
                        .map(|span| UnicodeWidthStr::width(span.content.as_ref()) as u16)
                        .sum::<u16>(),
                );
                let row = panel_area.y.saturating_add(pending_action_row as u16);
                let end_col =
                    start_col.saturating_add(UnicodeWidthStr::width(text.as_str()) as u16);
                let is_hovered = hover
                    .map(|(mx, my)| my == row && mx >= start_col && mx < end_col)
                    .unwrap_or(false);
                let mut style = Style::default()
                    .fg(if is_hovered {
                        theme.matrix_flash_good
                    } else {
                        theme.accent
                    })
                    .add_modifier(Modifier::BOLD);
                if is_hovered {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                pending_action_spans.push(Span::styled(text, style));
                if let (Some(session_id), Some(panel_id)) = (session_id, panel_id) {
                    hits.push(crate::app::DynamicUiActionHit {
                        session_id: session_id.to_string(),
                        panel_id: panel_id.to_string(),
                        action: agentd_protocol::UiAction {
                            id,
                            label,
                            key,
                            style: None,
                            close,
                        },
                        row,
                        start_col,
                        end_col,
                    });
                }
            }
            continue;
        }
        if !pending_action_spans.is_empty() {
            let line = Line::from(std::mem::take(&mut pending_action_spans));
            rendered_rows += visual_line_count(std::iter::once(&line), panel_area.width);
            lines.push(line);
        }
        if suppress_first_heading
            && !skipped_first_heading
            && line.trim().is_empty()
            && lines.is_empty()
        {
            continue;
        }
        let before_normal_lines = lines.len();
        if line.is_empty() {
            lines.push(Line::raw(""));
        } else if suppress_first_heading
            && !skipped_first_heading
            && parse_markdown_heading(line).is_some()
        {
            skipped_first_heading = true;
            continue;
        } else if let Some(rest) = parse_markdown_heading(line) {
            lines.push(Line::from(Span::styled(
                rest,
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
        } else if let Some(rest) = line.strip_prefix("**") {
            if let Some((key, value)) = rest.split_once(":**") {
                lines.push(Line::from(vec![
                    Span::styled(format!("{key:<10}"), Style::default().fg(theme.dim)),
                    Span::raw(" "),
                    Span::styled(value.trim().to_string(), Style::default().fg(theme.text)),
                ]));
            } else {
                lines.push(Line::raw(strip_markdown_emphasis(line)));
            }
        } else if let Some(line) = parse_checkline(
            line,
            theme,
            hover,
            panel_area,
            rendered_rows,
            session_id,
            panel_id,
            hits,
            url_hits,
        ) {
            lines.push(line);
        } else {
            // Paragraph fallback: route through `render_inline_action_spans`
            // so `[text](https?://…)` URLs register as `DynamicUiUrlHit`s
            // and get the underline affordance. Lines containing only
            // `agentd:action/...` links are caught by the dedicated
            // action-line branch above; this catch-all picks up the mixed
            // paragraph case ("See [docs](https://…) for details.") that
            // would otherwise render the URL as inert text.
            let start_col = panel_area.x.saturating_add(1);
            let row = panel_area.y.saturating_add(rendered_rows as u16);
            let spans = render_inline_action_spans(
                line, theme, hover, row, start_col, session_id, panel_id, hits, url_hits,
            );
            lines.push(Line::from(spans));
        }
        if before_normal_lines < lines.len() {
            rendered_rows +=
                visual_line_count(lines[before_normal_lines..].iter(), panel_area.width);
        }
    }
    if !pending_action_spans.is_empty() {
        lines.push(Line::from(pending_action_spans));
    }
    if let Some(timeline) = in_timeline.as_ref() {
        lines.extend(render_timeline_block(
            timeline,
            theme,
            hover,
            panel_area,
            rendered_rows,
            session_id,
            panel_id,
            hits,
            url_hits,
        ));
    }
    lines
}

#[derive(Debug, Clone)]
struct TimelineBlock {
    items: Vec<String>,
}

#[derive(Debug)]
struct TimelineItem<'a> {
    text: &'a str,
    nested: Vec<&'a str>,
}

fn is_timeline_open(line: &str) -> bool {
    line.trim() == ":::timeline" || line.trim() == ":::agentd-timeline"
}

fn visual_line_count<'a>(lines: impl IntoIterator<Item = &'a Line<'static>>, width: u16) -> usize {
    let content_width = width.saturating_sub(2).max(1) as usize;
    lines
        .into_iter()
        .map(|line| {
            let width = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>();
            width.div_ceil(content_width).max(1)
        })
        .sum()
}

fn render_timeline_block(
    block: &TimelineBlock,
    theme: &Theme,
    hover: Option<(u16, u16)>,
    panel_area: Rect,
    rendered_rows_start: usize,
    session_id: Option<&str>,
    panel_id: Option<&str>,
    hits: &mut Vec<crate::app::DynamicUiActionHit>,
    url_hits: &mut Vec<crate::app::DynamicUiUrlHit>,
) -> Vec<Line<'static>> {
    let items = timeline_items(&block.items);
    let mut lines = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        let is_last = idx + 1 == items.len();
        let (glyph, text, color, bold) = timeline_item_parts(item.text, theme);
        let mut style = Style::default().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        let row = panel_area
            .y
            .saturating_add(1)
            .saturating_add(rendered_rows_start as u16)
            .saturating_add(lines.len() as u16);
        let item_start_col = panel_area
            .x
            .saturating_add(1)
            .saturating_add(UnicodeWidthStr::width("  ") as u16)
            .saturating_add(UnicodeWidthStr::width(format!("{glyph} ").as_str()) as u16);
        let mut spans = vec![Span::raw("  "), Span::styled(format!("{glyph} "), style)];
        spans.extend(render_inline_action_spans(
            &text,
            theme,
            hover,
            row,
            item_start_col,
            session_id,
            panel_id,
            hits,
            url_hits,
        ));
        lines.push(Line::from(spans));
        for nested in &item.nested {
            render_timeline_nested_line(
                &mut lines,
                nested,
                theme,
                !is_last,
                hover,
                panel_area,
                rendered_rows_start,
                session_id,
                panel_id,
                hits,
                url_hits,
            );
        }
        if is_last {
            lines.push(Line::raw(""));
        } else {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled("│", Style::default().fg(theme.dim)),
            ]));
        }
    }
    lines
}

fn timeline_items(raw_items: &[String]) -> Vec<TimelineItem<'_>> {
    let mut items: Vec<TimelineItem<'_>> = Vec::new();
    for raw in raw_items {
        if raw.trim().is_empty() {
            continue;
        }
        if is_indented(raw) {
            if let Some(item) = items.last_mut() {
                item.nested.push(raw.as_str());
            } else {
                items.push(TimelineItem {
                    text: raw.trim(),
                    nested: Vec::new(),
                });
            }
        } else {
            items.push(TimelineItem {
                text: raw.trim(),
                nested: Vec::new(),
            });
        }
    }
    items
}

fn is_indented(line: &str) -> bool {
    line.starts_with(' ') || line.starts_with('\t')
}

fn render_timeline_nested_line(
    lines: &mut Vec<Line<'static>>,
    nested: &str,
    theme: &Theme,
    continue_line: bool,
    hover: Option<(u16, u16)>,
    panel_area: Rect,
    rendered_rows_start: usize,
    session_id: Option<&str>,
    panel_id: Option<&str>,
    hits: &mut Vec<crate::app::DynamicUiActionHit>,
    url_hits: &mut Vec<crate::app::DynamicUiUrlHit>,
) {
    let connector = if continue_line { "│  " } else { "   " };
    let indent_cols = nested_indent_cols(nested).saturating_sub(2);
    let (glyph, text, color, bold) = timeline_item_parts(nested.trim(), theme);
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    let row = panel_area
        .y
        .saturating_add(1)
        .saturating_add(rendered_rows_start as u16)
        .saturating_add(lines.len() as u16);
    let item_start_col = panel_area
        .x
        .saturating_add(1)
        .saturating_add(UnicodeWidthStr::width("  ") as u16)
        .saturating_add(UnicodeWidthStr::width(connector) as u16)
        .saturating_add(indent_cols as u16)
        .saturating_add(UnicodeWidthStr::width(format!("{glyph} ").as_str()) as u16);
    let mut spans = vec![
        Span::raw("  "),
        Span::styled(connector, Style::default().fg(theme.dim)),
        Span::raw(" ".repeat(indent_cols)),
        Span::styled(format!("{glyph} "), style),
    ];
    spans.extend(render_inline_action_spans(
        &text,
        theme,
        hover,
        row,
        item_start_col,
        session_id,
        panel_id,
        hits,
        url_hits,
    ));
    lines.push(Line::from(spans));
}

fn nested_indent_cols(line: &str) -> usize {
    line.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .map(|ch| if ch == '\t' { 4 } else { 1 })
        .sum()
}

fn timeline_item_parts(item: &str, theme: &Theme) -> (&'static str, String, Color, bool) {
    let trimmed = item.trim();
    if let Some(text) = trimmed
        .strip_prefix("[x] ")
        .or_else(|| trimmed.strip_prefix("- [x] "))
    {
        (
            "✓",
            strip_markdown_emphasis(text),
            theme.matrix_flash_good,
            true,
        )
    } else if let Some(text) = trimmed
        .strip_prefix("[~] ")
        .or_else(|| trimmed.strip_prefix("- [~] "))
    {
        ("◉", strip_markdown_emphasis(text), theme.accent, true)
    } else if let Some(text) = trimmed
        .strip_prefix("[!] ")
        .or_else(|| trimmed.strip_prefix("- [!] "))
    {
        ("!", strip_markdown_emphasis(text), theme.warning, true)
    } else if let Some(text) = trimmed
        .strip_prefix("[ ] ")
        .or_else(|| trimmed.strip_prefix("- [ ] "))
    {
        ("○", strip_markdown_emphasis(text), theme.dim, false)
    } else {
        let text = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
            .or_else(|| trimmed.strip_prefix("+ "))
            .unwrap_or(trimmed);
        ("•", strip_markdown_emphasis(text), theme.accent_alt, false)
    }
}

fn is_checkline(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- [x] ")
        || trimmed.starts_with("- [~] ")
        || trimmed.starts_with("- [!] ")
        || trimmed.starts_with("- [ ] ")
}

fn parse_checkline(
    line: &str,
    theme: &Theme,
    hover: Option<(u16, u16)>,
    panel_area: Rect,
    rendered_rows: usize,
    session_id: Option<&str>,
    panel_id: Option<&str>,
    hits: &mut Vec<crate::app::DynamicUiActionHit>,
    url_hits: &mut Vec<crate::app::DynamicUiUrlHit>,
) -> Option<Line<'static>> {
    let indent = line
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .collect::<String>()
        .replace('\t', "    ");
    let trimmed = line.trim_start();
    let (glyph, item, color, bold) = if let Some(item) = trimmed.strip_prefix("- [x] ") {
        ("✓", item, theme.matrix_flash_good, true)
    } else if let Some(item) = trimmed.strip_prefix("- [~] ") {
        ("◉", item, theme.accent, true)
    } else if let Some(item) = trimmed.strip_prefix("- [!] ") {
        ("!", item, theme.warning, true)
    } else if let Some(item) = trimmed.strip_prefix("- [ ] ") {
        ("○", item, theme.dim, false)
    } else {
        return None;
    };
    let mut style = Style::default().fg(color);
    if bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    let row = panel_area
        .y
        .saturating_add(1)
        .saturating_add(rendered_rows as u16);
    let item_start_col = panel_area
        .x
        .saturating_add(1)
        .saturating_add(UnicodeWidthStr::width(indent.as_str()) as u16)
        .saturating_add(UnicodeWidthStr::width(format!("{glyph} ").as_str()) as u16);
    let mut spans = vec![Span::raw(indent), Span::styled(format!("{glyph} "), style)];
    spans.extend(render_inline_action_spans(
        item,
        theme,
        hover,
        row,
        item_start_col,
        session_id,
        panel_id,
        hits,
        url_hits,
    ));
    Some(Line::from(spans))
}

fn render_inline_action_spans(
    text: &str,
    theme: &Theme,
    hover: Option<(u16, u16)>,
    row: u16,
    start_col: u16,
    session_id: Option<&str>,
    panel_id: Option<&str>,
    hits: &mut Vec<crate::app::DynamicUiActionHit>,
    url_hits: &mut Vec<crate::app::DynamicUiUrlHit>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut col = start_col;
    while let Some(label_start) = rest.find('[') {
        let before = &rest[..label_start];
        if !before.is_empty() {
            let rendered = strip_markdown_emphasis(before);
            col = col.saturating_add(UnicodeWidthStr::width(rendered.as_str()) as u16);
            spans.push(Span::styled(rendered, Style::default().fg(theme.text)));
        }
        let after_open = &rest[label_start + 1..];
        let Some(label_end) = after_open.find(']') else {
            let rendered = strip_markdown_emphasis(&rest[label_start..]);
            spans.push(Span::styled(rendered, Style::default().fg(theme.text)));
            return spans;
        };
        let label = &after_open[..label_end];
        let after_label = &after_open[label_end + 1..];
        // Try to peel the `(target)` part out of `[label](target)`. If it
        // isn't there, render `[label]` literally.
        let parens = after_label
            .strip_prefix('(')
            .and_then(|s| s.find(')').map(|end| (&s[..end], &s[end + 1..])));
        let Some((target, after_close)) = parens else {
            let literal = &rest[label_start..label_start + label_end + 2];
            let rendered = strip_markdown_emphasis(literal);
            col = col.saturating_add(UnicodeWidthStr::width(rendered.as_str()) as u16);
            spans.push(Span::styled(rendered, Style::default().fg(theme.text)));
            rest = after_label;
            continue;
        };
        // Branch by URL scheme: agentd:action/ → in-app action hit;
        // http(s):// → external URL hit (opens via `open_url` on click);
        // anything else → fall through and render the literal text.
        if let Some(action_target) = target.strip_prefix("agentd:action/") {
            let (action_id, key, close) = parse_action_target(action_target);
            let prefix = key
                .as_ref()
                .map(|key| format!("[{key}] "))
                .unwrap_or_default();
            let display = format!("{prefix}{label}");
            let end_col = col.saturating_add(UnicodeWidthStr::width(display.as_str()) as u16);
            let is_hovered = hover
                .map(|(mx, my)| my == row && mx >= col && mx < end_col)
                .unwrap_or(false);
            let mut style = Style::default().fg(if is_hovered {
                theme.matrix_flash_good
            } else {
                theme.accent
            });
            if is_hovered {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if let (Some(session_id), Some(panel_id)) = (session_id, panel_id) {
                hits.push(crate::app::DynamicUiActionHit {
                    session_id: session_id.to_string(),
                    panel_id: panel_id.to_string(),
                    action: agentd_protocol::UiAction {
                        id: action_id,
                        label: label.to_string(),
                        key,
                        style: None,
                        close,
                    },
                    row,
                    start_col: col,
                    end_col,
                });
            }
            spans.push(Span::styled(display, style));
            col = end_col;
            rest = after_close;
        } else if target.starts_with("http://") || target.starts_with("https://") {
            let display = label.to_string();
            let end_col = col.saturating_add(UnicodeWidthStr::width(display.as_str()) as u16);
            let is_hovered = hover
                .map(|(mx, my)| my == row && mx >= col && mx < end_col)
                .unwrap_or(false);
            // Always underline external URLs so they read as links even
            // without a pointer; flip to REVERSED on hover so the hit area
            // is unambiguous before the user clicks (matches the action
            // link affordance above).
            let mut style = Style::default()
                .fg(if is_hovered {
                    theme.matrix_flash_good
                } else {
                    theme.accent
                })
                .add_modifier(Modifier::UNDERLINED);
            if is_hovered {
                style = style.add_modifier(Modifier::REVERSED);
            }
            if let (Some(session_id), Some(panel_id)) = (session_id, panel_id) {
                url_hits.push(crate::app::DynamicUiUrlHit {
                    session_id: session_id.to_string(),
                    panel_id: panel_id.to_string(),
                    url: target.to_string(),
                    row,
                    start_col: col,
                    end_col,
                });
            }
            spans.push(Span::styled(display, style));
            col = end_col;
            rest = after_close;
        } else {
            let literal = &rest[label_start..label_start + label_end + 2];
            let rendered = strip_markdown_emphasis(literal);
            col = col.saturating_add(UnicodeWidthStr::width(rendered.as_str()) as u16);
            spans.push(Span::styled(rendered, Style::default().fg(theme.text)));
            rest = after_label;
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(
            strip_markdown_emphasis(rest),
            Style::default().fg(theme.text),
        ));
    }
    spans
}

fn parse_action_target(target: &str) -> (String, Option<String>, bool) {
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

fn parse_agentd_action_links(line: &str) -> Vec<(String, String, Option<String>, bool)> {
    let mut out = Vec::new();
    let mut rest = line;
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
        let (id, key, close) = parse_action_target(&after_open[..id_end]);
        if !label.is_empty() && !id.is_empty() {
            out.push((label.to_string(), id, key, close));
        }
        rest = &after_open[id_end + 1..];
    }
    out
}

fn strip_markdown_emphasis(s: &str) -> String {
    s.replace("**", "")
}

fn render_terminal_scrollbar(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    visible_until: Option<Instant>,
    rendered_scrollback: usize,
    max_scrollback: usize,
) -> Option<crate::app::TerminalScrollbarHit> {
    if area.height < 3 || area.width < 2 || max_scrollback == 0 {
        return None;
    }
    let at_bottom = rendered_scrollback == 0;
    if at_bottom {
        let Some(visible_until) = visible_until else {
            return None;
        };
        if Instant::now() >= visible_until {
            return None;
        }
    }

    let track_h = area.height as usize;
    let viewport_h = area.height as usize;
    let total_h = max_scrollback.saturating_add(viewport_h).max(1);
    let thumb_h = ((viewport_h * track_h + total_h - 1) / total_h)
        .clamp(1, track_h)
        .max((track_h / 8).max(1));
    let max_thumb_top = track_h.saturating_sub(thumb_h);
    let thumb_top = if max_scrollback == 0 {
        0
    } else {
        ((max_scrollback.saturating_sub(rendered_scrollback)) * max_thumb_top) / max_scrollback
    };

    const SCROLLBAR_W: u16 = 4;
    let bar_w = area.width.min(SCROLLBAR_W);
    let x0 = area.x + area.width.saturating_sub(bar_w);
    let scrollbar_area = Rect {
        x: x0,
        y: area.y,
        width: bar_w,
        height: area.height,
    };
    let thumb = Rect {
        x: x0,
        y: area.y + thumb_top as u16,
        width: bar_w,
        height: thumb_h as u16,
    };
    let track_color = blend_color(Color::Black, theme.text, 0.30);
    let thumb_color = blend_color(Color::Black, theme.text, 0.80);
    for row in 0..track_h {
        let y = area.y + row as u16;
        for col in 0..bar_w {
            if let Some(cell) = f.buffer_mut().cell_mut(Position { x: x0 + col, y }) {
                // Keep the terminal glyph/foreground intact and tint only the
                // cell background. This approximates opacity while preserving
                // the text underneath the scrollbar track.
                cell.set_bg(track_color);
            }
        }
    }
    for row in 0..thumb_h {
        let y = area.y + (thumb_top + row) as u16;
        for col in 0..bar_w {
            if let Some(cell) = f.buffer_mut().cell_mut(Position { x: x0 + col, y }) {
                // Same opacity approximation as the track: preserve the
                // underlying glyph and foreground, only tint the background.
                cell.set_bg(thumb_color);
            }
        }
    }
    Some(crate::app::TerminalScrollbarHit {
        area: scrollbar_area,
        thumb,
        max_scrollback,
    })
}

fn render_browser_preview_overlay(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    mouse_pos: Option<(u16, u16)>,
    preview: Option<&crate::app::BrowserPreviewState>,
    resize_cache: &mut crate::app::ImageResizeCache,
) -> (Option<Rect>, Option<(u16, u16, u16)>) {
    let Some(preview_state) = preview else {
        return (None, None);
    };
    if area.width < 40 || area.height < 12 {
        return (None, None);
    }

    // Use the preview image (decoded once on insert) to size the panel to
    // its exact aspect ratio — no per-frame base64 decode.
    let Some(img) = preview_state.decoded.as_ref() else {
        return (None, None);
    };
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return (None, None);
    }

    let max_w = area.width.min((area.width / 3).max(36)).min(64);
    let max_h = area.height.min(18).min((area.height * 2 / 3).max(10));
    if max_w < 20 || max_h < 8 {
        return (None, None);
    }

    let max_inner_w = max_w.saturating_sub(2).max(1) as u32;
    let max_inner_h = max_h.saturating_sub(2).max(1) as u32;

    let scale = (max_inner_w as f32 / w as f32).min((max_inner_h as f32 * 2.0) / h as f32);
    let out_w = ((w as f32 * scale).round() as u32).clamp(1, max_inner_w) as u16;
    let out_h_px = ((h as f32 * scale).round() as u32).clamp(1, max_inner_h * 2) as u16;
    let rows = out_h_px.div_ceil(2);

    let panel_w = out_w + 2;
    let panel_h = rows + 2;

    let panel = Rect {
        x: area.x + area.width.saturating_sub(panel_w + 1),
        y: area.y + 1,
        width: panel_w,
        height: panel_h,
    };
    let close_w = 3;
    let close_bounds = (
        panel.x + panel.width.saturating_sub(close_w + 1),
        panel.x + panel.width.saturating_sub(1),
        panel.y,
    );
    let close_hovered = mouse_pos
        .map(|(mx, my)| my == close_bounds.2 && mx >= close_bounds.0 && mx < close_bounds.1)
        .unwrap_or(false);
    let close_style = if close_hovered {
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.matrix_dim)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.text))
        .title(
            Line::from(Span::styled(" x ", close_style))
                .alignment(ratatui::layout::Alignment::Right),
        );
    let inner = block.inner(panel);
    f.render_widget(Clear, panel);
    f.render_widget(block, panel);
    if inner.width == 0 || inner.height == 0 {
        return (Some(panel), Some(close_bounds));
    }
    let image_area = Rect {
        x: inner.x,
        y: inner.y,
        width: inner.width,
        height: inner.height,
    };
    if let Some(img) = preview_state.decoded.as_ref() {
        // Same dial-up reveal/erase as the matrix wallpaper, in sync.
        // Hovering this overlay pins it, so the erase won't start.
        let row_frac = preview_reveal_range(
            preview_state.revealed_at,
            preview_state.hide_after,
            std::time::Instant::now(),
            preview_state.hover_started.is_some(),
        );
        if row_frac.1 > row_frac.0 {
            let (ow, oh) = blit_scale_dims(img.dimensions(), image_area, false);
            let resized = resized_image(resize_cache, img, ow * 2, oh);
            paint_resized_quadrants(f, image_area, &resized, 1.0, row_frac);
        }
    }
    (Some(panel), Some(close_bounds))
}

/// Blit an RGBA image into `area` using half-block cells — each cell is
/// two vertically-stacked pixels rendered as `▀` (fg = top pixel, bg =
/// bottom pixel), so a cell row is 2 image rows.
///
/// - `cover = false` (contain): scale to fit inside `area`, centered,
///   letterboxed — the whole image is visible.
/// - `cover = true`: scale to fill `area` and crop the overflow,
///   preserving aspect ratio — no empty margins. Used for the wallpaper.
///
/// `dim` in `0.0..=1.0` multiplies brightness (1.0 = untouched); the
/// wallpaper dims so the rain stays legible on top.
/// Output pixel dims to resize an image to before half-block blitting
/// into `area`. `cover` true scales to fill (image ≥ area, crop); false
/// fits inside (image ≤ area, letterbox). Half-block packs 2 px per row.
fn blit_scale_dims((w, h): (u32, u32), area: Rect, cover: bool) -> (u32, u32) {
    let target_w = area.width as u32;
    let target_h_px = area.height as u32 * 2;
    let sx = target_w as f32 / w.max(1) as f32;
    let sy = target_h_px as f32 / h.max(1) as f32;
    let scale = if cover { sx.max(sy) } else { sx.min(sy) };
    (
        ((w as f32 * scale).round() as u32).max(1),
        ((h as f32 * scale).round() as u32).max(1),
    )
}

/// Resize `src` to `out_w × out_h`, memoized in `cache` keyed by the
/// source `Arc` identity + output dims. The matrix-rain wallpaper
/// re-blits the same image every animation frame; without this we'd
/// re-run a (very expensive) downscale each frame and the animation
/// stutters. Cache is a tiny MRU — at most a handful of live previews ×
/// render targets (overlay + wallpaper).
fn resized_image(
    cache: &mut crate::app::ImageResizeCache,
    src: &std::sync::Arc<image::RgbaImage>,
    out_w: u32,
    out_h: u32,
) -> std::sync::Arc<image::RgbaImage> {
    let key = (std::sync::Arc::as_ptr(src) as usize, out_w, out_h);
    if let Some(pos) = cache.iter().position(|e| e.0 == key) {
        let entry = cache.remove(pos);
        let img = entry.1.clone();
        cache.push((key, img.clone()));
        return img;
    }
    let resized = std::sync::Arc::new(image::imageops::resize(
        src.as_ref(),
        out_w,
        out_h,
        image::imageops::FilterType::Triangle,
    ));
    cache.push((key, resized.clone()));
    while cache.len() > 4 {
        cache.remove(0);
    }
    resized
}

/// Paint an already-resized RGBA image into `area` as half-block (`▀`)
/// cells — fg = top pixel, bg = bottom pixel. Center-crops `resized`
/// into the area (so cover images crop, contain images letterbox). `dim`
/// in `0.0..=1.0` multiplies brightness. `row_frac` is the `[start, end)`
/// fraction of the image's cell rows to paint (rest stay blank):
/// `(0.0, 1.0)` draws all; `(0.0, a)` reveals the top `a` (appear);
/// `(d, 1.0)` keeps only the bottom `1-d` (top-down erase on disappear).
/// Block-Elements quadrant glyph for a 2x2 foreground mask. Bit i set
/// means sub-cell i is foreground: 0=top-left, 1=top-right,
/// 2=bottom-left, 3=bottom-right.
const QUAD_CHARS: [&str; 16] = [
    " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
];

/// Best-fit quadrant glyph + (fg, bg) for a 2x2 block of pixels
/// (`[tl, tr, bl, br]`). Tries all 16 fg/bg partitions and keeps the one
/// with the lowest squared color error — chafa's symbol-mode core over
/// the universally-supported Block-Elements set. Doubles the effective
/// resolution vs a single half-block (4 sub-cells instead of 2).
fn best_quadrant(px: [[u8; 3]; 4]) -> (&'static str, [u8; 3], [u8; 3]) {
    let mut best_err = f32::INFINITY;
    let mut best = (0u8, [0u8; 3], [0u8; 3]);
    for pat in 0u8..16 {
        let (mut fg, mut bg) = ([0f32; 3], [0f32; 3]);
        let (mut fg_n, mut bg_n) = (0f32, 0f32);
        for (i, p) in px.iter().enumerate() {
            if pat & (1 << i) != 0 {
                (0..3).for_each(|c| fg[c] += p[c] as f32);
                fg_n += 1.0;
            } else {
                (0..3).for_each(|c| bg[c] += p[c] as f32);
                bg_n += 1.0;
            }
        }
        if fg_n > 0.0 {
            (0..3).for_each(|c| fg[c] /= fg_n);
        }
        if bg_n > 0.0 {
            (0..3).for_each(|c| bg[c] /= bg_n);
        }
        // Degenerate all-fg / all-bg: the empty set borrows the other's
        // mean so the cell renders as a single solid color.
        if fg_n == 0.0 {
            fg = bg;
        }
        if bg_n == 0.0 {
            bg = fg;
        }
        let mut err = 0f32;
        for (i, p) in px.iter().enumerate() {
            let t = if pat & (1 << i) != 0 { fg } else { bg };
            (0..3).for_each(|c| {
                let dd = p[c] as f32 - t[c];
                err += dd * dd;
            });
        }
        if err < best_err {
            best_err = err;
            let q = |v: [f32; 3]| [v[0].round() as u8, v[1].round() as u8, v[2].round() as u8];
            best = (pat, q(fg), q(bg));
        }
    }
    (QUAD_CHARS[best.0 as usize], best.1, best.2)
}

/// Paint `resized` (sampled at 2 sub-pixels per cell in each axis, i.e.
/// `2*cols × 2*rows`) into `area` as best-fit quadrant glyphs. Each cell
/// reads its 2x2 block and picks the glyph + fg/bg via [`best_quadrant`].
/// Center-crops into the area (cover crops, contain letterboxes). `dim`
/// scales brightness; `row_frac` is the `[start, end)` cell-row reveal
/// window (top-down appear / erase).
fn paint_resized_quadrants(
    f: &mut Frame,
    area: Rect,
    resized: &image::RgbaImage,
    dim: f32,
    row_frac: (f32, f32),
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (rw, rh) = resized.dimensions();
    if rw < 2 || rh < 2 {
        return;
    }
    let avail_cols = rw / 2;
    let avail_rows = rh / 2;
    let cols = avail_cols.min(area.width as u32);
    let rows = avail_rows.min(area.height as u32);
    // Center-crop in cell space, then center the result in the area.
    let cell_off_x = (avail_cols - cols) / 2;
    let cell_off_y = (avail_rows - rows) / 2;
    let x0 = area.x + ((area.width as u32 - cols) / 2) as u16;
    let y0 = area.y + ((area.height as u32 - rows) / 2) as u16;
    let rf = rows as f32;
    let start_row = ((row_frac.0 * rf).floor() as u32).min(rows);
    let end_row = ((row_frac.1 * rf).ceil() as u32).min(rows);
    let dim = dim.clamp(0.0, 1.0);
    let d = |c: u8| (c as f32 * dim).round() as u8;
    let buf = f.buffer_mut();
    for cy in start_row..end_row {
        let sy = (cell_off_y + cy) * 2;
        for cx in 0..cols {
            let sx = (cell_off_x + cx) * 2;
            let p = |dx: u32, dy: u32| {
                let q = resized.get_pixel(sx + dx, sy + dy).0;
                [q[0], q[1], q[2]]
            };
            let (ch, fg, bg) = best_quadrant([p(0, 0), p(1, 0), p(0, 1), p(1, 1)]);
            if let Some(cell) = buf.cell_mut(Position {
                x: x0 + cx as u16,
                y: y0 + cy as u16,
            }) {
                cell.set_symbol(ch);
                cell.set_style(
                    Style::default()
                        .fg(Color::Rgb(d(fg[0]), d(fg[1]), d(fg[2])))
                        .bg(Color::Rgb(d(bg[0]), d(bg[1]), d(bg[2]))),
                );
            }
        }
    }
}

/// Test-only convenience: resize + paint in one shot (no cache).
#[cfg(test)]
fn blit_image_quadrants(f: &mut Frame, area: Rect, img: &image::RgbaImage, cover: bool, dim: f32) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let (ow, oh) = blit_scale_dims(img.dimensions(), area, cover);
    let resized = image::imageops::resize(img, ow * 2, oh, image::imageops::FilterType::Triangle);
    paint_resized_quadrants(f, area, &resized, dim, (0.0, 1.0));
}

/// Paint the fixed bottom input pane:
/// - zero or more queued lines (gray `↑`), then
/// - one blank spacer row, then
/// - the active editor — one row per `\n`-separated buf line, cyan `❯`
///   on the first row, two-space indent on continuation rows.
/// Cursor is placed on the active line/col that corresponds to `state.cursor`.
fn render_editor_pane(
    f: &mut Frame,
    area: Rect,
    state: Option<&crate::app::EditorState>,
    agent_status: Option<&agentd_protocol::AgentStatus>,
    theme: &Theme,
    set_cursor: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    // Ensure we clear any stale content from a previous frame. When the
    // completion popup shrinks (or disappears) we must explicitly clear
    // the editor area so leftover glyphs don't linger until a terminal
    // resize forces a repaint.
    f.render_widget(Clear, area);

    let queued_style = Style::default().fg(theme.dim);
    let queued_glyph_style = queued_style.add_modifier(Modifier::BOLD);
    let active_glyph_style = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let active_text_style = Style::default().fg(theme.text);
    let prompt_w: u16 = 2;
    let empty_state = crate::app::EditorState::default();
    let state = state.unwrap_or(&empty_state);

    let total_rows = area.height as usize;
    let mut y = area.y;
    let mut remaining = total_rows;

    if let Some(status) = agent_status.filter(|s| s.active) {
        if remaining > 1 {
            y = y.saturating_add(1);
            remaining -= 1;
        }
        if remaining > 1 {
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            let label = format!(
                "* {}.. {}",
                status.status,
                format_elapsed(status.started_at_ms)
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    label,
                    Style::default().fg(theme.dim),
                )])),
                row,
            );
            y = y.saturating_add(1);
            remaining -= 1;
        }
        if remaining > 1 {
            y = y.saturating_add(1);
            remaining -= 1;
        }
    }

    let text_width = area.width.saturating_sub(prompt_w).max(1) as usize;

    // Queued entries — one `↑` per entry; wrapped/continuation rows
    // align under the prompt's text column with a two-space indent.
    'queued: for entry in &state.queued {
        let mut first = true;
        for logical in split_preserve_empty_lines(entry) {
            let wrapped = wrap_text(logical, text_width);
            for visual in wrapped {
                if remaining <= 1 {
                    break 'queued;
                }
                let row = Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: 1,
                };
                let spans = if first {
                    first = false;
                    vec![
                        Span::styled("↑ ", queued_glyph_style),
                        Span::styled(visual.text, queued_style),
                    ]
                } else {
                    vec![Span::raw("  "), Span::styled(visual.text, queued_style)]
                };
                f.render_widget(Paragraph::new(Line::from(spans)), row);
                y = y.saturating_add(1);
                remaining -= 1;
            }
        }
    }

    // Spacer row above completions / active prompt — visual breathing room.
    if remaining > 1 {
        y = y.saturating_add(1);
        remaining -= 1;
    }

    // Completion suggestions — bottom-pane anchored, rendered above
    // the active prompt so they don't pollute PTY scrollback or get
    // clipped below the terminal edge.
    let completion_style = Style::default().fg(theme.dim);
    for completion in &state.completions {
        // Keep at least one row for the active editor.
        if remaining <= 1 {
            break;
        }
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let text: String = completion.chars().take(text_width).collect();
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("  "),
                Span::styled(text, completion_style),
            ])),
            row,
        );
        y = y.saturating_add(1);
        remaining -= 1;
    }

    // Active editor — multiline and width-wrapped.
    let buf_lines = split_preserve_empty_lines(&state.buf);
    let mut cursor_pos: Option<(u16, u16)> = None;
    let mut char_seen = 0usize;
    let mut first_visual = true;
    'active: for logical in buf_lines {
        let logical_chars = logical.chars().count();
        let wrapped = wrap_text(logical, text_width);
        for visual in wrapped {
            if remaining == 0 {
                break 'active;
            }
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            let para = if first_visual {
                first_visual = false;
                Paragraph::new(Line::from(vec![
                    Span::styled("❯ ", active_glyph_style),
                    Span::styled(visual.text.clone(), active_text_style),
                ]))
            } else {
                Paragraph::new(Line::from(vec![
                    Span::raw("  "), // align with prompt width
                    Span::styled(visual.text.clone(), active_text_style),
                ]))
            };
            f.render_widget(para, row);
            let abs_start = char_seen + visual.start;
            let abs_end = char_seen + visual.end;
            let contains_cursor = state.cursor >= abs_start
                && (state.cursor < abs_end
                    || (visual.end == logical_chars && state.cursor <= abs_end));
            if cursor_pos.is_none() && contains_cursor {
                let col =
                    width_between_chars(logical, visual.start, state.cursor - char_seen) as u16;
                let x = area
                    .x
                    .saturating_add(prompt_w)
                    .saturating_add(col)
                    .min(area.x + area.width.saturating_sub(1));
                cursor_pos = Some((x, y));
            }
            y = y.saturating_add(1);
            remaining -= 1;
        }
        char_seen += logical_chars + 1; // +1 for the `\n`
    }
    if set_cursor {
        if let Some((x, y)) = cursor_pos {
            render_editor_cursor(f, Position { x, y }, theme);
        }
    }
}

fn render_editor_cursor(f: &mut Frame, pos: Position, theme: &Theme) {
    let Some(cell) = f.buffer_mut().cell_mut(pos) else {
        return;
    };
    if cell.symbol().is_empty() {
        cell.set_symbol(" ");
    }
    cell.set_style(
        Style::default()
            .fg(theme.highlight_fg)
            .bg(theme.accent)
            .add_modifier(Modifier::BOLD),
    );
}

fn editor_pane_rows(
    state: Option<&crate::app::EditorState>,
    agent_status: Option<&agentd_protocol::AgentStatus>,
    width: u16,
) -> usize {
    let text_width = width.saturating_sub(2).max(1) as usize;
    let queued_lines: usize = state
        .map(|s| {
            s.queued
                .iter()
                .map(|q| wrapped_text_rows(q, text_width))
                .sum()
        })
        .unwrap_or(0);
    let completion_lines = state.map(|s| s.completions.len()).unwrap_or(0);
    let buf_lines = state
        .map(|s| wrapped_text_rows(&s.buf, text_width))
        .unwrap_or(1);
    let status_lines = agent_status.filter(|s| s.active).map(|_| 3).unwrap_or(0);
    status_lines + queued_lines + 1 + completion_lines + buf_lines
}

#[derive(Debug, Clone)]
struct WrappedLine {
    text: String,
    start: usize,
    end: usize,
}

fn split_preserve_empty_lines(s: &str) -> Vec<&str> {
    if s.is_empty() {
        vec![""]
    } else {
        s.split('\n').collect()
    }
}

fn wrapped_text_rows(s: &str, width: usize) -> usize {
    split_preserve_empty_lines(s)
        .into_iter()
        .map(|line| wrap_text(line, width).len())
        .sum::<usize>()
        .max(1)
}

fn wrap_text(s: &str, width: usize) -> Vec<WrappedLine> {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1);
    if s.is_empty() {
        return vec![WrappedLine {
            text: String::new(),
            start: 0,
            end: 0,
        }];
    }

    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    for (idx, ch) in chars.iter().enumerate() {
        let ch_width = UnicodeWidthChar::width(*ch).unwrap_or(0);
        if idx > start && col + ch_width > width {
            out.push(WrappedLine {
                text: chars[start..idx].iter().collect(),
                start,
                end: idx,
            });
            start = idx;
            col = 0;
        }
        col += ch_width;
    }
    out.push(WrappedLine {
        text: chars[start..].iter().collect(),
        start,
        end: chars.len(),
    });
    out
}

fn width_between_chars(s: &str, start: usize, end: usize) -> usize {
    use unicode_width::UnicodeWidthChar;
    s.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
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

fn render_transcript(f: &mut Frame, area: Rect, app: &App) {
    // Windowed render: format only the events visible in the current
    // viewport instead of the full transcript. `format_event` is the
    // hot allocator here, so this keeps long sessions snappy even
    // when `app.transcript` contains thousands of events.
    //
    // Scroll is event-indexed (not wrapped-line-indexed), so wide
    // messages that wrap may push later rows off the bottom of the
    // viewport. The user can scroll one event at a time to bring
    // them in — same model as the pre-windowing code.
    let total = app.transcript.len();
    let height = area.height as usize;
    let max_scroll = total.saturating_sub(height);
    let scroll_start = if app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        (app.transcript_scroll as usize).min(max_scroll)
    };
    let end = (scroll_start + height).min(total);
    let lines: Vec<Line> = app.transcript[scroll_start..end]
        .iter()
        .map(|ev| format_event(&app.theme, ev))
        .collect();
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_modeline(f: &mut Frame, area: Rect, app: &App) {
    let s = app.selected_session();
    let conn = if app.connected { "" } else { " disconnected!" };
    let focus_label = match app.focus {
        PaneFocus::List => "list",
        PaneFocus::View => "view",
    };
    let scrollback_label = if app.view_scrollback > 0 {
        format!("scrollback:{}  ", app.view_scrollback)
    } else {
        String::new()
    };
    let approval_mode_badge = match s.and_then(|s| s.approval_mode.badge()) {
        Some(badge) => format!("[{badge}]  "),
        None => String::new(),
    };
    // "● remote: N" badge when at least one phone / remote client is
    // attached to the daemon. Visible signal that another surface
    // is also driving sessions, so the local user doesn't get
    // surprised by a session changing under them.
    let remote_badge = if app.remote_clients > 0 {
        format!("[● remote: {}]  ", app.remote_clients)
    } else {
        String::new()
    };
    let status = app.status.as_ref().map(|(m, _)| m.as_str()).unwrap_or("");
    let empty_hint = if s.is_none() && app.list_items().is_empty() && status.is_empty() {
        "new: C-x C-f  help: ?  palette: C-x x"
    } else {
        ""
    };
    let modeline = format!(
        " agentd  focus:{focus}  {sel}  {model}  {remote}{approval_mode}{scrollback}{chord}{empty_hint}{status}{conn} ",
        focus = focus_label,
        scrollback = scrollback_label,
        approval_mode = approval_mode_badge,
        remote = remote_badge,
        sel = match s {
            Some(s) => format!("\"{}\"", primary_label(s)),
            None => "-".into(),
        },
        model = match s {
            Some(s) => s.model.clone().unwrap_or_else(|| "-".into()),
            None => "-".into(),
        },
        chord = if app.chord_label.is_empty() {
            String::new()
        } else {
            format!("({})  ", app.chord_label)
        },
        empty_hint = empty_hint,
        status = status,
    );
    let para = Paragraph::new(modeline).style(
        Style::default()
            .bg(app.theme.modeline_bg)
            .fg(app.theme.modeline_fg),
    );
    f.render_widget(para, area);
}

/// Compute how many rows the minibuffer footer occupies this frame.
/// The default footer is 1 row (palette / hints / intent prompts).
/// When the orchestrator panel is focused (its `MinibufferIntent`
/// active) it expands to a fixed cap so the embedded zarvis REPL has
/// room to render its banner + chat bubbles, leaving the main view
/// most of the screen.
pub fn compute_minibuffer_height(app: &App, total_h: u16) -> u16 {
    let is_orch = matches!(
        app.minibuffer.as_ref().map(|m| &m.intent),
        Some(MinibufferIntent::Orchestrator)
    );
    if !is_orch {
        return 1;
    }
    minibuffer_panel_height(app.orchestrator_panel_h, total_h)
}

fn minibuffer_panel_height(preferred: Option<u16>, total_h: u16) -> u16 {
    // ~12 rows of panel + 1 row for the top border by default. The minimum
    // floor leaves room for the modeline + at least one row of the main view
    // on tiny terminals.
    let preferred = preferred
        .unwrap_or(crate::app::MINIBUFFER_PANEL_H_DEFAULT)
        .clamp(
            crate::app::MINIBUFFER_PANEL_H_MIN,
            crate::app::MINIBUFFER_PANEL_H_MAX,
        );
    let max_allowed = total_h
        .saturating_sub(3)
        .max(crate::app::MINIBUFFER_PANEL_H_MIN);
    preferred.min(max_allowed)
}

fn render_minibuffer(f: &mut Frame, area: Rect, app: &mut App) {
    app.layout.minibuffer_harness_hits.clear();

    // Orchestrator panel: events above, input row at the bottom.
    if matches!(
        app.minibuffer.as_ref().map(|m| &m.intent),
        Some(MinibufferIntent::Orchestrator)
    ) {
        render_orchestrator_panel(f, area, app);
        return;
    }

    if let Some(mb) = &app.minibuffer {
        // Harness picker: render `[name1|name2|...|group]` with each
        // name as its own clickable span, recording column ranges
        // for the click handler. Hover bolds + underlines.
        if matches!(mb.intent, MinibufferIntent::NewSessionHarness) {
            let mb_clone = mb.clone();
            render_harness_picker(f, area, app, &mb_clone);
            return;
        }
        let mut spans = vec![Span::raw(mb.prompt.clone()), Span::raw(mb.input.clone())];
        if let Some(err) = &mb.error {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(err.clone(), minibuffer_hint_style(app, mb)));
        }
        let para = Paragraph::new(Line::from(spans));
        f.render_widget(para, area);
        let x = area.x + mb.prompt.width() as u16 + mb.cursor as u16;
        f.set_cursor_position(Position { x, y: area.y });
        return;
    }
    if app.help_visible {
        let para = Paragraph::new("").style(Style::default().fg(app.theme.dim));
        f.render_widget(para, area);
        return;
    }
    // Build the hint as a sequence of (prefix, [(label, action), ...])
    // — prefix is non-clickable plain text, segments are individually
    // clickable + hover-highlightable. In the unzoomed layout focus is
    // a mouse click away, so `C-x o` is dropped — only zoom + palette
    // are shown. The zoomed layout keeps `C-x o` since the other pane
    // isn't visible to click.
    let (prefix, segments): (&str, Vec<(&str, KeyAction)>) = match app.zoom {
        ZoomMode::View => (
            "zoomed: view — ",
            vec![
                ("C-x x operator", KeyAction::OpenCommandPalette),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o list", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::List => (
            "zoomed: list — ",
            vec![
                ("C-x x operator", KeyAction::OpenCommandPalette),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o view", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::None => (
            "",
            vec![
                ("C-x x operator", KeyAction::OpenCommandPalette),
                ("C-x z zoom", KeyAction::ToggleZoom),
            ],
        ),
    };

    let mouse = app.mouse_pos;
    let base_style = Style::default().fg(app.theme.dim);
    let hover_style = Style::default()
        .fg(app.theme.text)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(segments.len() * 2 + 1);
    let mut col: u16 = area.x;
    if !prefix.is_empty() {
        spans.push(Span::styled(prefix.to_string(), base_style));
        col += UnicodeWidthStr::width(prefix) as u16;
    }
    let sep = "   ";
    let sep_w = UnicodeWidthStr::width(sep) as u16;
    for (i, (label, action)) in segments.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(sep.to_string()));
            col += sep_w;
        }
        let w = UnicodeWidthStr::width(*label) as u16;
        let x_start = col;
        let x_end = col + w;
        let hovered = match mouse {
            Some((mx, my)) => my == area.y && mx >= x_start && mx < x_end,
            None => false,
        };
        let style = if hovered { hover_style } else { base_style };
        spans.push(Span::styled(label.to_string(), style));
        app.layout.shortcut_hints.push(HintZone {
            x_start,
            x_end,
            y: area.y,
            action: *action,
        });
        col = x_end;
    }
    let para = Paragraph::new(Line::from(spans));
    f.render_widget(para, area);
}

fn minibuffer_hint_style(app: &App, mb: &Minibuffer) -> Style {
    if matches!(mb.intent, MinibufferIntent::SwitchSession) {
        Style::default().fg(app.theme.muted)
    } else {
        Style::default().fg(app.theme.danger)
    }
}

fn render_help(f: &mut Frame, area: Rect, theme: &Theme) -> Rect {
    // Target a comfortable reading width — long enough to keep each
    // command line on one row without wrapping, capped so it doesn't
    // span an ultra-wide terminal edge-to-edge. The outer rect adds
    // 1-cell margins on all four sides so the popup's border is
    // visually detached from the underlying TUI content. The bounds
    // (`-6` width / `-4` height) reserve room for both borders plus
    // those margins.
    const MARGIN: u16 = 1;
    let target_w = 92u16;
    let width = target_w.min(area.width.saturating_sub(2 * MARGIN + 4));
    // Content height = lines + 2 borders + 2 vertical padding.
    let height =
        (HELP_TEXT.lines().count() as u16 + 4).min(area.height.saturating_sub(2 * MARGIN + 2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    // Outer rect = popup grown by `MARGIN` cells on each side. We
    // Clear *this* so the gap between the popup's border and any
    // background content paints blank — without that, foreground
    // text from the underlying frame leaks right up to the border.
    let outer = Rect {
        x: x.saturating_sub(MARGIN),
        y: y.saturating_sub(MARGIN),
        width: width + 2 * MARGIN,
        height: height + 2 * MARGIN,
    };
    f.render_widget(Clear, outer);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border_focused))
        .padding(ratatui::widgets::Padding::new(2, 2, 1, 1))
        .title(" help (any key to close) ");
    let para = Paragraph::new(HELP_TEXT)
        .block(block)
        .style(Style::default().fg(theme.text))
        .wrap(Wrap { trim: false });
    f.render_widget(para, popup);
    popup
}

const HELP_TEXT: &str = "
emacs keymap (default; AGENTD_KEYMAP=vim for vim profile)

  getting started
    A session is one live task or terminal that agentd keeps in the list.
    A harness is the runtime for a session: zarvis, codex, claude, or shell.
    The left pane selects sessions; the right pane shows the selected session.
    Use C-x C-f to create a session, then choose a harness.
    Use C-x x for the command palette when you forget a shortcut.

  focus + view
    C-x o           other window (list → windows → list)
    RET (on list)   focus the selected session's view
    C-x 2 / C-x 3   split current main window below / right
    C-x 0 / C-x 1   delete current window / delete other windows
    C-x ^           make current window taller
    C-x } / C-x {   make current window wider / narrower
    C-x t           toggle transcript ↔ terminal view
    C-x z           zoom: fill the screen with the session view
    C-n / down      next session
    C-p / up        prev session

  session actions
    C-x C-f         new session
    C-x b           switch focused window to an existing session
    C-x i           send input to selected session
    C-x k           delete selected session (confirms; kills if running)
    C-x d           show diff
    C-x r           rename selected session (clears title on empty submit)
    C-c C-c         interrupt

  scrollback
    C-x [ / C-x ]   scroll page up/down
    C-v / M-v       scroll page down/up
    g g / G         scroll top / bottom

  pinning (live tile in the pin strip below the main view)
    Space / C-x p   toggle pin on selected session

  reorder list
    C-x C-p         move selected session up   (Meta-free, works everywhere)
    C-x C-n         move selected session down
    Shift-up/down   same, in terminals that pass Shift to arrows
                    (iTerm2/WezTerm/Alacritty yes; macOS Terminal.app no)

  mouse
    drag text       select visible TUI text and copy to terminal clipboard
    C-x m           toggle mouse capture off/on for native selection fallback

  global
    M-x / C-x x     command palette (C-x x is Meta-free)
                    palette commands: new send delete rename diff border
                                      zoom interrupt refresh harnesses help
    ?               toggle this help
    C-x C-c / q     quit

When the right pane is showing a PTY-backed session (shell / interactive
claude / interactive codex) and focus is on the view, keystrokes go to the
child. `C-x` is the escape prefix — start any `C-x …` chord above to run
an agentd command without changing focus.
";

fn format_event(theme: &Theme, ev: &TimestampedEvent) -> Line<'static> {
    let ts = ev.at.format("%H:%M:%S").to_string();
    let mut spans = vec![Span::styled(
        format!("[{ts}] "),
        Style::default().fg(theme.dim),
    )];
    spans.extend(format_event_body(theme, &ev.event));
    Line::from(spans)
}

fn format_event_body(theme: &Theme, ev: &SessionEvent) -> Vec<Span<'static>> {
    match ev {
        // UI-only geometry hint; never rendered as a transcript line.
        SessionEvent::PtyResize { .. } => Vec::new(),
        SessionEvent::Message { role, text } => {
            let role_label = match role {
                MessageRole::User => "user",
                MessageRole::Assistant => "agent",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };
            vec![
                Span::styled(format!("{role_label:>7}: "), role_style(theme, *role)),
                Span::raw(text.clone()),
            ]
        }
        SessionEvent::Reasoning { text } => {
            // Model's private thinking — dim + italic so the user can
            // tell it apart from the actual response.
            let style = Style::default()
                .fg(theme.dim)
                .add_modifier(Modifier::ITALIC);
            vec![
                Span::styled("thinking: ".to_string(), style),
                Span::styled(text.clone(), style),
            ]
        }
        SessionEvent::ToolUse { tool, args } => {
            let args_s = serde_json::to_string(args).unwrap_or_default();
            vec![
                Span::styled("   tool: ", Style::default().fg(theme.tool)),
                Span::raw(format!("{tool}({})", shorten(&args_s, 120))),
            ]
        }
        SessionEvent::ToolResult { tool, ok, output } => {
            let (mark, color) = if *ok {
                (" ✓ ", theme.success)
            } else {
                (" ✗ ", theme.danger)
            };
            vec![
                Span::styled(format!("   {}", mark), Style::default().fg(color)),
                Span::raw(format!("{tool} {}", shorten(output, 200))),
            ]
        }
        SessionEvent::AwaitingInput { prompt } => {
            let p = prompt.clone().unwrap_or_default();
            vec![Span::styled(
                format!("   ◐ awaiting input: {p}"),
                Style::default().fg(theme.warning),
            )]
        }
        SessionEvent::Status { state, detail } => {
            let d = detail.clone().unwrap_or_default();
            vec![Span::styled(
                format!("   ⟳ {} {}", state.label(), d),
                Style::default().fg(theme.info),
            )]
        }
        SessionEvent::AgentStatus(status) => {
            if status.active {
                vec![Span::styled(
                    format!(
                        "   * {}.. {}",
                        status.status,
                        format_elapsed(status.started_at_ms)
                    ),
                    Style::default().fg(theme.dim),
                )]
            } else {
                vec![]
            }
        }
        SessionEvent::Cost {
            usd,
            tokens_in,
            tokens_out,
        } => vec![Span::styled(
            format!("   $ ${:.4} (in={} out={})", usd, tokens_in, tokens_out),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::Diff { patch } => vec![Span::raw(format!("   Δ {}", shorten(patch, 200)))],
        SessionEvent::Error { message } => vec![Span::styled(
            format!("   ! {message}"),
            Style::default().fg(theme.danger),
        )],
        SessionEvent::Reset => vec![Span::styled(
            "   ↺ session reset".to_string(),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::Done { exit_code } => vec![Span::styled(
            format!("   ▢ done (exit {exit_code})"),
            Style::default().fg(theme.success),
        )],
        SessionEvent::Pty { data } => vec![Span::styled(
            format!("   ⌷ pty: {} bytes (switch to terminal view)", data.len()),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::ToolApprovalRequest {
            tool,
            args_summary,
            risk,
            ..
        } => {
            let risk_label = match risk {
                agentd_protocol::ToolRisk::Safe => "safe",
                agentd_protocol::ToolRisk::Risky => "risky",
            };
            vec![Span::styled(
                format!(
                    "   ? approve [{risk_label}] {tool}({})",
                    shorten(args_summary, 160)
                ),
                Style::default().fg(theme.warning),
            )]
        }
        // Task-lifecycle events are bookkeeping; the daemon tracks
        // them in its per-session registry. The transcript already
        // shows the matching ToolUse / ToolResult, so render these
        // minimally (or hide entirely).
        SessionEvent::TaskStart { tool, .. } => vec![Span::styled(
            format!("   ⏵ task start: {tool}"),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::TaskBackgrounded { .. } => vec![Span::styled(
            "   ↳ task backgrounded".to_string(),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::TaskEnd { ok, .. } => {
            let glyph = if *ok { "✓" } else { "✗" };
            vec![Span::styled(
                format!("   {glyph} task end"),
                Style::default().fg(theme.dim),
            )]
        }
        SessionEvent::EditorState { .. } => {
            // Editor state is rendered by the input pane, not the
            // chat transcript.
            vec![]
        }
        SessionEvent::UiPanel(panel) => vec![Span::styled(
            format!(
                "   ▣ ui panel: {}",
                panel.title.as_deref().unwrap_or(&panel.id)
            ),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::UiDelete { id } => vec![Span::styled(
            format!("   ▣ ui panel deleted: {id}"),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::BrowserPreview(preview) => vec![Span::styled(
            format!("   ◱ browser preview: {}", shorten(&preview.url, 120)),
            Style::default().fg(theme.dim),
        )],
        SessionEvent::ContextCompacted {
            dropped_turns,
            tokens_before,
            tokens_after,
            summary_preview,
            ..
        } => vec![Span::styled(
            format!(
                "   ◧ compacted {} turns (~{}→{} tok): {}",
                dropped_turns,
                tokens_before,
                tokens_after,
                shorten(summary_preview, 120)
            ),
            Style::default().fg(theme.dim),
        )],
    }
}

fn pane_border_style(theme: &Theme, focused: bool) -> Style {
    if focused {
        Style::default().fg(theme.border_focused)
    } else {
        Style::default().fg(theme.border)
    }
}

fn group_name_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.group)
        .add_modifier(Modifier::BOLD)
}

fn harness_style(theme: &Theme) -> Style {
    Style::default()
        .fg(theme.harness)
        .add_modifier(Modifier::BOLD)
}

/// Clip `s` to at most `max` display columns, appending `…` when the
/// original didn't fit. Width-aware (handles multi-cell glyphs / CJK).
fn truncate_to_width(s: &str, max: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    let target = max.saturating_sub(1); // reserve a cell for "…"
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > target {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

fn render_pin_strip(f: &mut Frame, area: Rect, app: &mut App, pinned_ids: &[String]) {
    if pinned_ids.is_empty() || area.height < 3 || area.width < 6 {
        return;
    }
    let tiles = pin_tile_layout(area, pinned_ids.len());
    let selected_id = app.selected_id();
    for (tile_area, id) in tiles.iter().zip(pinned_ids.iter()) {
        let summary = app.sessions.iter().find(|s| &s.id == id);
        let is_selected = selected_id.as_deref() == Some(id.as_str());
        // Title: ` ★ <status> <label> `. The star on the top
        // border is the unpin affordance — same gesture as the
        // diamond in the list view. Click anywhere in the 4-cell
        // gutter that holds the diamond + status glyph to unpin;
        // `render_pin_diamond_tooltip` paints a hover tooltip and
        // recolors the diamond on hover. The harness name is shown
        // right-aligned on the same top border (mirrors the main
        // session view's title layout) in the border color, and the
        // session label ellipsizes when the tile is too narrow to
        // fit both.
        let total_pin = tile_area.width as usize;
        let harness_w = summary
            .map(|s| 2 + UnicodeWidthStr::width(harness_label(s).as_str()))
            .unwrap_or(0);
        let glyph_w = summary
            .map(|s| UnicodeWidthStr::width(session_status_glyph(app, s)))
            .unwrap_or(0);
        // Title shape ` ★ <glyph> <label> ` = 5 cells of scaffolding
        // (1 leading + diamond + 1 + glyph + 1 + label + 1 trailing
        // = label + 4 + diamond + glyph; diamond is 1 cell).
        let pin_label_budget = total_pin
            .saturating_sub(2) // corners
            .saturating_sub(harness_w)
            .saturating_sub(5 + glyph_w);
        // ` ★ <status> <label> ` — the star is the pinned marker (same
        // shape + bluish `info` color as the list-view pin); the rest of
        // the title stays in the border style.
        let title_rest = match summary {
            Some(s) => format!(
                " {} {} ",
                session_status_glyph(app, s),
                truncate_to_width(&primary_label(s), pin_label_budget),
            ),
            None => format!(" {} ", short_id(id)),
        };
        let title = Line::from(vec![
            Span::raw(" "),
            Span::styled("★", Style::default().fg(app.theme.info)),
            Span::raw(title_rest),
        ]);
        let harness_right = summary.map(|s| {
            Line::from(Span::styled(
                format!(" {} ", harness_label(s)),
                pane_border_style(&app.theme, is_selected),
            ))
            .alignment(ratatui::layout::Alignment::Right)
        });
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(&app.theme, is_selected))
            .title(title);
        if let Some(h) = harness_right {
            block = block.title(h);
        }
        let inner = block.inner(*tile_area);
        f.render_widget(block, *tile_area);
        clear_pane_side_borders(f, *tile_area, app);
        if let Some(history) = app.histories.get_mut(id) {
            // Render at the *main view's* virtual size, not the
            // pin tile's narrow size. Each `ItemHistory` is shared
            // between the main view and the pin tile, and `replay`
            // resizes the cached vt100 parser to the requested
            // dims. Asking the pin tile (e.g. 30×6) and then the
            // main view (e.g. 120×30) on alternating frames
            // thrashes the parser: every resize re-feeds the
            // pending chunk through a freshly-sized grid, content
            // reflows at the wrong width, and the main view
            // appears clipped at the pin width.
            //
            // Always replay at main-view dims so the parser stays
            // stable; `render_pty_tail` crops to `inner` for
            // display (top-left of the rendered screen, anchored
            // to the cursor row).
            let (main_cols, main_rows) = app.terminal_pane_size;
            let cols = main_cols.max(inner.width).max(1);
            let rows = main_rows.max(inner.height).max(1);
            let out = history.replay(cols, rows, 0);
            render_pty_tail(f, inner, out.screen, &app.theme);
        } else {
            // No PTY data yet — show a placeholder.
            let label = if app.hydrating_sessions.contains(id) {
                "loading history…"
            } else {
                "(no data yet)"
            };
            let p = Paragraph::new(label).style(Style::default().fg(app.theme.dim));
            f.render_widget(p, inner);
        }
        render_pin_transition(f, inner, app, id);
    }
}

pub fn pin_tile_layout(area: Rect, n: usize) -> Vec<Rect> {
    let n = n.max(1);
    let cols = n.min(4).max(1);
    let rows = (n + cols - 1) / cols;
    let row_constraints: Vec<Constraint> = (0..rows)
        .map(|_| Constraint::Ratio(1, rows as u32))
        .collect();
    let row_areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints(row_constraints)
        .split(area);
    let mut tiles: Vec<Rect> = Vec::with_capacity(n);
    for (r_idx, row_area) in row_areas.iter().enumerate() {
        let placed = r_idx * cols;
        let remaining = n.saturating_sub(placed);
        if remaining == 0 {
            break;
        }
        let cols_here = remaining.min(cols).max(1);
        let col_constraints: Vec<Constraint> = (0..cols_here)
            .map(|_| Constraint::Ratio(1, cols_here as u32))
            .collect();
        let col_areas = Layout::default()
            .direction(Direction::Horizontal)
            .constraints(col_constraints)
            .split(*row_area);
        for col_area in col_areas.iter() {
            tiles.push(*col_area);
        }
    }
    tiles
}

/// Render a slice of a vt100 screen into `area`, preserving colors and
/// attributes. The window is anchored at the bottom of the source screen so
/// harness status/input bars (zarvis, codex, claude all park them in the
/// last few rows) stay visible on pinned tiles. Used by the pin strip.
/// Translate tool-block hit-rects from full-parser-screen rows into
/// `chat_area`-relative rows when the chat is rendered as the bottom
/// slice of a taller parser (see `render_pty_screen`). Blocks scrolled
/// entirely above the visible slice are dropped; partially-visible ones
/// are clipped. Header/button hit zones only survive if the header row
/// itself is visible. A `row_offset` of 0 is the identity transform.
fn translate_block_hits(
    blocks: Vec<crate::pty_render::BlockHitRect>,
    row_offset: u16,
    visible_h: u16,
) -> Vec<crate::pty_render::BlockHitRect> {
    if row_offset == 0 {
        return blocks;
    }
    blocks
        .into_iter()
        .filter_map(|b| {
            if b.row_end <= row_offset {
                return None; // entirely above the visible slice
            }
            let row_start = b.row_start.saturating_sub(row_offset);
            let row_end = (b.row_end - row_offset).min(visible_h);
            if row_end <= row_start {
                return None;
            }
            let header_visible =
                b.header_row >= row_offset && (b.header_row - row_offset) < visible_h;
            Some(crate::pty_render::BlockHitRect {
                call_id: b.call_id,
                row_start,
                row_end,
                bg_button: if header_visible { b.bg_button } else { None },
                kill_button: if header_visible { b.kill_button } else { None },
                header_row: if header_visible {
                    b.header_row - row_offset
                } else {
                    row_start
                },
            })
        })
        .collect()
}

/// Paint `screen` into `area`, showing the rows starting at `row_offset`.
///
/// `row_offset` is non-zero only when the parser is taller than the chat
/// area — i.e. a zarvis editor pane is carved out below. We keep the
/// parser at the full pane height (so the editor growing/shrinking on
/// every keystroke never resizes — and rebuilds — it) and render only
/// its bottom `area.height` rows here. `row_offset = full_height -
/// area.height`.
fn render_pty_screen(
    f: &mut Frame,
    area: Rect,
    screen: &vt100::Screen,
    theme: &Theme,
    show_cursor: bool,
    row_offset: u16,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    f.render_widget(Clear, area);
    let visible_h = area.height;
    let visible_w = area.width;
    let buf = f.buffer_mut();
    for row in 0..visible_h {
        for col in 0..visible_w {
            let Some(cell) = screen.cell(row_offset + row, col) else {
                continue;
            };
            let x = area.x + col;
            let y = area.y + row;
            if let Some(buf_cell) = buf.cell_mut(Position { x, y }) {
                paint_vt100_cell(buf_cell, cell, theme);
            }
        }
    }
    if show_cursor && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        let row = row
            .saturating_add(u16::try_from(screen.scrollback()).unwrap_or(u16::MAX))
            .saturating_sub(row_offset);
        if row < area.height && col < area.width {
            let x = area.x + col;
            let y = area.y + row;
            if let Some(buf_cell) = f.buffer_mut().cell_mut(Position { x, y }) {
                if screen
                    .cell(row + row_offset, col)
                    .map(|c| c.has_contents())
                    .unwrap_or(false)
                {
                    buf_cell.set_style(Style::default().add_modifier(Modifier::REVERSED));
                } else {
                    buf_cell.set_symbol("█");
                    buf_cell.set_style(Style::default().fg(theme.muted));
                }
            }
        }
    }
}

fn render_pty_tail(f: &mut Frame, area: Rect, screen: &vt100::Screen, theme: &Theme) {
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let visible_h = area.height.min(rows);
    let visible_w = area.width.min(cols);
    // End of window is exclusive; always show the bottom `visible_h` rows.
    let end_row = rows;
    let start_row = end_row.saturating_sub(visible_h);
    let buf = f.buffer_mut();
    for r in 0..visible_h {
        for c in 0..visible_w {
            let src_row = start_row + r;
            let src_col = c;
            let Some(cell) = screen.cell(src_row, src_col) else {
                continue;
            };
            let x = area.x + c;
            let y = area.y + r;
            if let Some(buf_cell) = buf.cell_mut(Position { x, y }) {
                paint_vt100_cell(buf_cell, cell, theme);
            }
        }
    }
}

fn paint_vt100_cell(buf_cell: &mut ratatui::buffer::Cell, cell: &vt100::Cell, theme: &Theme) {
    let contents = cell.contents();
    if contents.is_empty() {
        buf_cell.set_char(' ');
    } else {
        buf_cell.set_symbol(contents);
    }
    buf_cell.set_style(vt100_cell_style(cell, theme));
}

fn vt100_cell_style(cell: &vt100::Cell, theme: &Theme) -> Style {
    let mut s = Style::default();
    s = s.fg(themed_vt100_fg(cell.fgcolor(), theme));
    if let Some(c) = vt100_color(cell.bgcolor()) {
        s = s.bg(c);
    }
    let mut mods = Modifier::empty();
    if cell.bold() {
        mods.insert(Modifier::BOLD);
    }
    // `\x1b[2m` (dim/faint) — without this the pin tile renders
    // styled-dim text (e.g. zarvis's `[+N lines — click to expand]`
    // markers and tool args) at full intensity, while the main view
    // shows them correctly because `tui_term::PseudoTerminal`
    // translates the attribute itself.
    if cell.dim() {
        mods.insert(Modifier::DIM);
    }
    if cell.italic() {
        mods.insert(Modifier::ITALIC);
    }
    if cell.underline() {
        mods.insert(Modifier::UNDERLINED);
    }
    if cell.inverse() {
        mods.insert(Modifier::REVERSED);
    }
    s.add_modifier(mods)
}

fn themed_vt100_fg(c: vt100::Color, theme: &Theme) -> Color {
    match c {
        vt100::Color::Default => theme.text,
        vt100::Color::Idx(i) => indexed_grayscale_luma(i)
            .map(|luma| green_for_luma(theme, luma))
            .unwrap_or(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) if r == g && g == b => green_for_luma(theme, r),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn indexed_grayscale_luma(i: u8) -> Option<u8> {
    match i {
        0 => Some(0),
        7 => Some(229),
        8 => Some(102),
        15 => Some(255),
        232..=255 => Some(8u8.saturating_add((i - 232).saturating_mul(10))),
        _ => None,
    }
}

fn green_for_luma(theme: &Theme, luma: u8) -> Color {
    blend_color(theme.matrix_dim, theme.text, f32::from(luma) / 255.0)
}

fn vt100_color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(Color::Indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

/// Glyph for a session's "what's it doing right now" indicator: an
/// animated spinner if the session is `Running` and its PTY has produced
/// bytes within the quiescence window, otherwise the session's static
/// lifecycle glyph.
fn session_status_glyph(app: &App, s: &SessionSummary) -> &'static str {
    // `agent_statuses` only holds entries while a turn is active (the live
    // handler removes them on the `active=false` turn-end event), so a
    // present, active entry means zarvis is working right now.
    let agent_active = app
        .agent_statuses
        .get(&s.id)
        .map(|st| st.active)
        .unwrap_or(false);
    if session_should_animate_status(s, app.pty_active(&s.id), agent_active) {
        app.spinner_frame()
    } else {
        s.state.glyph()
    }
}

fn session_should_animate_status(s: &SessionSummary, pty_active: bool, agent_active: bool) -> bool {
    if !matches!(s.state, SessionState::Running) {
        return false;
    }
    // Zarvis reports an explicit agent-turn signal (`AgentStatus`):
    // active=true at turn start, active=false at every turn end. A zarvis
    // session can linger in `Running` while idle (e.g. an interrupted turn
    // that returned without flipping back to AwaitingInput), so animate
    // strictly while that turn is active — not merely because the
    // lifecycle state reads `Running`. Animating on `Running` alone was
    // the bug: an idle session kept spinning.
    //
    // Shell / PTY-only harnesses have no agent-status signal and also sit
    // in `Running` while idle, so they keep the short PTY-activity gate.
    if s.harness == "zarvis" {
        agent_active
    } else {
        pty_active
    }
}

fn state_style(theme: &Theme, state: SessionState) -> Style {
    match state {
        SessionState::Pending => Style::default().fg(theme.muted),
        SessionState::Running => Style::default().fg(theme.success),
        SessionState::AwaitingInput => Style::default().fg(theme.success),
        SessionState::Paused => Style::default().fg(theme.warning),
        SessionState::Done => Style::default().fg(theme.info),
        SessionState::Errored => Style::default().fg(theme.danger),
    }
}

fn role_style(theme: &Theme, role: MessageRole) -> Style {
    match role {
        MessageRole::User => Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
        MessageRole::Assistant => Style::default().fg(theme.assistant),
        MessageRole::System => Style::default().fg(theme.system),
        MessageRole::Tool => Style::default().fg(theme.tool),
    }
}

/// Fit a session name into `width` cells.
///
/// - When the name fits, return it as-is (padded callers handle alignment).
/// - When the name doesn't fit and `scroll` is `None`, truncate with a
///   trailing ellipsis.
/// - When the name doesn't fit and `scroll = Some(offset)`, treat
///   `name + "   "` as a cyclic ribbon and return `width` chars starting
///   at `offset % ribbon_len`. The caller is expected to bump `offset`
///   each tick so the selected row's name appears to scroll.
fn fit_name(name: &str, width: usize, scroll: Option<usize>) -> String {
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= width {
        return name.to_string();
    }
    match scroll {
        None => {
            if width == 0 {
                return String::new();
            }
            if width == 1 {
                return "…".into();
            }
            let take = width - 1;
            let mut s: String = chars.iter().take(take).collect();
            s.push('…');
            s
        }
        Some(offset) => {
            // 3-char gap so the wrap-around boundary is visible.
            let mut ribbon: Vec<char> = chars.clone();
            ribbon.extend("   ".chars());
            let n = ribbon.len();
            // Hold position 0 for `PAUSE_TICKS` extra cycles so the user
            // can read the title's start before it begins scrolling.
            // At ~6 chars/sec, 7 ticks ≈ ~1.2s pause.
            const PAUSE_TICKS: usize = 7;
            let cycle = n + PAUSE_TICKS;
            let phase = offset % cycle;
            let start = if phase < PAUSE_TICKS {
                0
            } else {
                phase - PAUSE_TICKS
            };
            let mut visible = String::with_capacity(width);
            for i in 0..width {
                visible.push(ribbon[(start + i) % n]);
            }
            visible
        }
    }
}

fn shorten(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.replace('\n', " ")
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}...", truncated.replace('\n', " "))
    }
}

pub fn short_event_label(ev: &SessionEvent) -> String {
    match ev {
        SessionEvent::PtyResize { cols, rows } => format!("pty_resize {cols}x{rows}"),
        SessionEvent::Message { role, text } => format!("msg:{:?} {}", role, shorten(text, 60)),
        SessionEvent::Reasoning { text } => format!("reasoning {}", shorten(text, 60)),
        SessionEvent::ToolUse { tool, .. } => format!("tool {tool}"),
        SessionEvent::ToolResult { tool, ok, .. } => format!("tool-result {tool} ok={ok}"),
        SessionEvent::AwaitingInput { .. } => "awaiting input".to_string(),
        SessionEvent::Status { state, .. } => format!("status {}", state.label()),
        SessionEvent::AgentStatus(status) => {
            if status.active {
                format!("agent-status {}", status.status)
            } else {
                "agent-status cleared".to_string()
            }
        }
        SessionEvent::BrowserPreview(preview) => {
            format!("browser-preview {}", shorten(&preview.url, 60))
        }
        SessionEvent::UiPanel(panel) => {
            format!("ui-panel {}", panel.title.as_deref().unwrap_or(&panel.id))
        }
        SessionEvent::UiDelete { id } => format!("ui-delete {id}"),
        SessionEvent::Cost { usd, .. } => format!("cost ${:.4}", usd),
        SessionEvent::Diff { .. } => "diff".to_string(),
        SessionEvent::Error { message } => format!("error: {}", shorten(message, 60)),
        SessionEvent::Reset => "reset".to_string(),
        SessionEvent::Done { exit_code } => format!("done (exit {exit_code})"),
        SessionEvent::Pty { data } => format!("pty: {} bytes", data.len()),
        SessionEvent::ToolApprovalRequest { tool, .. } => format!("approve? {tool}"),
        SessionEvent::TaskStart { tool, call_id, .. } => format!("task-start {tool} {call_id}"),
        SessionEvent::TaskBackgrounded { call_id } => format!("task-bg {call_id}"),
        SessionEvent::TaskEnd { call_id, ok, .. } => format!("task-end {call_id} ok={ok}"),
        SessionEvent::EditorState {
            queued,
            buf,
            cursor,
            completions,
        } => {
            format!(
                "editor: q={} buf={}b cur={} completions={}",
                queued.len(),
                buf.len(),
                cursor,
                completions.len()
            )
        }
        SessionEvent::ContextCompacted {
            dropped_turns,
            tokens_before,
            tokens_after,
            ..
        } => format!(
            "compact: {} turns ~{}→{} tok",
            dropped_turns, tokens_before, tokens_after
        ),
    }
}

fn short_id(id: &str) -> &str {
    let n = id.len().min(10);
    &id[..n]
}

pub fn is_headless(s: &agentd_protocol::SessionSummary) -> bool {
    matches!(s.mode.as_deref(), Some("headless"))
}

fn harness_label(s: &agentd_protocol::SessionSummary) -> String {
    if is_headless(s) {
        format!("(headless) {}", s.harness)
    } else {
        s.harness.clone()
    }
}

/// User-facing primary label for a session: the user-set title when present,
/// otherwise the short id (the hash). Trimmed/empty titles count as unset.
fn primary_label(s: &agentd_protocol::SessionSummary) -> String {
    match s.title.as_deref() {
        Some(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => short_id(&s.id).to_string(),
    }
}

/// Render the orchestrator's PTY in the bottom strip. The orchestrator
/// is a zarvis interactive session; the same items-model history that
/// renders any other PTY-backed session is replayed onto a fresh
/// parser sized to the panel's inner area each frame. Tool-block
/// hit ranges land in `app.block_hits` for click-toggle dispatch.
fn render_orchestrator_panel(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(id) = app.orchestrator_id.clone() else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(app.theme.border));
    let inner = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    // Publish the orchestrator panel's inner size; `run_loop` debounces
    // and fires the actual `pty_resize` once the value settles. Stops
    // drag-resize from spraying one IPC per frame.
    let cols = inner.width.max(1);
    let rows = inner.height.max(1);
    if app.orchestrator_desired_size != Some((cols, rows)) {
        app.orchestrator_desired_size = Some((cols, rows));
    }

    // Same split logic as the main view: if the orchestrator session
    // is publishing `EditorState`, carve out a fixed editor pane at
    // the bottom of the panel so the `❯` and live typing are always
    // visible — otherwise this panel rendered only the PTY scrollback
    // and the editor was invisible (zarvis stopped painting it).
    //
    // On a fresh TUI attach, the user can open the orchestrator panel
    // before the adapter's initial `EditorState` notification has
    // reached us. Still reserve one fallback input row in that first
    // frame so the prompt/cursor render in the right place instead of
    // showing an empty panel with the terminal cursor at the origin.
    let has_editor_state = app.editor_states.contains_key(&id);
    let editor_state = app.editor_states.get(&id).cloned();
    let agent_status = app.agent_statuses.get(&id).cloned();
    let force_input_pane = !has_editor_state && app.is_orchestrator_panel_open();
    let (chat_area, editor_area) =
        if editor_state.is_some() || agent_status.is_some() || force_input_pane {
            let raw_rows =
                editor_pane_rows(editor_state.as_ref(), agent_status.as_ref(), inner.width);
            let editor_rows: u16 = (raw_rows as u16).min(inner.height.saturating_sub(1));
            let chat_height = inner.height.saturating_sub(editor_rows);
            (
                Rect {
                    x: inner.x,
                    y: inner.y,
                    width: inner.width,
                    height: chat_height,
                },
                Some(Rect {
                    x: inner.x,
                    y: inner.y + chat_height,
                    width: inner.width,
                    height: editor_rows,
                }),
            )
        } else {
            (inner, None)
        };

    let history = app.histories.entry(id.clone()).or_default();
    // Full panel height (stable) keeps the parser from being rebuilt as
    // the editor pane grows on each keystroke; render only the bottom
    // slice. See the matching note in `render_terminal`.
    let row_offset = inner.height.saturating_sub(chat_area.height);
    let out = history.replay(inner.width, inner.height, app.orchestrator_scrollback);
    render_pty_screen(
        f,
        chat_area,
        out.screen,
        &app.theme,
        editor_area.is_none(),
        row_offset,
    );
    app.block_hits.insert(
        id,
        translate_block_hits(out.blocks, row_offset, chat_area.height),
    );
    if let Some(area) = editor_area {
        render_editor_pane(
            f,
            area,
            editor_state.as_ref(),
            agent_status.as_ref(),
            &app.theme,
            true,
        );
    }
}

/// Modal popup listing the selected session's task registry, opened
/// by `/tasks`. Shows running + backgrounded + recent terminal
/// states with a one-line summary per task. v1 is read-only at the
/// keyboard / mouse layer (Esc closes; clicks outside close);
/// in-block `[kill]` / `[bg]` buttons stay the way to act on a
/// running task. Future iterations can wire per-row kill buttons
/// here without changing the data model.
fn render_tasks_popup(f: &mut Frame, app: &mut App) {
    let Some(popup) = app.tasks_popup.as_ref() else {
        return;
    };
    let total = f.area();
    let w = total.width.saturating_sub(8).min(90);
    let h = total
        .height
        .saturating_sub(4)
        .min((popup.tasks.len() as u16 + 4).max(8));
    if w < 30 || h < 6 {
        return;
    }
    let x = total.x + (total.width.saturating_sub(w)) / 2;
    let y = total.y + (total.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    app.layout.modal_area = Some(rect);
    let title = format!(
        " tasks — session {} ({} entries) — Esc to close ",
        short_id(&popup.session_id),
        popup.tasks.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border_focused))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(app.theme.info)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    if popup.tasks.is_empty() {
        let p = Paragraph::new("(no tasks recorded for this session)")
            .style(Style::default().fg(app.theme.dim));
        f.render_widget(p, inner);
        return;
    }

    // Render newest-first table; bounded to inner.height rows.
    let visible = popup
        .tasks
        .iter()
        .rev()
        .take(inner.height as usize)
        .enumerate();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let mut lines: Vec<Line> = Vec::new();
    for (_idx, t) in visible {
        let (state_glyph, state_color) = match t.state {
            agentd_protocol::TaskState::Running => ("◐", app.theme.warning),
            agentd_protocol::TaskState::Backgrounded => ("↻", app.theme.info),
            agentd_protocol::TaskState::Completed => ("✓", app.theme.success),
            agentd_protocol::TaskState::Failed => ("✗", app.theme.danger),
            agentd_protocol::TaskState::Cancelled => ("⊘", app.theme.dim),
        };
        let elapsed_ms = t.ended_at_ms.unwrap_or(now_ms) - t.started_at_ms;
        let elapsed = format!("{:.1}s", (elapsed_ms.max(0)) as f64 / 1000.0);
        let title_text: String = t.args_summary.chars().take(40).collect();
        let body = format!(
            " {state_glyph}  {tool:<20}  {args:<40}  {elapsed:>7}",
            tool = t.tool.chars().take(20).collect::<String>(),
            args = title_text,
            elapsed = elapsed,
        );
        lines.push(Line::from(vec![Span::styled(
            body,
            Style::default().fg(state_color),
        )]));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

/// Centered modal that paints either the URL+QR (success) or a
/// diagnostic (tunnel timeout). Auto-sizes around the actual
/// content — wide enough for the QR block, tall enough for the
/// rows + URL + optional hint. Esc to dismiss (wired in `app.rs`).
fn render_remote_control_popup(f: &mut Frame, app: &mut App) {
    let Some(popup) = app.remote_control_popup.as_ref() else {
        return;
    };
    let total = f.area();

    let (title, title_color, body_lines, body_w, body_h) = match popup {
        crate::app::RemoteControlPopup::Starting(p) => render_remote_starting(app, p),
        crate::app::RemoteControlPopup::Ok(p) => render_remote_ok(app, p),
        crate::app::RemoteControlPopup::Err {
            local_only,
            message,
        } => render_remote_err(app, *local_only, message),
    };

    let want_w = body_w + 6;
    let want_h = body_h + 4;
    let w = want_w.min(total.width.saturating_sub(4)).max(40);
    let h = want_h.min(total.height.saturating_sub(2)).max(8);
    if w < 30 || h < 6 {
        return;
    }
    let x = total.x + (total.width.saturating_sub(w)) / 2;
    let y = total.y + (total.height.saturating_sub(h)) / 2;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    app.layout.modal_area = Some(rect);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border_focused))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(title_color)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    let para = Paragraph::new(body_lines).wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn render_remote_starting<'a>(
    app: &App,
    p: &'a crate::app::RemoteControlOk,
) -> (&'static str, ratatui::style::Color, Vec<Line<'a>>, u16, u16) {
    let mut title_tuple = render_remote_ok(app, p);
    title_tuple.0 = " /remote-control — starting public tunnel… — Esc to close ";
    title_tuple.1 = app.theme.warning;
    title_tuple.2.insert(
        0,
        Line::from(Span::styled(
            "Setting up public tunnel… local URL is ready; QR will update automatically.",
            Style::default()
                .fg(app.theme.warning)
                .add_modifier(Modifier::BOLD),
        )),
    );
    title_tuple.2.insert(1, Line::raw(""));
    title_tuple.3 = title_tuple.3.max(72);
    title_tuple.4 = title_tuple.4.saturating_add(2);
    title_tuple
}

/// Build the popup body for a successful `remote.start`. Returns
/// `(title, title_color, lines, body_w, body_h)`.
fn render_remote_ok<'a>(
    app: &App,
    p: &'a crate::app::RemoteControlOk,
) -> (&'static str, ratatui::style::Color, Vec<Line<'a>>, u16, u16) {
    let qr_lines: Vec<&str> = p.qr.lines().collect();
    let qr_w = qr_lines
        .iter()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(0);
    let qr_h = qr_lines.len() as u16;
    let url_w = p.url.chars().count() as u16;
    let user_line = "user: remote";
    let user_w = user_line.chars().count() as u16;
    let password_line = format!("password: {}", p.password);
    let pw_w = password_line.chars().count() as u16;
    let hint_w = p
        .hint
        .as_deref()
        .map(|s| s.chars().count() as u16)
        .unwrap_or(0);
    let body_w = qr_w.max(url_w).max(user_w).max(pw_w).max(hint_w);
    // QR + blank + URL + user + password (+ blank + hint if present).
    let body_h = qr_h + 4 + if p.hint.is_some() { 2 } else { 0 };

    let (title, title_color) = match (p.local_only, p.tunnel_ready) {
        (true, _) => (
            " /remote-control debug — local URL only — Esc to close ",
            app.theme.warning,
        ),
        (false, true) => (
            " /remote-control — public tunnel ready — Esc to close ",
            app.theme.success,
        ),
        // local_only=false + tunnel_ready=false is no longer
        // reachable on the daemon side (tunnel timeout now
        // returns an error), but keep a graceful title just in
        // case the shape evolves.
        (false, false) => (" /remote-control — Esc to close ", app.theme.warning),
    };

    let mut lines: Vec<Line> = Vec::with_capacity(qr_lines.len() + 4);
    for ql in &qr_lines {
        lines.push(Line::from(Span::raw((*ql).to_string())));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        p.url.clone(),
        Style::default()
            .fg(app.theme.info)
            .add_modifier(Modifier::BOLD),
    )));
    // Browser's basic-auth prompt asks for both username and
    // password; render both so the user knows what to type. The
    // daemon enforces username == "remote" (see REMOTE_USERNAME)
    // so this value isn't decorative — anything else 401s.
    lines.push(Line::from(vec![
        Span::styled("user:     ", Style::default().fg(app.theme.dim)),
        Span::styled(
            "remote",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("password: ", Style::default().fg(app.theme.dim)),
        Span::styled(
            p.password.clone(),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    if let Some(hint) = p.hint.as_deref() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            hint.to_string(),
            Style::default().fg(app.theme.dim),
        )));
    }

    (title, title_color, lines, body_w, body_h)
}

/// Build the popup body for a failed `remote.start`. Used when the
/// tunnel-mode call times out: the daemon returned a diagnostic;
/// we paint it instead of a fake URL so the user knows exactly
/// what to fix.
fn render_remote_err<'a>(
    app: &App,
    local_only: bool,
    message: &'a str,
) -> (&'static str, ratatui::style::Color, Vec<Line<'a>>, u16, u16) {
    let title = if local_only {
        " /remote-control debug — failed — Esc to close "
    } else {
        " /remote-control — tunnel didn't come up — Esc to close "
    };
    let header = "tunnel start failed:";
    let body_lines: Vec<Line> = vec![
        Line::from(Span::styled(
            header.to_string(),
            Style::default()
                .fg(app.theme.danger)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            message.to_string(),
            Style::default().fg(app.theme.text),
        )),
    ];
    let body_w = message
        .lines()
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(40)
        .max(header.chars().count() as u16);
    let body_h = 3 + message.lines().count() as u16;
    (title, app.theme.danger, body_lines, body_w, body_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A cell is "painted" by the quadrant renderer iff it has an explicit
    // Rgb foreground (solid regions render as a space with fg==bg color).
    fn cell_painted(buf: &ratatui::buffer::Buffer, x: u16, y: u16) -> bool {
        matches!(
            buf.cell((x, y)).map(|c| c.style().fg),
            Some(Some(Color::Rgb(..)))
        )
    }

    fn blit_filled_cells(area_w: u16, area_h: u16, img: &image::RgbaImage, cover: bool) -> usize {
        let backend = ratatui::backend::TestBackend::new(area_w, area_h);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            blit_image_quadrants(f, Rect::new(0, 0, area_w, area_h), img, cover, 1.0);
        })
        .expect("draw");
        let buf = term.backend().buffer();
        let mut n = 0;
        for y in 0..area_h {
            for x in 0..area_w {
                if cell_painted(buf, x, y) {
                    n += 1;
                }
            }
        }
        n
    }

    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn timeline_renders_nested_actions_and_depth() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "# Timeline demo\n\n:::timeline\n- [~] [Start demo](agentd:action/start-demo?key=d)\n  - [x] Prepare demo workspace\n    - [ ] Record demo\n- [ ] [Run checks](agentd:action/run-checks?key=r)\n- Plain milestone\n:::";
        let lines = render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(10, 20, 80, 20),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            true,
        );
        let rendered: Vec<_> = lines.iter().map(line_text).collect();
        assert!(rendered
            .iter()
            .any(|line| line.contains("◉ [d] Start demo")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("│  ✓ Prepare demo workspace")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("│    ○ Record demo")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("○ [r] Run checks")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("• Plain milestone")));
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].action.id, "start-demo");
        assert_eq!(hits[0].action.key.as_deref(), Some("d"));
        assert_eq!(hits[1].action.id, "run-checks");
        assert_eq!(hits[1].action.key.as_deref(), Some("r"));
    }

    #[test]
    fn checklist_action_links_keep_list_layout_and_optional_keys() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "- [x] [Run checks](agentd:action/run-checks?key=r) and [Start demo](agentd:action/start-demo)";
        let lines = render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(0, 0, 80, 10),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            false,
        );
        assert_eq!(line_text(&lines[0]), "✓ [r] Run checks and Start demo");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].action.key.as_deref(), Some("r"));
        assert_eq!(hits[1].action.key, None);
    }

    /// Regression: an http(s) URL link inside a widget must register a
    /// `DynamicUiUrlHit` (with the URL + visible hit geometry) so the
    /// click handler can dispatch it through `open_url`. Before the URL
    /// hit list existed, these rendered as plain text and clicks were
    /// silently swallowed even though the hover underline implied they
    /// were active.
    #[test]
    fn http_link_in_widget_registers_url_hit() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "See [docs](https://example.com/x) for details.";
        render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(0, 0, 80, 10),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            false,
        );
        assert!(
            hits.is_empty(),
            "an external URL must not register as an action hit"
        );
        assert_eq!(url_hits.len(), 1);
        assert_eq!(url_hits[0].url, "https://example.com/x");
        assert_eq!(url_hits[0].session_id, "session");
        assert_eq!(url_hits[0].panel_id, "panel");
        // Hit width should cover the visible label "docs" (4 cols), not the
        // raw URL — clicks land where the text actually renders.
        assert_eq!(url_hits[0].end_col - url_hits[0].start_col, 4);
    }

    /// `http://` URLs are also clickable, and the link's hit geometry
    /// follows where the label appears in the rendered line (here the
    /// label is in a checklist item, so it's offset by the glyph and
    /// leading indent).
    #[test]
    fn http_link_in_checklist_is_clickable_and_offset_correctly() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "- [ ] visit [home](http://example.com)";
        render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(0, 0, 80, 10),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            false,
        );
        assert!(hits.is_empty());
        assert_eq!(url_hits.len(), 1);
        assert_eq!(url_hits[0].url, "http://example.com");
        // Label width "home" = 4 cols regardless of where it lands.
        assert_eq!(url_hits[0].end_col - url_hits[0].start_col, 4);
    }

    /// Mixed widgets — action links on one line, an http link in the
    /// following paragraph — must each land in their own hit list. The
    /// existing dedicated action-line branch only accepts lines that are
    /// purely action links, so a real-world widget uses separate lines.
    #[test]
    fn mixed_action_and_url_links_partition_correctly() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "[Run](agentd:action/run)\n\nSee [docs](https://example.com) for details.";
        render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(0, 0, 80, 10),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            false,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].action.id, "run");
        assert_eq!(url_hits.len(), 1);
        assert_eq!(url_hits[0].url, "https://example.com");
    }

    /// Schemes other than http(s) / agentd:action are not turned into
    /// hits — they render as plain text. This is the guard against
    /// dispatching `mailto:`, `file://`, etc., which would need explicit
    /// handling and a security review.
    #[test]
    fn non_http_link_schemes_do_not_register_url_hits() {
        let mut hits = Vec::new();
        let mut url_hits = Vec::new();
        let markdown = "[email](mailto:me@example.com) and [file](file:///etc/passwd)";
        render_agentd_markdown_lines(
            markdown,
            &Theme::default(),
            None,
            Rect::new(0, 0, 80, 10),
            Some("session"),
            Some("panel"),
            &mut hits,
            &mut url_hits,
            false,
        );
        assert!(hits.is_empty());
        assert!(url_hits.is_empty());
    }

    #[test]
    fn best_quadrant_matches_two_color_blocks() {
        let r = [200, 0, 0];
        let b = [0, 0, 200];
        let w = [255, 255, 255];
        let k = [0, 0, 0];
        // Top row one color, bottom row another → a top/bottom half block,
        // fg = top color, bg = bottom color (px order TL,TR,BL,BR).
        let (ch, fg, bg) = best_quadrant([r, r, b, b]);
        assert!(ch == "▀" || ch == "▄", "top/bottom split glyph, got {ch:?}");
        assert!((fg == r && bg == b) || (fg == b && bg == r));
        // Left/right split → vertical half block.
        let (ch, _, _) = best_quadrant([r, b, r, b]);
        assert!(ch == "▌" || ch == "▐", "left/right split glyph, got {ch:?}");
        // A single bright sub-cell → that one quadrant, exact colors.
        let (ch, fg, bg) = best_quadrant([w, k, k, k]);
        assert_eq!(ch, "▘");
        assert_eq!((fg, bg), (w, k));
        // Uniform block → a single solid color (fg == bg), zero error.
        let (_, fg, bg) = best_quadrant([r, r, r, r]);
        assert_eq!((fg, bg), (r, r));
    }

    #[test]
    fn wallpaper_cover_fills_area_contain_letterboxes() {
        // Square-ish image into a non-matching aspect area.
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]));
        // Cover fills every cell of a 5x3 area (no empty margins).
        assert_eq!(
            blit_filled_cells(5, 3, &img, true),
            15,
            "cover must fill every cell"
        );
        // A wide image fit into a square area letterboxes — some cells
        // stay empty.
        let wide = image::RgbaImage::from_pixel(16, 8, image::Rgba([30, 200, 30, 255]));
        let filled = blit_filled_cells(4, 4, &wide, false);
        assert!(
            filled > 0 && filled < 16,
            "contain should letterbox (partial fill), got {filled}/16"
        );
    }

    #[test]
    fn resized_image_memoizes_per_source_and_size() {
        let mut cache: crate::app::ImageResizeCache = Vec::new();
        let src = std::sync::Arc::new(image::RgbaImage::from_pixel(
            100,
            100,
            image::Rgba([1, 2, 3, 255]),
        ));
        let a = resized_image(&mut cache, &src, 20, 10);
        let b = resized_image(&mut cache, &src, 20, 10);
        assert!(
            std::sync::Arc::ptr_eq(&a, &b),
            "same source + size must hit the cache (no re-resize)"
        );
        assert_eq!(cache.len(), 1);
        // Different output size → distinct entry.
        let _c = resized_image(&mut cache, &src, 30, 10);
        assert_eq!(cache.len(), 2);
        // Different source image → distinct entry.
        let src2 = std::sync::Arc::new(image::RgbaImage::from_pixel(
            100,
            100,
            image::Rgba([9, 9, 9, 255]),
        ));
        let _d = resized_image(&mut cache, &src2, 20, 10);
        assert_eq!(cache.len(), 3);
        // MRU stays bounded.
        for w in 40..60 {
            let _ = resized_image(&mut cache, &src, w, 10);
        }
        assert!(
            cache.len() <= 4,
            "cache must stay bounded, got {}",
            cache.len()
        );
    }

    fn paint_rows_for(row_frac: (f32, f32)) -> Vec<bool> {
        // 4x6 cover-filled image; return per-cell-row whether it's painted.
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([200, 30, 30, 255]));
        let area = Rect::new(0, 0, 4, 6);
        let (ow, oh) = blit_scale_dims(img.dimensions(), area, true);
        let resized =
            image::imageops::resize(&img, ow * 2, oh, image::imageops::FilterType::Triangle);
        let backend = ratatui::backend::TestBackend::new(4, 6);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| paint_resized_quadrants(f, area, &resized, 1.0, row_frac))
            .expect("draw");
        let buf = term.backend().buffer();
        (0..6)
            .map(|y| (0..4).all(|x| cell_painted(buf, x, y)))
            .collect()
    }

    #[test]
    fn preview_reveal_range_freezes_erase_on_hover() {
        use std::time::{Duration, Instant};
        let now = Instant::now();
        let revealed = now - Duration::from_secs(5); // fully appeared
        let hide_soon = now + Duration::from_millis(300); // inside the erase window
        let (start, end) = preview_reveal_range(revealed, hide_soon, now, false);
        assert!(
            start > 0.0,
            "erase should be underway when not hovered: {start}"
        );
        assert!((end - 1.0).abs() < 1e-3);
        let (start_h, _) = preview_reveal_range(revealed, hide_soon, now, true);
        assert_eq!(start_h, 0.0, "hover must freeze the top-down erase");
    }

    #[test]
    fn wallpaper_appears_top_down() {
        // Appear half-done → top ~3 of 6 rows drawn, bottom blank.
        let rows = paint_rows_for((0.0, 0.5));
        assert!(rows[0] && rows[1] && rows[2], "top rows revealed: {rows:?}");
        assert!(!rows[3] && !rows[4] && !rows[5], "bottom not yet: {rows:?}");
    }

    #[test]
    fn wallpaper_erases_top_down_on_disappear() {
        // Disappear half-done → top ~3 rows erased (blank), bottom remains.
        let rows = paint_rows_for((0.5, 1.0));
        assert!(
            !rows[0] && !rows[1] && !rows[2],
            "top rows erased: {rows:?}"
        );
        assert!(
            rows[3] && rows[4] && rows[5],
            "bottom still shown: {rows:?}"
        );
    }

    #[test]
    fn wallpaper_dim_darkens_pixels() {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 200, 200, 255]));
        let backend = ratatui::backend::TestBackend::new(2, 1);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| blit_image_quadrants(f, Rect::new(0, 0, 2, 1), &img, true, 0.5))
            .expect("draw");
        let buf = term.backend().buffer();
        // 200 * 0.5 = 100, so the dimmed fg should be well below 200.
        match buf.cell((0, 0)).map(|c| c.style().fg) {
            Some(Some(Color::Rgb(r, _, _))) => {
                assert!(r <= 110, "dim should darken (~100), got {r}")
            }
            other => panic!("expected an Rgb fg cell, got {other:?}"),
        }
    }

    /// User-reported regression: zarvis emits `\x1b[2m` (DIM/faint)
    /// for markers like `[+N lines — click to expand]` and tool
    /// args. The main session view renders that as dim/gray because
    /// `tui_term::PseudoTerminal` translates the attribute itself,
    /// but the pin tile uses `render_pty_tail` which copies cells
    /// through `vt100_cell_style`. If `vt100_cell_style` doesn't
    /// emit `Modifier::DIM`, the pin tile shows the same content at
    /// full intensity — visually inconsistent with the main view.
    ///
    /// This test feeds dim bytes through a vt100 parser, looks up
    /// the resulting cell, runs it through `vt100_cell_style`, and
    /// asserts the DIM modifier is set. It also asserts that a
    /// neighboring non-dim cell does NOT have DIM, so we catch a
    /// future bug where we accidentally set DIM unconditionally.
    #[test]
    fn vt100_cell_style_preserves_dim_attribute() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        // "X" in default style, then "Y" in DIM style.
        parser.process(b"X\x1b[2mY\x1b[0m");
        let screen = parser.screen();

        let plain = screen.cell(0, 0).expect("plain cell");
        let dimmed = screen.cell(0, 1).expect("dim cell");

        let theme = Theme::default();
        let plain_style = super::vt100_cell_style(plain, &theme);
        let dimmed_style = super::vt100_cell_style(dimmed, &theme);

        assert!(
            !plain_style.add_modifier.contains(Modifier::DIM),
            "non-dim cell should not have DIM modifier"
        );
        assert!(
            dimmed_style.add_modifier.contains(Modifier::DIM),
            "dim cell must carry DIM modifier — without it the pin \
             tile renders zarvis's gray markers at full intensity"
        );
    }

    #[test]
    fn vt100_cell_style_maps_grayscale_foreground_to_theme_green() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"\x1b[97mW\x1b[90mD\x1b[0m");
        let screen = parser.screen();
        let theme = Theme::default();

        let white = super::vt100_cell_style(screen.cell(0, 0).expect("white cell"), &theme);
        let dark_gray = super::vt100_cell_style(screen.cell(0, 1).expect("dark gray cell"), &theme);

        assert_eq!(white.fg, Some(theme.text));
        assert_ne!(dark_gray.fg, Some(Color::DarkGray));
        assert_eq!(
            dark_gray.fg,
            Some(super::green_for_luma(&theme, 102)),
            "ANSI dark gray should become the corresponding Matrix green"
        );
    }

    #[test]
    fn split_list_pane_reserves_matrix_height_so_list_scrolls_when_items_overflow() {
        // Tall pane: items would overflow but matrix should still be
        // anchored at the default 12 rows at the bottom, and the list
        // gets the remainder (which is what makes ratatui's List
        // widget scroll to keep the selection in view).
        let inner = Rect::new(0, 0, 20, 20);
        let (list, matrix) = split_list_pane(inner, false, None);
        assert_eq!(matrix.height, crate::app::MATRIX_RAIN_H_DEFAULT);
        assert_eq!(list.height, 20 - crate::app::MATRIX_RAIN_H_DEFAULT);
        assert_eq!(matrix.y, list.y + list.height);
        assert_eq!(list.x, inner.x);
        assert_eq!(matrix.x, inner.x);
    }

    #[test]
    fn split_list_pane_falls_back_to_full_list_when_height_too_short() {
        // SESSION_LIST_H_MIN (3) + MATRIX_RAIN_H_MIN (4) = 7. With
        // only 6 rows of inner space we can't honor both, so list
        // takes everything and matrix is reported as a zero-height
        // rect anchored past the bottom.
        let inner = Rect::new(0, 0, 20, 6);
        let (list, matrix) = split_list_pane(inner, false, None);
        assert_eq!(list, inner);
        assert_eq!(matrix.height, 0);
    }

    #[test]
    fn split_list_pane_keeps_min_list_height_when_pane_is_tight() {
        // SESSION_LIST_H_MIN=3 + MATRIX_RAIN_H_MIN=4 = 7 inner rows.
        // The list takes exactly its minimum, matrix takes the rest.
        let inner = Rect::new(0, 0, 20, 7);
        let (list, matrix) = split_list_pane(inner, false, None);
        assert_eq!(list.height, crate::app::SESSION_LIST_H_MIN);
        assert_eq!(matrix.height, crate::app::MATRIX_RAIN_H_MIN);
        assert_eq!(matrix.y, list.y + list.height);
    }

    #[test]
    fn split_list_pane_skips_matrix_when_hidden() {
        let inner = Rect::new(0, 0, 20, 30);
        let (list, matrix) = split_list_pane(inner, true, None);
        assert_eq!(list, inner);
        assert_eq!(matrix.height, 0);
    }

    #[test]
    fn matrix_rain_panel_height_defaults_and_clamps() {
        assert_eq!(
            matrix_rain_panel_height(None, 30),
            crate::app::MATRIX_RAIN_H_DEFAULT
        );
        assert_eq!(matrix_rain_panel_height(None, 8), 8);
        assert_eq!(
            matrix_rain_panel_height(Some(2), 30),
            crate::app::MATRIX_RAIN_H_MIN
        );
        assert_eq!(matrix_rain_panel_height(Some(50), 30), 30);
        assert_eq!(matrix_rain_panel_height(Some(8), 3), 3);
    }

    #[test]
    fn matrix_rain_intensity_ramps_up_faster_than_down() {
        assert_eq!(
            eased_matrix_rain_intensity(0.0, 1.0, Duration::from_secs(5)),
            1.0
        );
        assert_eq!(
            eased_matrix_rain_intensity(1.0, 0.0, Duration::from_secs(5)),
            0.75
        );
        assert_eq!(
            eased_matrix_rain_intensity(1.0, 0.0, Duration::from_secs(20)),
            0.0
        );
    }

    #[test]
    fn rain_activity_counts_active_sessions_in_quarters() {
        assert_eq!(rain_activity_for_active_sessions(0), 0.0);
        assert_eq!(rain_activity_for_active_sessions(1), 0.25);
        assert_eq!(rain_activity_for_active_sessions(2), 0.5);
        assert_eq!(rain_activity_for_active_sessions(3), 0.75);
        assert_eq!(rain_activity_for_active_sessions(4), 1.0);
        assert_eq!(rain_activity_for_active_sessions(5), 1.0);
    }

    #[test]
    fn foreground_rain_frame_position_is_deterministic_from_epoch() {
        // The drop position is a pure function of (now - epoch),
        // seed, speed, cycle. Same inputs ⇒ same head. The two
        // calls below differ by exactly `cell_ms`, so the head
        // advances by exactly one row — never teleports.
        let epoch = Instant::now();
        let seed: u64 = 42;
        let speed: u64 = 3;
        let cycle: u16 = 20;
        let cell_ms = 58 + speed * 19;
        let a = foreground_rain_frame(
            epoch + Duration::from_millis(cell_ms * 5),
            epoch,
            seed,
            speed,
            cycle,
        );
        let b = foreground_rain_frame(
            epoch + Duration::from_millis(cell_ms * 6),
            epoch,
            seed,
            speed,
            cycle,
        );
        // Heads advance by 1 (mod cycle); never jump.
        let advance = (b.head as i32 - a.head as i32).rem_euclid(cycle as i32);
        assert_eq!(advance, 1);
    }

    #[test]
    fn foreground_rain_frame_columns_are_phase_staggered() {
        // Two columns at the same instant must not share a head —
        // their seed-derived phase offsets keep the field from
        // looking like a marching curtain at frame 0.
        let epoch = Instant::now();
        let now = epoch + Duration::from_millis(0);
        let cycle: u16 = 20;
        let mut heads = std::collections::HashSet::new();
        for col in 0u16..32 {
            let seed = hash64(col as u64 ^ ((32u64) << 24));
            let speed = 2 + (seed % 7);
            heads.insert(foreground_rain_frame(now, epoch, seed, speed, cycle).head);
        }
        assert!(
            heads.len() > 1,
            "expected staggered phases across columns, got {} distinct heads",
            heads.len()
        );
    }

    #[test]
    fn matrix_rain_tail_is_keyed_not_activity_scaled() {
        let a = matrix_rain_tail_for_key(42);
        let b = matrix_rain_tail_for_key(43);
        assert!((MATRIX_RAIN_TAIL_MIN..=MATRIX_RAIN_TAIL_MAX).contains(&a));
        assert!((MATRIX_RAIN_TAIL_MIN..=MATRIX_RAIN_TAIL_MAX).contains(&b));
        assert_eq!(matrix_rain_tail_for_key(42), a);
    }

    #[test]
    fn find_text_ranges_respects_selection_bounds() {
        let frame = vec![
            "outside match".to_string(),
            "  inside match  ".to_string(),
            "outside match".to_string(),
        ];

        let ranges = find_text_ranges(&frame, "inside", Some(Rect::new(2, 1, 12, 1)), None);

        assert_eq!(ranges, vec![(1, 2, 7)]);
        assert!(
            find_text_ranges(&frame, "outside", Some(Rect::new(2, 1, 12, 1)), None,).is_empty()
        );
    }

    #[test]
    fn find_text_ranges_prefers_duplicate_nearest_original_range() {
        let frame = vec![
            "same target".to_string(),
            "filler".to_string(),
            "same target".to_string(),
        ];
        let original = TextSelectionRange {
            start: ScreenPoint { col: 5, row: 2 },
            end: ScreenPoint { col: 10, row: 2 },
        };

        let ranges = find_text_ranges(&frame, "target", None, Some(original));

        assert_eq!(ranges, vec![(2, 5, 10)]);
    }

    #[test]
    fn translate_block_hits_shifts_clips_and_drops() {
        use crate::pty_render::BlockHitRect;
        let mk = |row_start, row_end, header_row| BlockHitRect {
            call_id: "c".into(),
            row_start,
            row_end,
            bg_button: Some((1, 5)),
            kill_button: Some((6, 10)),
            header_row,
        };

        // offset 0 is the identity.
        let out = translate_block_hits(vec![mk(2, 5, 2)], 0, 30);
        assert_eq!(
            (out[0].row_start, out[0].row_end, out[0].header_row),
            (2, 5, 2)
        );
        assert!(out[0].bg_button.is_some());

        // chat shows the bottom slice; parser is 10 rows taller than the
        // chat area (editor pane = 10 rows). A block fully above the
        // slice is dropped.
        assert!(translate_block_hits(vec![mk(3, 9, 3)], 10, 20).is_empty());

        // A block straddling the slice top is clipped: its header (above
        // the slice) is gone, so buttons drop and row_start pins to 0.
        let out = translate_block_hits(vec![mk(8, 14, 8)], 10, 20);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].row_start, out[0].row_end), (0, 4));
        assert!(out[0].bg_button.is_none() && out[0].kill_button.is_none());

        // A block fully inside the slice shifts up by the offset and
        // keeps its buttons.
        let out = translate_block_hits(vec![mk(15, 18, 15)], 10, 20);
        assert_eq!(
            (out[0].row_start, out[0].row_end, out[0].header_row),
            (5, 8, 5)
        );
        assert!(out[0].bg_button.is_some());
    }

    #[test]
    fn minibuffer_panel_height_uses_preference_and_clamps() {
        assert_eq!(
            minibuffer_panel_height(None, 40),
            crate::app::MINIBUFFER_PANEL_H_DEFAULT
        );
        assert_eq!(minibuffer_panel_height(Some(20), 40), 20);
        assert_eq!(
            minibuffer_panel_height(Some(1), 40),
            crate::app::MINIBUFFER_PANEL_H_MIN
        );
        assert_eq!(minibuffer_panel_height(Some(80), 20), 17);
    }

    #[test]
    fn editor_pane_rows_includes_completion_suggestions() {
        let state = crate::app::EditorState {
            queued: Vec::new(),
            buf: "/".to_string(),
            cursor: 1,
            completions: vec!["/help".to_string(), "/model".to_string()],
        };

        // spacer + two completion rows + active prompt row
        assert_eq!(editor_pane_rows(Some(&state), None, 80), 4);
    }

    #[test]
    fn editor_pane_themes_active_prompt_text() {
        let state = crate::app::EditorState {
            queued: Vec::new(),
            buf: "hello".to_string(),
            cursor: 5,
            completions: Vec::new(),
        };
        let theme = Theme::default();
        let backend = ratatui::backend::TestBackend::new(20, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| {
                render_editor_pane(f, Rect::new(0, 0, 20, 3), Some(&state), None, &theme, false);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let glyph_cell = buffer.cell((0, 1)).expect("glyph cell");
        let text_cell = buffer.cell((2, 1)).expect("text cell");

        assert_eq!(glyph_cell.style().fg, Some(theme.accent));
        assert_eq!(text_cell.style().fg, Some(theme.text));
    }

    #[test]
    fn editor_pane_renders_themed_prompt_cursor() {
        let state = crate::app::EditorState {
            queued: Vec::new(),
            buf: "hello".to_string(),
            cursor: 5,
            completions: Vec::new(),
        };
        let theme = Theme::default();
        let backend = ratatui::backend::TestBackend::new(20, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");

        terminal
            .draw(|f| {
                render_editor_pane(f, Rect::new(0, 0, 20, 3), Some(&state), None, &theme, true);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let cursor_cell = buffer.cell((7, 1)).expect("cursor cell");

        assert_eq!(cursor_cell.style().fg, Some(theme.highlight_fg));
        assert_eq!(cursor_cell.style().bg, Some(theme.accent));
        assert!(cursor_cell.style().add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn shrink_popup_clears_editor_area() {
        // Render a bigger editor area then a smaller one and ensure the
        // old content wouldn't persist. We can't render a real Frame in
        // unit tests easily here, but the logic is exercised by calling
        // editor_pane_rows to ensure the reserved rows decrease. The
        // actual visual clearing is done by calling `Clear` at the start
        // of `render_editor_pane`, which has been added to prevent the
        // terminal-resize-only repaint symptom.
        let big_state = crate::app::EditorState {
            queued: Vec::new(),
            buf: "/".to_string(),
            cursor: 1,
            completions: vec![
                "/help".to_string(),
                "/model".to_string(),
                "/new".to_string(),
            ],
        };
        let small_state = crate::app::EditorState {
            queued: Vec::new(),
            buf: "/".to_string(),
            cursor: 1,
            completions: vec!["/help".to_string()],
        };
        assert!(
            editor_pane_rows(Some(&big_state), None, 80)
                > editor_pane_rows(Some(&small_state), None, 80)
        );
    }

    fn summary_with_mode(harness: &str, mode: Option<&str>) -> agentd_protocol::SessionSummary {
        let mut json = serde_json::json!({
            "id": "s1",
            "harness": harness,
            "cwd": "/tmp",
            "state": "running",
            "created_at": "2026-05-20T00:00:00Z",
        });
        if let Some(m) = mode {
            json["mode"] = serde_json::json!(m);
        }
        serde_json::from_value(json).expect("valid SessionSummary")
    }

    #[test]
    fn is_headless_only_for_headless_mode() {
        assert!(is_headless(&summary_with_mode("zarvis", Some("headless"))));
        assert!(!is_headless(&summary_with_mode(
            "zarvis",
            Some("interactive")
        )));
        // Missing mode is treated as not-headless (older sessions
        // persisted before the mode fix, and PTY harnesses).
        assert!(!is_headless(&summary_with_mode("shell", None)));
    }

    #[test]
    fn harness_label_prefixes_headless() {
        // Headless sessions get a "(headless) " prefix so the list /
        // title bar visibly distinguish them from interactive ones.
        assert_eq!(
            harness_label(&summary_with_mode("zarvis", Some("headless"))),
            "(headless) zarvis"
        );
        // Interactive and mode-less sessions render the bare harness.
        assert_eq!(
            harness_label(&summary_with_mode("zarvis", Some("interactive"))),
            "zarvis"
        );
        assert_eq!(harness_label(&summary_with_mode("shell", None)), "shell");
    }

    #[test]
    fn zarvis_running_animates_only_while_agent_active() {
        let mut s = summary_with_mode("zarvis", Some("interactive"));
        s.state = SessionState::Running;
        // Mid-turn: agent active → animate, even with no recent PTY bytes.
        assert!(session_should_animate_status(&s, false, true));
        // Running but the turn has ended (agent inactive) → stay static,
        // even though the lifecycle state still reads Running. This is the
        // idle-zarvis regression: PR #179 spun the glyph here.
        assert!(!session_should_animate_status(&s, false, false));
        assert!(!session_should_animate_status(&s, true, false));
    }

    #[test]
    fn shell_running_status_uses_pty_activity_gate() {
        let mut s = summary_with_mode("shell", None);
        s.state = SessionState::Running;
        // Shell has no agent-status signal; gate on recent PTY bytes
        // (agent_active is irrelevant for non-zarvis harnesses).
        assert!(!session_should_animate_status(&s, false, false));
        assert!(session_should_animate_status(&s, true, false));
    }

    #[test]
    fn awaiting_input_status_stays_static() {
        let mut s = summary_with_mode("zarvis", Some("interactive"));
        s.state = SessionState::AwaitingInput;
        // Not Running → never animates, regardless of activity signals.
        assert!(!session_should_animate_status(&s, true, true));
    }

    fn widget(markdown: &str) -> agentd_protocol::UiPanel {
        agentd_protocol::UiPanel {
            id: "w".into(),
            source: None,
            title: None,
            placement: agentd_protocol::UiPlacement::Inline,
            markdown: markdown.into(),
        }
    }

    #[test]
    fn inline_widget_rows_floors_at_three() {
        // Empty markdown still gets the minimum row budget (room for the
        // top + bottom borders plus at least one content line, so an empty
        // widget doesn't render as just a single fused border).
        let panel = widget("");
        let h = inline_widget_rows(&panel, 40, 50, &Theme::default());
        assert_eq!(h, 3);
    }

    #[test]
    fn inline_widget_rows_accounts_for_wrapping_long_lines() {
        // A single source line that wraps to multiple terminal rows must
        // grow the panel: the old source-line count returned the floor (3)
        // here and the wrapped content got clipped.
        let long = "x".repeat(200);
        let panel = widget(&long);
        let theme = Theme::default();
        let narrow = inline_widget_rows(&panel, 40, 50, &theme);
        let wide = inline_widget_rows(&panel, 220, 50, &theme);
        assert!(
            narrow > wide,
            "narrow panel should need more rows (wrapping): narrow={narrow} wide={wide}"
        );
        assert!(narrow > 3, "200-char line at width 40 should exceed floor");
    }

    #[test]
    fn inline_widget_rows_caps_at_available_height() {
        let huge = "line\n".repeat(500);
        let panel = widget(&huge);
        let h = inline_widget_rows(&panel, 40, 12, &Theme::default());
        assert_eq!(h, 12, "must never exceed available_height");
    }
}
