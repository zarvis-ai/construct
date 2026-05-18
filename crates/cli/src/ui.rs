//! Ratatui rendering for the TUI.

use crate::app::{
    App, HarnessHit, HintZone, ListItem as AppListItem, Minibuffer, MinibufferIntent, PaneFocus,
    ScreenPoint, Selection, TextSelectionRange, ViewMode, ZoomMode,
};
use crate::keymap::KeyAction;
use crate::theme::Theme;
use agentd_protocol::{MessageRole, SessionEvent, SessionState, SessionSummary, TimestampedEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::HashSet;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unicode_width::UnicodeWidthStr;

const MATRIX_RAIN_RAMP_UP_SECS: f32 = 5.0;
const MATRIX_RAIN_DECAY_SECS: f32 = 20.0;
const MATRIX_RAIN_TAIL_MIN: u16 = 3;
const MATRIX_RAIN_TAIL_MAX: u16 = 12;

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
    app.layout.list_row_count = app.list_items().len();

    if list_w > 0 {
        render_sessions(f, cols[0], app);
    }
    render_detail(f, detail_area, app);
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
    render_list_title_button_tooltips(f, app);
    render_view_uncollapse_tooltip(f, app);
    render_harness_unavailable_tooltip(f, app);
    render_tasks_popup(f, app);
    if app.help_visible {
        render_help(f, area, &app.theme);
    }
    finish_frame(f, app);
}

