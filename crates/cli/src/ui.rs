//! Ratatui rendering for the TUI.

use crate::app::{
    App, HarnessHit, HintZone, ListItem as AppListItem, Minibuffer, MinibufferIntent, PaneFocus,
    ScreenPoint, Selection, TextSelectionRange, ViewMode, ZoomMode,
};
use crate::keymap::KeyAction;
use agentd_protocol::{MessageRole, SessionEvent, SessionState, SessionSummary, TimestampedEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

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
        render_help(f, area);
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
    let style = Style::default().bg(Color::Blue).fg(Color::White);
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
fn render_button_tooltip(f: &mut Frame, label: &str, anchor_x: u16, anchor_y: u16) {
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
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));
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
            render_button_tooltip(f, " New session ", xs, y);
            return;
        }
    }
    if let Some((xs, xe, y)) = list_collapse_button_range(list) {
        if my == y && mx >= xs && mx < xe {
            render_button_tooltip(f, " Collapse list ", xs, y);
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
        render_button_tooltip(f, " Expand list ", gx, gy);
    }
}

fn render_view_uncollapse_glyph(f: &mut Frame, _app: &App, view_area: Rect) {
    let (gx, gy) = view_uncollapse_glyph_pos(view_area);
    let style = Style::default()
        .fg(Color::White)
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
    let overlay_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
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
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));
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
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));
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
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));
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
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);
    let hover_available = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let base_disabled = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::CROSSED_OUT);
    let hover_disabled = Style::default()
        .fg(Color::Red)
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
        spans.push(Span::styled(err.clone(), Style::default().fg(Color::Red)));
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
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Yellow)
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
        .border_style(Style::default().fg(Color::DarkGray));
    let p = Paragraph::new(label)
        .block(block)
        .style(Style::default().fg(Color::White).bg(Color::Black));
    f.render_widget(Clear, rect);
    f.render_widget(p, rect);
}

fn pin_strip_height(total_h: u16) -> u16 {
    (total_h / 3).clamp(7, 18)
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
        render_help(f, area);
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
    app.layout.minibuffer_area = Some(minibuffer_area);
    app.layout.list_row_count = app.list_items().len();

    render_sessions(f, main_area, app);
    render_minibuffer(f, minibuffer_area, app);
    if app.help_visible {
        render_help(f, area);
    }
}

fn render_sessions(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == PaneFocus::List;
    // Collapsed render path: a thin column with a `>` expand glyph
    // on the top border. Anywhere inside the pane click-expands.
    let effective_collapsed = app.list_collapsed && !focused;
    if effective_collapsed {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(focused))
            .title(Line::from(Span::styled(
                "›",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )));
        f.render_widget(block, area);
        return;
    }
    // Expanded render path: title is ` + sessions ` with a
    // right-aligned ` − ` for collapse. Both are clickable; the
    // click handler in `App::click_list` consults
    // `list_title_button_hit` for the geometry.
    let plus_style = Style::default()
        .fg(Color::White)
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
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    let collapse_line =
        Line::from(Span::styled(" − ", minus_style)).alignment(ratatui::layout::Alignment::Right);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title_line)
        .title(collapse_line);

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
                        Span::styled(pin_glyph.to_string(), Style::default().fg(Color::Yellow)),
                        Span::styled(
                            format!(" {} ", session_status_glyph(app, s)),
                            state_style(s.state),
                        ),
                        Span::styled(name_display, Style::default().fg(Color::White)),
                        Span::raw(gap_str),
                        Span::styled(harness.to_string(), Style::default().fg(Color::Cyan)),
                    ]))
                }
                AppListItem::GroupHeader {
                    group,
                    member_count,
                } => {
                    let glyph = if group.collapsed { "▶" } else { "▼" };
                    ListItem::new(Line::from(vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(Color::Magenta)),
                        Span::styled(
                            group.name.clone(),
                            Style::default()
                                .fg(Color::Magenta)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            format!("({member_count})"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                }
            }
        })
        .collect();

    let highlight_style = if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().bg(Color::DarkGray).fg(Color::White)
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
}