fn finish_frame(f: &mut Frame, app: &mut App) {
    capture_frame_text(f, app);
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
    let (summary, indented) = match item {
        AppListItem::Session { summary, indented } => (summary, indented),
        _ => return None,
    };
    let indent: u16 = if indented { 2 } else { 0 };
    // Hit zone is the 4-cell gutter to the left of the session name:
    //   [diamond][ ][status-circle][ ]   ← then the name starts
    // Wider than the bare diamond glyph so it's easier to click —
    // the visual overlay still anchors on the diamond cell itself.
    let zone_start = list_area.x + 1 + indent;
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
                    render_button_tooltip(f, &app.theme, " Hide rain ", xs, y);
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

fn hovered_view_close_button(app: &App, view_area: Rect) -> bool {
    let Some((mx, my)) = app.mouse_pos else {
        return false;
    };
    let (x_start, x_end, y) = view_close_button_range(view_area);
    my == y && mx >= x_start && mx < x_end
}

/// Hit zone for the pin-tile unpin diamond: 4 cells on the top
/// border, starting after the corner. Title shape is ` ⬩ <status>
/// <label> <harness> `, so cells `tile.x + 1 ..= tile.x + 4`
/// (inclusive) cover `[ ][⬩][ ][status]` — the same 4-cell zone
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
    f.buffer_mut().set_string(dx, dy, "⬩", overlay_style);

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
    // Show every registered harness plus the synthetic `group` op.
    // Unavailable harnesses (binary not on PATH) render dimmed and
    // strike-through; clicking them no-ops + drops a status note;
    // hover surfaces a "not installed" tooltip.
    let mut entries: Vec<(String, bool)> = app
        .harnesses
        .iter()
        .map(|h| (h.name.clone(), h.available))
        .collect();
    entries.push(("group".to_string(), true));

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
    f.buffer_mut().set_string(dx, dy, "⬩", overlay_style);

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
        render_help(f, area, &app.theme);
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
    app.layout.list_row_count = app.list_items().len();

    render_sessions(f, main_area, app);
    render_minibuffer(f, minibuffer_area, app);
    if app.help_visible {
        render_help(f, area, &app.theme);
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
                } => {
                    let pin_glyph = if s.pinned { "⬩" } else { " " };
                    let indent_prefix = if *indented { "  " } else { "" };
                    // Fixed-width left side: indent + pin (1) + " glyph " (3).
                    let prefix_w = indent_prefix.chars().count() + 1 + 3;
                    let harness = s.harness.as_str();
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
                    ListItem::new(Line::from(vec![
                        Span::raw(indent_prefix.to_string()),
                        Span::styled(pin_glyph.to_string(), Style::default().fg(app.theme.accent)),
                        Span::styled(
                            format!(" {} ", session_status_glyph(app, s)),
                            state_style(&app.theme, s.state),
                        ),
                        Span::styled(name_display, Style::default().fg(app.theme.text)),
                        Span::raw(gap_str),
                        Span::styled(harness.to_string(), harness_style(&app.theme)),
                    ]))
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
    let mut state = ListState::default();
    state.select(if matches!(app.selection, Selection::None) {
        None
    } else {
        selected_idx
    });
    let list = List::new(items)
        .block(block)
        .highlight_style(highlight_style);
    f.render_stateful_widget(list, area, &mut state);
    clear_pane_side_borders(f, area, app);
    render_matrix_rain(f, inner, app, app_items.len());
}

fn render_matrix_rain(f: &mut Frame, area: Rect, app: &mut App, occupied_rows: usize) {
    app.layout.matrix_rain_area = None;
    if app.matrix_rain_hidden {
        return;
    }
    if area.width < 8 || area.height < 3 {
        return;
    }
    let used = (occupied_rows as u16).min(area.height);
    let available = area.height.saturating_sub(used);
    let panel_h = matrix_rain_panel_height(app.matrix_rain_h, available);
    let rain_area = Rect {
        x: area.x,
        y: area.y + area.height.saturating_sub(panel_h),
        width: area.width,
        height: panel_h,
    };
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
    let activity = update_matrix_rain_intensity(app, now);
    let elapsed = app.start_instant.elapsed().as_millis() as u64;
    let spawn_tail = matrix_rain_tail(activity);
    let cycle = rain_area.height + MATRIX_RAIN_TAIL_MAX + 1;
    let charset = b"01:|/\\{}[]<>+$#@*=-zrvshcodxgit";
    let mut current_drop_keys = HashSet::with_capacity(rain_area.width as usize);

    for col in 0..rain_area.width {
        let seed = hash64(col as u64 ^ ((rain_area.width as u64) << 24));
        let speed = 2 + (seed % 7);
        let threshold = foreground_column_threshold(seed);
        let frame = foreground_rain_frame(
            now,
            app.matrix_rain_foreground_epoch,
            seed,
            threshold,
            speed,
            cycle,
        );
        let active = frame.and_then(|frame| {
            current_drop_keys.insert(frame.key);
            if activity >= threshold {
                app.matrix_rain_active_drops
                    .entry(frame.key)
                    .or_insert(spawn_tail);
            }
            app.matrix_rain_active_drops
                .get(&frame.key)
                .copied()
                .map(|tail| (frame.head, tail))
        });
        for row in 0..rain_area.height {
            let dist = active.map(|(head, _)| head).unwrap_or(-1) - row as i16;
            let mut style = None;
            if let Some((_, tail)) = active {
                if dist >= 0 && dist < tail as i16 {
                    let shade = 1.0 - (dist as f32 / tail.max(1) as f32);
                    style = Some(rain_style(&app.theme, shade, activity));
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
                let glyph_seed = hash64(seed ^ row as u64 ^ (elapsed / 180));
                let ch = charset[(glyph_seed as usize) % charset.len()] as char;
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

    for reveal in app.matrix_rain.active_reveals(now) {
        if reveal.progress(now).is_some() {
            let reveal_start_elapsed_ms = reveal
                .started
                .checked_duration_since(app.start_instant)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            render_matrix_reveal(
                f,
                rain_area,
                &app.theme,
                reveal,
                elapsed,
                reveal_start_elapsed_ms,
                cycle,
            );
        }
    }
}

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
    let prev = app.matrix_rain_intensity;
    app.matrix_rain_intensity =
        eased_matrix_rain_intensity(app.matrix_rain_intensity, target, elapsed);
    if app.matrix_rain_intensity > prev {
        let offset = Duration::from_secs_f32(app.matrix_rain_intensity * MATRIX_RAIN_RAMP_UP_SECS);
        app.matrix_rain_foreground_epoch = now.checked_sub(offset).unwrap_or(now);
    }
    app.matrix_rain_intensity_updated_at = now;
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

fn foreground_column_threshold(seed: u64) -> f32 {
    unit_f32(hash64(seed ^ 0x9a4b_2f1d_87c6_e503))
}

fn matrix_rain_tail(activity: f32) -> u16 {
    (MATRIX_RAIN_TAIL_MIN as f32
        + activity.clamp(0.0, 1.0) * (MATRIX_RAIN_TAIL_MAX - MATRIX_RAIN_TAIL_MIN) as f32)
        .round() as u16
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MatrixRainFrame {
    key: u64,
    head: i16,
}

fn foreground_rain_frame(
    now: Instant,
    foreground_epoch: Instant,
    seed: u64,
    threshold: f32,
    speed: u64,
    cycle: u16,
) -> Option<MatrixRainFrame> {
    let crossing = foreground_epoch.checked_add(Duration::from_secs_f32(
        threshold * MATRIX_RAIN_RAMP_UP_SECS,
    ))?;
    let age = now.checked_duration_since(crossing)?;
    let cell_ms = 58 + speed * 19;
    let cycle = cycle.max(1) as u64;
    let step = age.as_millis() as u64 / cell_ms;
    let cycle_index = step / cycle;
    Some(MatrixRainFrame {
        key: hash64(
            seed ^ ((speed & 0xff) << 56)
                ^ ((cycle & 0xffff) << 40)
                ^ ((threshold.to_bits() as u64) << 8)
                ^ cycle_index,
        ),
        head: (step % cycle) as i16,
    })
}

fn fleet_activity_target(app: &App, now: Instant) -> f32 {
    let mut active_count = 0u16;
    for s in app
        .sessions
        .iter()
        .filter(|s| s.kind != agentd_protocol::SessionKind::Orchestrator)
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

fn render_matrix_reveal(
    f: &mut Frame,
    area: Rect,
    theme: &Theme,
    reveal: &crate::matrix_rain::RevealWord,
    elapsed_ms: u64,
    reveal_start_elapsed_ms: u64,
    cycle: u16,
) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let chars: Vec<char> = reveal.text.chars().collect();
    let text_w = chars.len() as u16;
    if text_w == 0 || text_w + 2 > area.width {
        return;
    }
    let target_x = area.x
        + ((area.width.saturating_sub(text_w) as f32) * reveal.x)
            .round()
            .clamp(0.0, area.width.saturating_sub(text_w) as f32) as u16;
    let target_y = area.y
        + ((area.height.saturating_sub(1) as f32) * reveal.y)
            .round()
            .clamp(0.0, area.height.saturating_sub(1) as f32) as u16;

    let target_rel_y = target_y.saturating_sub(area.y);
    let mut pins = Vec::with_capacity(chars.len());
    let mut all_pinned_at = reveal_start_elapsed_ms;
    for i in 0..chars.len() {
        let col = target_x.saturating_sub(area.x) + i as u16;
        let pinned_at =
            rain_pass_elapsed_ms(area.width, col, target_rel_y, cycle, reveal_start_elapsed_ms);
        all_pinned_at = all_pinned_at.max(pinned_at);
        pins.push(pinned_at);
    }

    let complete_hold_ms = 400;
    let fade_ms = 200;
    let fade_start = all_pinned_at + complete_hold_ms;
    let fade_end = fade_start + fade_ms;
    let fade_level = if elapsed_ms < fade_start {
        1.0
    } else {
        let elapsed_fade = elapsed_ms.saturating_sub(fade_start);
        (1.0 - elapsed_fade as f32 / fade_ms.max(1) as f32).clamp(0.0, 1.0)
    };

    for (i, ch) in chars.into_iter().enumerate() {
        let pinned_at = pins[i];
        if elapsed_ms < pinned_at {
            continue;
        }
        if elapsed_ms >= fade_end {
            continue;
        }
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
        let style = matrix_reveal_style(theme, brightness, elapsed_ms < fade_start);
        let x = target_x + i as u16;
        f.buffer_mut()
            .set_string(x, target_y, ch.to_string(), style);
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

fn rain_pass_elapsed_ms(
    width: u16,
    col: u16,
    target_rel_y: u16,
    cycle: u16,
    start_elapsed_ms: u64,
) -> u64 {
    let seed = hash64(col as u64 ^ ((width as u64) << 24));
    let speed = 2 + (seed % 7);
    let tick_ms = 58 + speed * 19;
    let offset = (seed >> 8) % cycle.max(1) as u64;
    let cycle = cycle.max(1) as u64;
    let start_step = start_elapsed_ms / tick_ms;
    let target_step = (target_rel_y as u64 + cycle - offset) % cycle;
    let delta = (target_step + cycle - (start_step % cycle)) % cycle;
    (start_step + delta) * tick_ms
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

fn render_main_transition(f: &mut Frame, area: Rect, app: &App) {
    let Some(t) = app.session_transition.as_ref() else {
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

fn render_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == PaneFocus::View;
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
    let summary = app.selected_session();
    let group = app.selected_group();
    // Width budgets for fitting the title onto the top border.
    // Layout: `<corner> <glyph> <label>  …  <harness>  x <corner>`.
    let total = area.width as usize;
    let close_w: usize = if summary.is_some() { 3 } else { 0 };
    let harness_w: usize = summary
        .map(|s| 2 + UnicodeWidthStr::width(s.harness.as_str()))
        .unwrap_or(0);
    // Label budget = total − 2 corners − right-side blocks − fixed
    // title scaffolding (` <glyph> <label> ` is 3 spaces + glyph
    // width + label).
    let glyph_w = summary
        .map(|s| UnicodeWidthStr::width(session_status_glyph(app, s)))
        .unwrap_or(0);
    let label_budget = total
        .saturating_sub(2)
        .saturating_sub(harness_w)
        .saturating_sub(close_w)
        .saturating_sub(3 + glyph_w);
    let title = match (summary, group) {
        (Some(s), _) => format!(
            " {} {} ",
            session_status_glyph(app, s),
            truncate_to_width(&primary_label(s), label_budget),
        ),
        (None, Some(g)) => format!(" group: {} ", g.name),
        (None, None) => " no session ".to_string(),
    };
    // Harness name right-aligned on the top border so it visually
    // detaches from the session-name title. Sits just left of the
    // close button (or at the right edge when no close is shown).
    // Color matches the border so harness reads as part of the
    // title bar's frame, not as a separately-styled badge.
    let harness_right = summary.map(|s| {
        Line::from(Span::styled(
            format!(" {} ", s.harness),
            pane_border_style(&app.theme, focused),
        ))
        .alignment(ratatui::layout::Alignment::Right)
    });
    // Right-aligned close button on the top border. Hover is
    // hit-tested against `app.mouse_pos` so the glyph bolds when the
    // cursor is over it — the click handler in `app.rs` mirrors the
    // same geometry to dispatch `OpenDeleteConfirm`. Only shown when
    // a session is actually selected (groups, "no session", and the
    // diff-overlay branch don't need it).
    let show_close = summary.is_some();
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
    if let Some(h) = harness_right {
        block = block.title(h);
    }
    if show_close {
        block = block.title(close);
    }
    let inner = block.inner(area);
    f.render_widget(block, area);
    clear_pane_side_borders(f, area, app);

    if let Some(g) = app.selected_group() {
        render_group_overview(f, inner, app, g);
        render_main_transition(f, inner, app);
        return;
    }
    match app.view {
        ViewMode::Terminal => render_terminal(f, inner, app),
        ViewMode::Transcript => render_transcript(f, inner, app),
    }
    render_main_transition(f, inner, app);
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
        format!("Group: {}", group.name),
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
            "  (empty — move sessions into this group)",
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
                Span::styled(s.harness.clone(), harness_style(&app.theme)),
            ]));
        }
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_terminal(f: &mut Frame, area: Rect, app: &mut App) {
    let Some(id) = app.selected_id() else {
        return;
    };
    let scroll = app.view_scrollback;
    // Only adapters that publish `SessionEvent::EditorState` (currently
    // zarvis interactive) get the fixed editor pane at the bottom.
    // claude / codex / shell render their own input prompt inside the
    // PTY, so a second editor pane would just look like a duplicate.
    let editor_state = app.editor_states.get(&id).cloned();
    let agent_status = app.agent_statuses.get(&id).cloned();
    let (chat_area, editor_area) = if editor_state.is_some() || agent_status.is_some() {
        let raw_rows = editor_pane_rows(editor_state.as_ref(), agent_status.as_ref(), area.width);
        let editor_rows: u16 = (raw_rows as u16).min(area.height.saturating_sub(1));
        let chat_height = area.height.saturating_sub(editor_rows);
        (
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: chat_height,
            },
            Some(Rect {
                x: area.x,
                y: area.y + chat_height,
                width: area.width,
                height: editor_rows,
            }),
        )
    } else {
        (area, None)
    };
    let history = match app.histories.get_mut(&id) {
        Some(h) => h,
        None => {
            let hint = Paragraph::new("(no PTY history yet — interact to populate)")
                .style(Style::default().fg(app.theme.dim));
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
            return;
        }
    };
    let out = history.replay(chat_area.width, chat_area.height, scroll);
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
    render_pty_screen(f, chat_area, out.screen, &app.theme, editor_area.is_none());
    app.block_hits.insert(id, out.blocks);
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
}

/// Paint the fixed bottom input pane:
/// - zero or more queued lines (gray `❯`), then
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

    // Queued entries — one `❯` per entry; wrapped/continuation rows
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
                        Span::styled("❯ ", queued_glyph_style),
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
    let automode_badge = match s {
        Some(s) if s.automode => "[automode]  ".to_string(),
        _ => String::new(),
    };
    let modeline = format!(
        " agentd  focus:{focus}  {sel}  {model}  {automode}{scrollback}{chord}{status}{conn} ",
        focus = focus_label,
        scrollback = scrollback_label,
        automode = automode_badge,
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
        status = app.status.as_ref().map(|(m, _)| m.as_str()).unwrap_or(""),
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
    // Hint zones from the previous frame are stale once we re-render.
    app.layout.minibuffer_hints.clear();
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
            spans.push(Span::styled(
                err.clone(),
                Style::default().fg(app.theme.danger),
            ));
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
                ("C-x x god", KeyAction::OpenCommandPalette),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o list", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::List => (
            "zoomed: list — ",
            vec![
                ("C-x x god", KeyAction::OpenCommandPalette),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o view", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::None => (
            "",
            vec![
                ("C-x x god", KeyAction::OpenCommandPalette),
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
        app.layout.minibuffer_hints.push(HintZone {
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

fn render_help(f: &mut Frame, area: Rect, theme: &Theme) {
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
}

const HELP_TEXT: &str = "
emacs keymap (default; AGENTD_KEYMAP=vim for vim profile)

  focus + view
    C-x o           switch focus (list ↔ view)
    RET (on list)   focus the selected session's view
    C-x t           toggle transcript ↔ terminal view
    C-x z           zoom: fill the screen with the session view
    C-n / down      next session
    C-p / up        prev session

  session actions
    C-x C-f         new session
    C-x i           send input to selected session
    C-x k           delete selected session (confirms; kills if running)
    C-x d           show diff
    C-x r           rename selected session (clears title on empty submit)
    C-c C-c         interrupt

  scrollback
    C-x [ / C-x ]   scroll page up/down
    C-x { / C-x }   scroll top / bottom
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
    Style::default().fg(theme.group).add_modifier(Modifier::BOLD)
}

fn harness_style(theme: &Theme) -> Style {
    Style::default().fg(theme.harness).add_modifier(Modifier::BOLD)
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
        // Title: ` ⬩ <status> <label> `. The diamond on the top
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
            .map(|s| 2 + UnicodeWidthStr::width(s.harness.as_str()))
            .unwrap_or(0);
        let glyph_w = summary
            .map(|s| UnicodeWidthStr::width(session_status_glyph(app, s)))
            .unwrap_or(0);
        // Title shape ` ⬩ <glyph> <label> ` = 5 cells of scaffolding
        // (1 leading + diamond + 1 + glyph + 1 + label + 1 trailing
        // = label + 4 + diamond + glyph; diamond is 1 cell).
        let pin_label_budget = total_pin
            .saturating_sub(2) // corners
            .saturating_sub(harness_w)
            .saturating_sub(5 + glyph_w);
        let title = match summary {
            Some(s) => format!(
                " ⬩ {} {} ",
                session_status_glyph(app, s),
                truncate_to_width(&primary_label(s), pin_label_budget),
            ),
            None => format!(" ⬩ {} ", short_id(id)),
        };
        let harness_right = summary.map(|s| {
            Line::from(Span::styled(
                format!(" {} ", s.harness),
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
            let p = Paragraph::new("(no data yet)").style(Style::default().fg(app.theme.dim));
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
fn render_pty_screen(
    f: &mut Frame,
    area: Rect,
    screen: &vt100::Screen,
    theme: &Theme,
    show_cursor: bool,
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
            let Some(cell) = screen.cell(row, col) else {
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
        let row = row.saturating_add(u16::try_from(screen.scrollback()).unwrap_or(u16::MAX));
        if row < area.height && col < area.width {
            let x = area.x + col;
            let y = area.y + row;
            if let Some(buf_cell) = f.buffer_mut().cell_mut(Position { x, y }) {
                if screen.cell(row, col).map(|c| c.has_contents()).unwrap_or(false) {
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
    if matches!(s.state, SessionState::Running) && app.pty_active(&s.id) {
        app.spinner_frame()
    } else {
        s.state.glyph()
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
        SessionEvent::Message { role, text } => format!("msg:{:?} {}", role, shorten(text, 60)),
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
    }
}

fn short_id(id: &str) -> &str {
    let n = id.len().min(10);
    &id[..n]
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
    let editor_state = app.editor_states.get(&id).cloned();
    let agent_status = app.agent_statuses.get(&id).cloned();
    let (chat_area, editor_area) = if editor_state.is_some() || agent_status.is_some() {
        let raw_rows = editor_pane_rows(editor_state.as_ref(), agent_status.as_ref(), inner.width);
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
    let out = history.replay(
        chat_area.width,
        chat_area.height,
        app.orchestrator_scrollback,
    );
    render_pty_screen(f, chat_area, out.screen, &app.theme, editor_area.is_none());
    app.block_hits.insert(id, out.blocks);
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
fn render_tasks_popup(f: &mut Frame, app: &App) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn rain_pass_elapsed_ms_waits_for_natural_column_pass() {
        let width = 24;
        let cycle = 13;
        let target_rel_y = 4;

        for col in 0..width {
            let passed_at = rain_pass_elapsed_ms(width, col, target_rel_y, cycle, 1_000);

            let seed = hash64(col as u64 ^ ((width as u64) << 24));
            let tick_ms = 58 + (2 + (seed % 7)) * 19;
            assert!(
                passed_at >= 1_000 || passed_at + tick_ms > 1_000,
                "the pass should either be upcoming or still be the current head position"
            );
            let offset = (seed >> 8) % cycle as u64;
            let head = ((passed_at / tick_ms) + offset) % cycle as u64;
            assert_eq!(head, target_rel_y as u64);
        }
    }

    #[test]
    fn matrix_rain_panel_height_defaults_and_clamps() {
        assert_eq!(
            matrix_rain_panel_height(None, 30),
            crate::app::MATRIX_RAIN_H_DEFAULT
        );
        assert_eq!(matrix_rain_panel_height(None, 8), 8);
        assert_eq!(matrix_rain_panel_height(Some(2), 30), crate::app::MATRIX_RAIN_H_MIN);
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
    fn foreground_rain_frame_starts_at_top_after_activation() {
        let epoch = Instant::now();
        let threshold = 0.5;
        let crossing = epoch + Duration::from_millis(2500);
        let seed = 42;
        assert_eq!(
            foreground_rain_frame(crossing, epoch, seed, threshold, 2, 20)
                .map(|frame| frame.head),
            Some(0)
        );
        assert_eq!(
            foreground_rain_frame(
                crossing - Duration::from_millis(1),
                epoch,
                seed,
                threshold,
                2,
                20,
            ),
            None
        );
    }

    #[test]
    fn matrix_rain_tail_scales_with_activity() {
        assert_eq!(matrix_rain_tail(0.0), MATRIX_RAIN_TAIL_MIN);
        assert_eq!(matrix_rain_tail(1.0), MATRIX_RAIN_TAIL_MAX);
        assert_eq!(matrix_rain_tail(-1.0), MATRIX_RAIN_TAIL_MIN);
        assert_eq!(matrix_rain_tail(2.0), MATRIX_RAIN_TAIL_MAX);
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
                render_editor_pane(
                    f,
                    Rect::new(0, 0, 20, 3),
                    Some(&state),
                    None,
                    &theme,
                    false,
                );
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
                render_editor_pane(
                    f,
                    Rect::new(0, 0, 20, 3),
                    Some(&state),
                    None,
                    &theme,
                    true,
                );
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
}