fn render_detail(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == PaneFocus::View;
    if let Some(diff) = &app.last_diff {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(focused))
            .title(" diff (ESC clears; press d to refresh) ");
        let para = Paragraph::new(diff.clone())
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
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
            pane_border_style(focused),
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
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let close =
        Line::from(Span::styled(" x ", close_style)).alignment(ratatui::layout::Alignment::Right);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
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

    if let Some(g) = app.selected_group() {
        render_group_overview(f, inner, app, g);
        return;
    }
    match app.view {
        ViewMode::Terminal => render_terminal(f, inner, app),
        ViewMode::Transcript => render_transcript(f, inner, app),
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
        format!("Group: {}", group.name),
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
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
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for s in &members {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {} ", session_status_glyph(app, s)),
                    state_style(s.state),
                ),
                Span::raw(primary_label(s)),
                Span::raw("  "),
                Span::styled(s.harness.clone(), Style::default().fg(Color::Cyan)),
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
    let (chat_area, editor_area) = if let Some(es) = &editor_state {
        // Each queued entry may itself be multi-line — sum the line
        // counts so a 3-line queued thought reserves 3 rows.
        let queued_lines: usize = es.queued.iter().map(|s| s.split('\n').count().max(1)).sum();
        let buf_lines = es.buf.lines().count().max(1);
        let raw_rows = queued_lines + 1 + buf_lines;
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
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(hint, chat_area);
            if let (Some(area), Some(es)) = (editor_area, editor_state.as_ref()) {
                render_editor_pane(f, area, es, true);
            }
            return;
        }
    };
    let out = history.replay(chat_area.width, chat_area.height, scroll);
    // Hide the chat pane's cursor block if we have our own editor pane
    // — otherwise the chat's vt100 cursor would render as a stray
    // block. For non-editor-pane sessions (claude / codex / shell)
    // keep the cursor visible so users see where their typing lands.
    let term = if editor_area.is_some() {
        let no_cursor = tui_term::widget::Cursor::default().visibility(false);
        tui_term::widget::PseudoTerminal::new(out.screen).cursor(no_cursor)
    } else {
        tui_term::widget::PseudoTerminal::new(out.screen)
    };
    f.render_widget(term, chat_area);
    app.block_hits.insert(id, out.blocks);
    if let (Some(area), Some(es)) = (editor_area, editor_state.as_ref()) {
        render_editor_pane(f, area, es, true);
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
    state: &crate::app::EditorState,
    set_cursor: bool,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let queued_style = Style::default().fg(Color::DarkGray);
    let queued_glyph_style = queued_style.add_modifier(Modifier::BOLD);
    let active_glyph_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let prompt_w: u16 = 2;

    let total_rows = area.height as usize;
    let mut y = area.y;
    let mut remaining = total_rows;

    // Queued entries — one `❯` per entry; continuation lines align
    // under the prompt's text column with a two-space indent.
    'queued: for entry in &state.queued {
        let mut first = true;
        for line in entry.split('\n') {
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
                    Span::styled(line.to_string(), queued_style),
                ]
            } else {
                vec![
                    Span::raw("  "),
                    Span::styled(line.to_string(), queued_style),
                ]
            };
            f.render_widget(Paragraph::new(Line::from(spans)), row);
            y = y.saturating_add(1);
            remaining -= 1;
        }
    }

    // Spacer row above the active prompt — visual breathing room.
    if remaining > 1 {
        y = y.saturating_add(1);
        remaining -= 1;
    }

    // Active editor — possibly multi-line.
    let buf_lines: Vec<&str> = if state.buf.is_empty() {
        vec![""]
    } else {
        state.buf.split('\n').collect()
    };
    let mut cursor_pos: Option<(u16, u16)> = None;
    let mut char_seen = 0usize;
    for (i, line) in buf_lines.iter().enumerate().take(remaining) {
        let row = Rect {
            x: area.x,
            y,
            width: area.width,
            height: 1,
        };
        let para = if i == 0 {
            Paragraph::new(Line::from(vec![
                Span::styled("❯ ", active_glyph_style),
                Span::raw(line.to_string()),
            ]))
        } else {
            Paragraph::new(Line::from(vec![
                Span::raw("  "), // align with prompt width
                Span::raw(line.to_string()),
            ]))
        };
        f.render_widget(para, row);
        let line_chars = line.chars().count();
        if cursor_pos.is_none()
            && state.cursor >= char_seen
            && state.cursor <= char_seen + line_chars
        {
            let col = (state.cursor - char_seen) as u16;
            let x = area
                .x
                .saturating_add(prompt_w)
                .saturating_add(col)
                .min(area.x + area.width.saturating_sub(1));
            cursor_pos = Some((x, y));
        }
        char_seen += line_chars + 1; // +1 for the `\n`
        y = y.saturating_add(1);
    }
    if set_cursor {
        if let Some(pos) = cursor_pos {
            f.set_cursor_position(pos);
        }
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
        .map(format_event)
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
    let para =
        Paragraph::new(modeline).style(Style::default().bg(Color::DarkGray).fg(Color::White));
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
            spans.push(Span::styled(err.clone(), Style::default().fg(Color::Red)));
        }
        let para = Paragraph::new(Line::from(spans));
        f.render_widget(para, area);
        let x = area.x + mb.prompt.width() as u16 + mb.cursor as u16;
        f.set_cursor_position(Position { x, y: area.y });
        return;
    }
    if app.help_visible {
        let para = Paragraph::new("").style(Style::default().fg(Color::DarkGray));
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
    let base_style = Style::default().fg(Color::DarkGray);
    let hover_style = Style::default()
        .fg(Color::White)
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

fn render_help(f: &mut Frame, area: Rect) {
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
        .padding(ratatui::widgets::Padding::new(2, 2, 1, 1))
        .title(" help (any key to close) ");
    let para = Paragraph::new(HELP_TEXT)
        .block(block)
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
                    palette commands: new send delete rename diff
                                      zoom interrupt refresh harnesses help
    ?               toggle this help
    C-x C-c / q     quit

When the right pane is showing a PTY-backed session (shell / interactive
claude / interactive codex) and focus is on the view, keystrokes go to the
child. `C-x` is the escape prefix — start any `C-x …` chord above to run
an agentd command without changing focus.
";

fn format_event(ev: &TimestampedEvent) -> Line<'static> {
    let ts = ev.at.format("%H:%M:%S").to_string();
    let mut spans = vec![Span::styled(
        format!("[{ts}] "),
        Style::default().fg(Color::DarkGray),
    )];
    spans.extend(format_event_body(&ev.event));
    Line::from(spans)
}

fn format_event_body(ev: &SessionEvent) -> Vec<Span<'static>> {
    match ev {
        SessionEvent::Message { role, text } => {
            let role_label = match role {
                MessageRole::User => "user",
                MessageRole::Assistant => "agent",
                MessageRole::System => "system",
                MessageRole::Tool => "tool",
            };
            vec![
                Span::styled(format!("{role_label:>7}: "), role_style(*role)),
                Span::raw(text.clone()),
            ]
        }
        SessionEvent::ToolUse { tool, args } => {
            let args_s = serde_json::to_string(args).unwrap_or_default();
            vec![
                Span::styled("   tool: ", Style::default().fg(Color::Yellow)),
                Span::raw(format!("{tool}({})", shorten(&args_s, 120))),
            ]
        }
        SessionEvent::ToolResult { tool, ok, output } => {
            let (mark, color) = if *ok {
                (" ✓ ", Color::Green)
            } else {
                (" ✗ ", Color::Red)
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
                Style::default().fg(Color::Magenta),
            )]
        }
        SessionEvent::Status { state, detail } => {
            let d = detail.clone().unwrap_or_default();
            vec![Span::styled(
                format!("   ⟳ {} {}", state.label(), d),
                Style::default().fg(Color::Blue),
            )]
        }
        SessionEvent::Cost {
            usd,
            tokens_in,
            tokens_out,
        } => vec![Span::styled(
            format!("   $ ${:.4} (in={} out={})", usd, tokens_in, tokens_out),
            Style::default().fg(Color::DarkGray),
        )],
        SessionEvent::Diff { patch } => vec![Span::raw(format!("   Δ {}", shorten(patch, 200)))],
        SessionEvent::Error { message } => vec![Span::styled(
            format!("   ! {message}"),
            Style::default().fg(Color::Red),
        )],
        SessionEvent::Reset => vec![Span::styled(
            "   ↺ session reset".to_string(),
            Style::default().fg(Color::DarkGray),
        )],
        SessionEvent::Done { exit_code } => vec![Span::styled(
            format!("   ▢ done (exit {exit_code})"),
            Style::default().fg(Color::Green),
        )],
        SessionEvent::Pty { data } => vec![Span::styled(
            format!("   ⌷ pty: {} bytes (switch to terminal view)", data.len()),
            Style::default().fg(Color::DarkGray),
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
                Style::default().fg(Color::Yellow),
            )]
        }
        // Task-lifecycle events are bookkeeping; the daemon tracks
        // them in its per-session registry. The transcript already
        // shows the matching ToolUse / ToolResult, so render these
        // minimally (or hide entirely).
        SessionEvent::TaskStart { tool, .. } => vec![Span::styled(
            format!("   ⏵ task start: {tool}"),
            Style::default().fg(Color::DarkGray),
        )],
        SessionEvent::TaskBackgrounded { .. } => vec![Span::styled(
            "   ↳ task backgrounded".to_string(),
            Style::default().fg(Color::DarkGray),
        )],
        SessionEvent::TaskEnd { ok, .. } => {
            let glyph = if *ok { "✓" } else { "✗" };
            vec![Span::styled(
                format!("   {glyph} task end"),
                Style::default().fg(Color::DarkGray),
            )]
        }
        SessionEvent::EditorState { .. } => {
            // Editor state is rendered by the input pane, not the
            // chat transcript.
            vec![]
        }
    }
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
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
                pane_border_style(is_selected),
            ))
            .alignment(ratatui::layout::Alignment::Right)
        });
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(is_selected))
            .title(title);
        if let Some(h) = harness_right {
            block = block.title(h);
        }
        let inner = block.inner(*tile_area);
        f.render_widget(block, *tile_area);
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
            render_pty_tail(f, inner, out.screen);
        } else {
            // No PTY data yet — show a placeholder.
            let p = Paragraph::new("(no data yet)").style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, inner);
        }
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
fn render_pty_tail(f: &mut Frame, area: Rect, screen: &vt100::Screen) {
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
                let contents = cell.contents();
                if contents.is_empty() {
                    buf_cell.set_char(' ');
                } else {
                    buf_cell.set_symbol(&contents);
                }
                buf_cell.set_style(vt100_cell_style(cell));
            }
        }
    }
}

fn vt100_cell_style(cell: &vt100::Cell) -> Style {
    let mut s = Style::default();
    if let Some(c) = vt100_color(cell.fgcolor()) {
        s = s.fg(c);
    }
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

fn state_style(state: SessionState) -> Style {
    match state {
        SessionState::Pending => Style::default().fg(Color::Gray),
        SessionState::Running => Style::default().fg(Color::Green),
        SessionState::AwaitingInput => Style::default().fg(Color::Green),
        SessionState::Paused => Style::default().fg(Color::Yellow),
        SessionState::Done => Style::default().fg(Color::Cyan),
        SessionState::Errored => Style::default().fg(Color::Red),
    }
}

fn role_style(role: MessageRole) -> Style {
    match role {
        MessageRole::User => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
        MessageRole::Assistant => Style::default().fg(Color::LightGreen),
        MessageRole::System => Style::default().fg(Color::DarkGray),
        MessageRole::Tool => Style::default().fg(Color::Yellow),
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
        } => {
            format!(
                "editor: q={} buf={}b cur={}",
                queued.len(),
                buf.len(),
                cursor
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
        .border_style(Style::default().fg(Color::DarkGray));
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
    let (chat_area, editor_area) = if let Some(es) = &editor_state {
        let queued_lines: usize = es.queued.iter().map(|s| s.split('\n').count().max(1)).sum();
        let buf_lines = es.buf.lines().count().max(1);
        let raw_rows = queued_lines + 1 + buf_lines;
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
    let term = if editor_area.is_some() {
        let no_cursor = tui_term::widget::Cursor::default().visibility(false);
        tui_term::widget::PseudoTerminal::new(out.screen).cursor(no_cursor)
    } else {
        tui_term::widget::PseudoTerminal::new(out.screen)
    };
    f.render_widget(term, chat_area);
    app.block_hits.insert(id, out.blocks);
    if let (Some(area), Some(es)) = (editor_area, editor_state.as_ref()) {
        render_editor_pane(f, area, es, true);
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
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    if popup.tasks.is_empty() {
        let p = Paragraph::new("(no tasks recorded for this session)")
            .style(Style::default().fg(Color::DarkGray));
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
            agentd_protocol::TaskState::Running => ("◐", Color::Yellow),
            agentd_protocol::TaskState::Backgrounded => ("↻", Color::Cyan),
            agentd_protocol::TaskState::Completed => ("✓", Color::Green),
            agentd_protocol::TaskState::Failed => ("✗", Color::Red),
            agentd_protocol::TaskState::Cancelled => ("⊘", Color::DarkGray),
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

        let plain_style = super::vt100_cell_style(plain);
        let dimmed_style = super::vt100_cell_style(dimmed);

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
    fn find_text_ranges_respects_selection_bounds() {
        let frame = vec![
            "outside match".to_string(),
            "  inside match  ".to_string(),
            "outside match".to_string(),
        ];

        let ranges = find_text_ranges(
            &frame,
            "inside",
            Some(Rect::new(2, 1, 12, 1)),
            None,
        );

        assert_eq!(ranges, vec![(1, 2, 7)]);
        assert!(find_text_ranges(
            &frame,
            "outside",
            Some(Rect::new(2, 1, 12, 1)),
            None,
        )
        .is_empty());
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
}
