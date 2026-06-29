//! Ratatui rendering for the TUI.

use crate::app::{
    App, PROGRAM_CONTENT_PADDING_X, PROGRAM_CONTENT_PADDING_Y, PROGRAM_REVEAL_MS, HarnessHit, HintZone,
    ListItem as AppListItem, MainWindowTree, Minibuffer, MinibufferIntent, PaneFocus, ScreenPoint,
    Selection, SessionTitleMenuAction, TextSelectionRange, ViewMode, WindowDividerHit,
    WindowPaneHit, WindowSplitDirection, ZoomMode,
};
use crate::keymap::KeyAction;
use crate::text_util::wrap_to_width;
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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
const PROGRAM_REVEAL_SECS: f32 = PROGRAM_REVEAL_MS as f32 / 1000.0;
const PROGRAM_RUN_BUTTON: &str = " ▶ ";
const PROGRAM_CLIP_HOVER_PREVIEW_COLS: u16 = 102;
const PROGRAM_CLIP_HOVER_PREVIEW_ROWS: u16 = 12;
/// How long the program shimmer-text preview lingers after the pointer stops
/// moving before it self-dismisses. The clip-chip preview, by contrast, stays
/// up for as long as the chip is hovered.
const PROGRAM_SHIMMER_HOVER_IDLE: Duration = Duration::from_secs(1);
const PROGRAM_SELECTION_RUN_MENU_W: u16 = 9;
const PROGRAM_SELECTION_RUN_MENU_H: u16 = 3;

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
    app.layout.matrix_operator_title_hit = None;
    app.layout.matrix_widget_hits.clear();
    app.layout.dynamic_ui_trigger = None;
    app.layout.dynamic_ui_triggers.clear();
    app.layout.shortcut_hints.clear();
    app.layout.modeline_approval_mode_hit = None;
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

    // Avoid per-frame full-surface clears; they cause faint background
    // blinking on some terminals. Only clear when we know geometry likely
    // exposed stale cells (handled inside individual renderers when panes
    // shrink).
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
    render_view_program_toggle_tooltip(f, app);
    render_view_close_tooltip(f, app);
    render_browser_preview_close_tooltip(f, app);
    render_list_title_button_tooltips(f, app);
    render_view_uncollapse_tooltip(f, app);
    render_harness_unavailable_tooltip(f, app);
    render_modeline_approval_mode_tooltip(f, app);
    app.sync_program_popup_with_selection();
    render_program_popup(f, app);
    render_tasks_popup(f, app);
    render_remote_control_popup(f, app);
    if app.help_visible {
        app.layout.modal_area = Some(render_help(f, area, &app.theme));
    }
    render_session_title_menu(f, app);
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

/// Hit zone for the Matrix-rain panel's collapse/expand toggle button (the
/// `−` / `+` glyph at the right edge of the panel title bar). Only one row is
/// needed because the title bar survives even when the panel is collapsed.
pub fn matrix_rain_close_button_range(rain_area: Rect) -> Option<(u16, u16, u16)> {
    if rain_area.width < 8 || rain_area.height < 1 {
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
    render_tooltip_at(f, theme, label, anchor_x, anchor_y, 2, -1);
}

fn render_tooltip_at(
    f: &mut Frame,
    theme: &Theme,
    label: &str,
    anchor_x: u16,
    anchor_y: u16,
    x_offset: i16,
    y_offset: i16,
) {
    let total = f.area();
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2;
    let h: u16 = 3;
    let mut tx = anchor_x.saturating_add_signed(x_offset);
    let mut ty = anchor_y.saturating_add_signed(y_offset);
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
    render_tooltip_rect(f, theme, label, rect);
}

fn render_tooltip_rect(f: &mut Frame, theme: &Theme, label: &str, rect: Rect) {
    let block = theme.themed_block("");
    let p = Paragraph::new(label)
        .block(block)
        .style(theme.text_style());
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
    if let Some((xs, xe, y)) = app.layout.matrix_operator_loop_hit {
        if my == y && mx >= xs && mx < xe {
            let label = if app.operator_loop_disabled() {
                " Resume operator loop "
            } else {
                " Pause operator loop "
            };
            render_button_tooltip(f, &app.theme, label, xs, y.saturating_add(2));
            return;
        }
    }
    if let Some((xs, xe, y)) = app.layout.matrix_operator_title_hit {
        if my == y && mx >= xs && mx < xe {
            render_button_tooltip(
                f,
                &app.theme,
                &format!(" operator {} ", matrix_operator_status(app)),
                xs,
                y.saturating_add(2),
            );
            return;
        }
    }
    // Note: widget title squares no longer show a tooltip — hovering a square
    // reveals the widget itself (see `render_session_widget_title` /
    // `render_matrix_rain_header`).
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
    if let Some(rain) = app.layout.matrix_rain_area {
        if let Some((xs, xe, y)) = matrix_rain_close_button_range(rain) {
            if my == y && mx >= xs && mx < xe {
                let (label, anchor_y) = if app.matrix_rain_hidden {
                    (" Expand Operator ", y)
                } else {
                    (" Collapse Operator ", y.saturating_add(2))
                };
                render_button_tooltip(f, &app.theme, label, xs, anchor_y);
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

pub fn view_program_toggle_button_range(view_area: Rect) -> (u16, u16, u16) {
    let x_start = view_area.x + 2;
    let x_end = view_area.x + 3;
    (x_start, x_end, view_area.y)
}

/// On-screen span of the right-aligned sticky-widget title cluster
/// (`─ □ □ …`) on a pane's top border. Returns `(x_start, x_end_exclusive, y)`
/// where `x_start` is the column of the cluster's leading `─`.
///
/// This mirrors ratatui's right-aligned title stacking exactly so hover/click
/// hit-testing lands on the visible glyphs. Ratatui lays right-aligned titles
/// out from the right border leftward, inserting one blank separator cell
/// between each. To this cluster's right sit (rightmost first) the actions
/// button (`close_width` cells — e.g. `" ☰ "` is 4, since ☰ is two cells wide;
/// 0 when hidden) and the harness label (`reserved_right_width`), each
/// consuming its own width plus one separator. `label_width` is this cluster's
/// own width. The harness label is always rendered alongside the widget
/// cluster, so its separator is always reserved.
pub fn dynamic_ui_trigger_range(
    view_area: Rect,
    close_width: u16,
    label_width: u16,
    reserved_right_width: u16,
) -> (u16, u16, u16) {
    // The right border column is the exclusive right edge of the title area.
    let titles_right = view_area
        .x
        .saturating_add(view_area.width)
        .saturating_sub(1);
    // Each title to the right of this cluster consumes its width + 1 separator.
    let close_reserved = if close_width > 0 {
        close_width.saturating_add(1)
    } else {
        0
    };
    let harness_reserved = reserved_right_width.saturating_add(1);
    let x_end = titles_right
        .saturating_sub(close_reserved)
        .saturating_sub(harness_reserved);
    (x_end.saturating_sub(label_width), x_end, view_area.y)
}

fn session_sticky_widget_panels(app: &App, session_id: &str) -> Vec<agentd_protocol::UiPanel> {
    let Some(panels) = app.ui_panels.get(session_id) else {
        return Vec::new();
    };
    let mut panels: Vec<_> = panels
        .values()
        .filter(|panel| panel.placement != agentd_protocol::UiPlacement::Inline)
        .cloned()
        .collect();
    panels.sort_by(|a, b| {
        a.created_at_ms
            .cmp(&b.created_at_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    panels
}

fn render_session_widget_title(
    app: &mut App,
    view_area: Rect,
    session_id: String,
    panels: Vec<agentd_protocol::UiPanel>,
    close_width: u16,
    reserved_right_width: u16,
    border_style: Style,
) -> Line<'static> {
    let label_width = 2u16.saturating_add((panels.len() as u16).saturating_mul(2));
    let (x_start, _x_end, y) =
        dynamic_ui_trigger_range(view_area, close_width, label_width, reserved_right_width);
    // The leading "─ " stitches the indicator into the top border, so it must
    // carry the pane's own border color (the session view's focus-aware border,
    // the program's accent border). Passing the style in keeps the two title bars
    // from drifting — a hardcoded `pane_border_style` here painted a green dash
    // on the program's accent border.
    let mut spans = vec![Span::styled("─ ", border_style)];
    // `x_start` is the on-screen column of the cluster's leading `─` (see
    // `dynamic_ui_trigger_range`, which reproduces ratatui's right-aligned
    // title geometry). The first square glyph sits two cells in, past the
    // leading "─ "; advancing by 2 per square then tracks each "□ " pair. This
    // puts the hover test and the registered hit exactly on the visible glyph.
    let now = Instant::now();
    // Drop a lapsed hover preview so a stale square doesn't read as filled.
    if app.dynamic_ui_hover.as_ref().is_some_and(|h| h.until <= now) {
        app.dynamic_ui_hover = None;
    }
    let mut icon_x = x_start.saturating_add(2);
    for panel in panels {
        let hovered = app
            .mouse_pos
            .is_some_and(|(mx, my)| my == y && mx >= icon_x && mx < icon_x.saturating_add(1));
        if hovered {
            // Hovering the square reveals the widget itself. The 1s grace lets
            // the pointer travel down onto the widget body, where it's held open.
            app.dynamic_ui_hover = Some(crate::app::DynamicUiHover {
                session_id: session_id.clone(),
                panel_id: panel.id.clone(),
                until: now + Duration::from_millis(crate::app::DYNAMIC_UI_HOVER_GRACE_MS),
            });
        }
        let pinned = app.dynamic_ui_panel_pinned(&session_id, &panel.id);
        let glyph = if pinned { "■" } else { "□" };
        let style = if pinned {
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default()
                .fg(app.theme.matrix_flash_good)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.dim)
        };
        app.layout
            .dynamic_ui_widget_hits
            .push(crate::app::DynamicUiWidgetHit {
                session_id: session_id.clone(),
                panel_id: panel.id.clone(),
                row: y,
                start_col: icon_x,
                end_col: icon_x.saturating_add(1),
            });
        spans.push(Span::styled(glyph, style));
        spans.push(Span::raw(" "));
        icon_x = icon_x.saturating_add(2);
    }
    Line::from(spans).alignment(ratatui::layout::Alignment::Right)
}

fn hovered_view_close_button(app: &App, view_area: Rect) -> bool {
    let Some((mx, my)) = app.mouse_pos else {
        return false;
    };
    let (x_start, x_end, y) = view_close_button_range(view_area);
    my == y && mx >= x_start && mx < x_end
}

fn hovered_view_program_toggle_button(app: &App, view_area: Rect) -> bool {
    let Some((mx, my)) = app.mouse_pos else {
        return false;
    };
    let (x_start, x_end, y) = view_program_toggle_button_range(view_area);
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

    render_tooltip_at(f, &app.theme, " Unpin session ", dx, dy, 2, -1);
}

fn render_view_close_tooltip(f: &mut Frame, app: &App) {
    let Some(view_area) = app.layout.view_area else {
        return;
    };
    if app.session_title_menu.is_some() {
        return;
    }
    if !hovered_view_close_button(app, view_area) {
        return;
    }
    let (cx, _, cy) = view_close_button_range(view_area);
    let label = " Session actions ";
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
    render_tooltip_rect(
        f,
        &app.theme,
        label,
        Rect {
            x: tx,
            y: ty,
            width: w,
            height: h,
        },
    );
}

fn render_view_program_toggle_tooltip(f: &mut Frame, app: &App) {
    let Some(view_area) = app.layout.view_area else {
        return;
    };
    if !hovered_view_program_toggle_button(app, view_area) {
        return;
    }
    let Some(s) = app.selected_session() else {
        return;
    };
    let program_open = app
        .open_program_session_ids()
        .iter()
        .any(|id| id == &s.id);
    let (cx, _, cy) = view_program_toggle_button_range(view_area);
    let label = if program_open {
        " Program mode: click to return to chat "
    } else {
        " Chat mode: click to open program "
    };
    let inner_w = UnicodeWidthStr::width(label) as u16;
    let w = inner_w + 2;
    let h: u16 = 3;
    let rect = view_program_toggle_tooltip_rect(view_area, f.area(), cx, cy, w, h);
    render_tooltip_rect(f, &app.theme, label, rect);
}

fn view_program_toggle_tooltip_rect(
    view_area: Rect,
    total: Rect,
    anchor_x: u16,
    anchor_y: u16,
    width: u16,
    height: u16,
) -> Rect {
    let view_right = view_area.x.saturating_add(view_area.width);
    let total_right = total.x.saturating_add(total.width);
    let max_right = view_right.min(total_right);
    let min_x = view_area.x.max(total.x);
    let mut x = anchor_x.saturating_add(2).max(min_x);
    if x.saturating_add(width) > max_right {
        x = max_right.saturating_sub(width).max(min_x);
    }

    let total_bottom = total.y.saturating_add(total.height);
    let mut y = anchor_y.saturating_add(1);
    if y.saturating_add(height) > total_bottom {
        y = total_bottom.saturating_sub(height);
    }

    Rect {
        x,
        y,
        width,
        height,
    }
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
    render_tooltip_rect(
        f,
        &app.theme,
        label.as_str(),
        Rect {
            x: tx,
            y: ty,
            width: w,
            height: h,
        },
    );
}

/// Render the new-session harness picker with each name as a
/// clickable span. Records per-name column ranges in
/// `app.layout.minibuffer_harness_hits` so the click handler can
/// submit the picked name without the user having to type it.
fn render_harness_picker(f: &mut Frame, area: Rect, app: &mut App, mb: &Minibuffer) {
    // Show every registered harness. For a new session we also surface the
    // synthetic `project` op; forking targets a real harness only.
    // Unavailable harnesses (binary not on PATH) render dimmed and
    // strike-through; clicking them no-ops + drops a status note;
    // hover surfaces a "not installed" tooltip.
    let is_fork = matches!(mb.intent, MinibufferIntent::ForkSessionHarness { .. });
    let mut entries: Vec<(String, bool)> = app
        .harnesses
        .iter()
        .map(|h| (h.name.clone(), h.available))
        .collect();
    if !is_fork {
        entries.push(("project".to_string(), true));
    }

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

    push_raw(&mut spans, &mut col, if is_fork { "Fork → [" } else { "New [" });
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
    render_tooltip_at(f, &app.theme, label, dx, dy, 2, -1);
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
            ViewMode::Chat => render_chat(f, main_area, app),
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
                    // Reserve room for the trailing unblock marker (" ●") so the
                    // right-aligned harness label doesn't shift when it shows.
                    let marker_w = if s.needs_attention && !s.archived { 2 } else { 0 };
                    // Always leave at least one cell of gap between the name
                    // and the right-aligned harness.
                    let name_avail = row_w.saturating_sub(prefix_w + 1 + harness_w + marker_w);
                    let raw_name = primary_label(s);
                    let scroll = if is_selected && focused {
                        // ~6 chars/sec (was 5; +20% per user feedback).
                        Some((app.start_instant.elapsed().as_millis() / 167) as usize)
                    } else {
                        None
                    };
                    let name_display = fit_name(&raw_name, name_avail, scroll);
                    let name_display_w = name_display.chars().count();
                    let gap =
                        row_w.saturating_sub(prefix_w + name_display_w + harness_w + marker_w);
                    let gap_str: String = " ".repeat(gap);
                    // Archived sessions read as muted — they're terminated and
                    // only visible because the "show archived" toggle is on.
                    let name_style = if s.archived {
                        Style::default()
                            .fg(app.theme.dim)
                            .add_modifier(Modifier::DIM)
                    } else {
                        Style::default().fg(app.theme.text)
                    };
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
                        Span::styled(name_display, name_style),
                        // Unblock marker: a blue dot trailing the title when the
                        // session needs the operator (spec 0054).
                        Span::styled(
                            if marker_w > 0 { " ●" } else { "" }.to_string(),
                            Style::default().fg(app.theme.info),
                        ),
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
                AppListItem::ArchivedRow {
                    section,
                    count,
                    expanded,
                    indented,
                } => {
                    // Expandable footer: "▸ N archived" (collapsed) /
                    // "▾ N archived" (open). Indented to sit under a project's
                    // members; flush-left for the ungrouped section.
                    let disclosure = if *expanded { "▾" } else { "▸" };
                    let indent = match section {
                        crate::app::ArchiveSection::Subagents(_) => "    ",
                        crate::app::ArchiveSection::Group(_) if *indented => "  ",
                        _ => "",
                    };
                    ListItem::new(Line::from(Span::styled(
                        format!("{indent}{disclosure} {count} archived"),
                        Style::default()
                            .fg(app.theme.dim)
                            .add_modifier(Modifier::DIM),
                    )))
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
/// `SESSION_LIST_H_MIN + MATRIX_RAIN_H_MIN` of total inner height the
/// list takes the entire pane and the rain area is reported as
/// zero-height — i.e., the rain effectively goes "out of view" when
/// the terminal is too short. When the user collapses the rain, only
/// its 1-row title bar stays pinned at the bottom of the list pane.
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
    // Collapsed: the rain panel shrinks to just its 1-row title bar, pinned at
    // the bottom of the list pane, as long as the list keeps its minimum height
    // above it. When the pane is too short the rain goes fully out of view.
    if matrix_rain_hidden {
        if inner.height <= crate::app::SESSION_LIST_H_MIN {
            return (inner, empty_matrix);
        }
        let list_h = inner.height.saturating_sub(1);
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
            height: 1,
        };
        return (list, matrix);
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
    // One row is enough for the title bar; below that the panel is fully out of
    // view. A collapsed panel keeps the title bar and skips the animated body.
    if rain_area.width < 8 || rain_area.height < 1 {
        return;
    }
    app.layout.matrix_rain_area = Some(rain_area);
    let now = Instant::now();
    render_matrix_rain_header(f, rain_area, app, now);
    if app.matrix_rain_hidden {
        // Collapsed: only the title bar shows; skip the animated body.
        return;
    }
    if rain_area.height < 4 {
        return;
    }
    let rain_area = Rect {
        x: rain_area.x,
        y: rain_area.y + 1,
        width: rain_area.width,
        height: rain_area.height.saturating_sub(1),
    };
    if rain_area.height < 3 {
        return;
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

    // Foreground thumbnail: show the most recent browser preview from a
    // session that is NOT currently displayed in any main-view pane (if the
    // user can already see the page in their session view, repeating it here
    // is redundant). Rendered after the rain so it sits on top, fitted to
    // preserve aspect ratio (no crop), at full brightness.
    {
        let visible_ids = app.main_windows.visible_session_ids();
        let thumb = app
            .browser_previews
            .iter()
            .filter(|(sid, _)| !visible_ids.contains(&sid.as_str()))
            .max_by_key(|(_, state)| state.revealed_at)
            .and_then(|(_, state)| {
                state.decoded.clone().map(|img| {
                    (img, state.revealed_at, state.hide_after, state.hover_started.is_some())
                })
            });
        if let Some((img, revealed_at, hide_after, hovered)) = &thumb {
            let row_frac = preview_reveal_range(*revealed_at, *hide_after, now, *hovered);
            if row_frac.1 > row_frac.0 {
                let (ow, oh) = blit_scale_dims(img.dimensions(), rain_area, false);
                let resized = resized_image(&mut app.image_resize_cache, img, ow * 2, oh);
                paint_resized_quadrants(f, rain_area, &resized, 1.0, row_frac);
            }
        }
    }

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
    // Operator monolog: overlaid on the still-running rain (not a takeover), so
    // the rain keeps animating underneath and doesn't restart when the text
    // clears. Skipped while the orchestrator panel is open (the text is already
    // visible right below). Widgets render after, on top.
    render_operator_monolog(f, rain_area, app, now);
    render_matrix_widget_viewport(f, rain_area, app, now);
}

/// Reveal speed and post-typing dwell for the operator monolog.
const MONOLOG_MS_PER_CHAR: u64 = 28;
const MONOLOG_HOLD_MS: u64 = 4200;
/// Clear-space padding around the typewriter text so it reads cleanly over the
/// rain instead of blending into it.
const MONOLOG_HPAD: u16 = 2;
const MONOLOG_VPAD: u16 = 1;

/// Word-wrap `text` to `max_w` display columns: break at spaces where possible,
/// hard-break overlong words, honor embedded newlines. Returns the lines so the
/// caller can size a tight box around them.
/// Overlay the operator's latest monolog on top of the (still-running) matrix
/// rain as a bright typewriter line in a padded clear-box, removing it at once
/// after the type → hold window (no fade). Returns `true` if it drew this
/// frame. Crucially this is an
/// *overlay*, not a takeover: the rain is rendered every frame regardless, so
/// it keeps animating underneath and never restarts when the text clears.
/// Skipped while the orchestrator panel is open — the operator's text is
/// already visible right below in the panel, so the overlay would duplicate it.
pub(crate) fn render_operator_monolog(
    f: &mut Frame,
    area: Rect,
    app: &mut App,
    now: Instant,
) -> bool {
    // Snapshot what we need so the borrow ends before we may clear the state.
    let (chars, started_at) = match app.operator_monolog.as_ref() {
        Some(m) => (m.text.chars().collect::<Vec<char>>(), m.started_at),
        None => return false,
    };
    let n = chars.len() as u64;
    let elapsed = now.saturating_duration_since(started_at).as_millis() as u64;
    let type_ms = n.saturating_mul(MONOLOG_MS_PER_CHAR);
    // Disappear at once after the hold — no fade-out.
    if elapsed >= type_ms + MONOLOG_HOLD_MS {
        app.operator_monolog = None;
        return false;
    }
    if matches!(
        app.minibuffer.as_ref().map(|m| &m.intent),
        Some(MinibufferIntent::Orchestrator)
    ) {
        return false; // duplicate of the open panel below
    }
    if area.width < 12 || area.height < 3 {
        return false;
    }

    let shown = ((elapsed / MONOLOG_MS_PER_CHAR) as usize).min(chars.len());
    let mut body: String = chars[..shown].iter().collect();
    let typing = (shown as u64) < n;
    if typing && (elapsed / 450) % 2 == 0 {
        body.push('▌'); // blinking cursor while typing
    }

    // Wrap to a tight box and clear a padded region around it, so the text sits
    // on clean backdrop (no rain bleeding into it) with a margin on all sides.
    let max_text_w = area
        .width
        .saturating_sub(2 + 2 * MONOLOG_HPAD) // 1-col area margin each side + padding
        .max(1) as usize;
    let lines = wrap_to_width(&body, max_text_w);
    let text_w = lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0) as u16;
    let max_text_h = area.height.saturating_sub(1 + 2 * MONOLOG_VPAD).max(1);
    let text_h = (lines.len() as u16).min(max_text_h);
    let box_w = (text_w + 2 * MONOLOG_HPAD).min(area.width);
    let box_h = (text_h + 2 * MONOLOG_VPAD).min(area.height);
    let box_x = area.x + 1;
    let box_y = area.y + 1;
    // Clear removes the rain in the padded box (resets to the terminal/backdrop
    // bg the rain draws on), leaving a clean margin around the text.
    f.render_widget(
        Clear,
        Rect {
            x: box_x,
            y: box_y,
            width: box_w,
            height: box_h,
        },
    );

    // Bright, matching the matrix horizontal keyword reveals (theme.text).
    let text_style = Style::default().fg(app.theme.text);
    let tx = box_x + MONOLOG_HPAD;
    let ty = box_y + MONOLOG_VPAD;
    for (i, line) in lines.iter().take(text_h as usize).enumerate() {
        f.buffer_mut()
            .set_string(tx, ty + i as u16, line, text_style);
    }
    true
}

/// If the mouse is hovering a matrix-rain horizontal reveal word, draw a
/// one-line tooltip on an adjacent row naming the source session.
fn render_matrix_widget_viewport(f: &mut Frame, rain_area: Rect, app: &mut App, now: Instant) {
    if !app.matrix_widget_visible(now) {
        return;
    }
    let panels = app.orchestrator_widget_panels();
    if panels.is_empty() || rain_area.width < 8 || rain_area.height < 3 {
        return;
    }
    let cursor_inside = app
        .mouse_pos
        .is_some_and(|(mx, my)| contains_rect(rain_area, mx, my));
    // Hovering anywhere in the rain panel holds a hover preview open, so the
    // pointer can slide off the title square down onto the widget body.
    if cursor_inside {
        if let Some(hover) = app.matrix_widget_hover.as_mut() {
            hover.until = now + Duration::from_millis(crate::app::DYNAMIC_UI_HOVER_GRACE_MS);
        }
    }
    let shown = app.matrix_widget_shown(now);
    let selected_idx = shown
        .as_ref()
        .and_then(|id| panels.iter().position(|panel| &panel.id == id))
        .unwrap_or(0);
    let panel = panels[selected_idx].clone();
    let Some(session_id) = app.orchestrator_id.clone() else {
        return;
    };

    let width = rain_area.width.saturating_sub(2).max(1);
    let max_height = rain_area.height.saturating_sub(2).max(1);
    let height = max_height.min(8).max(1);
    let area = Rect {
        x: rain_area.x.saturating_add(1),
        y: rain_area
            .y
            .saturating_add(rain_area.height.saturating_sub(height + 1) / 2),
        width,
        height,
    };
    let title = dynamic_ui_panel_title(&panel).unwrap_or_else(|| "Operator widget".to_string());
    let title = format!(" {} ", title);
    f.render_widget(Clear, area);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                truncate_to_width(&title, area.width.saturating_sub(2) as usize),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(app.theme.matrix_line)),
        area,
    );
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let suppress_first_heading = leading_markdown_heading(&panel.markdown).is_some();
    let mut lines = render_agentd_markdown_lines(
        &panel.markdown,
        &app.theme,
        app.mouse_pos,
        inner,
        Some(session_id.as_str()),
        Some(panel.id.as_str()),
        &mut app.layout.dynamic_ui_action_hits,
        &mut app.layout.dynamic_ui_url_hits,
        suppress_first_heading,
    );
    let viewport_rows = inner.height as usize;
    let padding_rows = viewport_rows.saturating_sub(lines.len());
    lines.extend(std::iter::repeat(Line::raw("")).take(padding_rows));
    let visible_lines: Vec<_> = lines.into_iter().take(viewport_rows).collect();
    f.render_widget(
        Paragraph::new(visible_lines).wrap(Wrap { trim: false }),
        inner,
    );
}

fn matrix_operator_status(app: &App) -> &'static str {
    if app.operator_has_pending_approval() {
        return "approval";
    }
    let Some(orchestrator_id) = app.orchestrator_id.as_deref() else {
        return "offline";
    };
    if app
        .agent_statuses
        .get(orchestrator_id)
        .is_some_and(|status| status.active)
    {
        return "thinking";
    }
    if app
        .sessions
        .iter()
        .find(|session| session.id == orchestrator_id)
        .is_some_and(|session| session.last_pty_at_ms.is_some_and(recent_pty_activity))
    {
        return "acting";
    }
    "watching"
}

fn recent_pty_activity(last_pty_at_ms: i64) -> bool {
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return false;
    };
    let now_ms = now.as_millis() as i64;
    now_ms.saturating_sub(last_pty_at_ms) < 600
}

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
            // tooltip says both (e.g. "fix auth · smith").
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

fn render_matrix_rain_header(f: &mut Frame, area: Rect, app: &mut App, now: Instant) {
    let line_style = Style::default().fg(app.theme.matrix_line);
    let close_style = Style::default()
        .fg(app.theme.matrix_close)
        .add_modifier(Modifier::BOLD);
    for x in area.x..area.x + area.width {
        f.buffer_mut().set_string(x, area.y, "─", line_style);
    }

    let panels = app.orchestrator_widget_panels();
    // Expire any lapsed hover preview (and clear state when no panels remain)
    // so the squares below reflect the live shown/pinned widget.
    app.matrix_widget_visible(now);
    let approval_pending = app.operator_has_pending_approval();
    let operator_text = if approval_pending {
        "operator !"
    } else {
        "operator"
    };

    // Play/pause toggle for the operator ambient loop.
    // ▶ = loop is paused (click to enable); ⏸ = loop is running (click to disable).
    let loop_disabled = app.operator_loop_disabled();
    let loop_icon = if loop_disabled { "▶" } else { "⏸" };
    let loop_icon_width = UnicodeWidthStr::width(loop_icon) as u16;
    let loop_icon_x = area.x.saturating_add(1);
    let loop_icon_end = loop_icon_x.saturating_add(loop_icon_width);
    let loop_icon_hovered = app
        .mouse_pos
        .is_some_and(|(mx, my)| my == area.y && mx >= loop_icon_x && mx < loop_icon_end);
    let loop_icon_style = if loop_icon_hovered {
        Style::default()
            .fg(app.theme.matrix_flash_good)
            .add_modifier(Modifier::BOLD)
    } else if loop_disabled {
        Style::default().fg(app.theme.dim)
    } else {
        Style::default().fg(app.theme.accent)
    };
    f.buffer_mut()
        .set_string(loop_icon_x, area.y, loop_icon, loop_icon_style);
    app.layout.matrix_operator_loop_hit = Some((loop_icon_x, loop_icon_end, area.y));

    // Operator label renders after the icon; the leading space in " operator "
    // provides the visual gap between icon and text.
    let label = format!(" {operator_text} ");
    let label_x = loop_icon_end;
    let operator_start = label_x.saturating_add(1);
    let operator_end = operator_start.saturating_add(UnicodeWidthStr::width(operator_text) as u16);
    let operator_hovered = app
        .mouse_pos
        .is_some_and(|(mx, my)| my == area.y && mx >= operator_start && mx < operator_end);
    let operator_style = if approval_pending {
        Style::default()
            .fg(app.theme.warning)
            .add_modifier(Modifier::BOLD)
    } else if operator_hovered {
        Style::default()
            .fg(app.theme.matrix_flash_good)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.accent)
    };
    f.buffer_mut()
        .set_string(label_x, area.y, label.as_str(), operator_style);
    app.layout.matrix_operator_title_hit = Some((operator_start, operator_end, area.y));

    let separator_x = operator_end.saturating_add(1);
    if !panels.is_empty() {
        f.buffer_mut()
            .set_string(separator_x, area.y, "─", line_style);
    }
    let mut icon_x = separator_x.saturating_add(2);
    let icon_limit = area.x + area.width.saturating_sub(5);
    for panel in panels {
        if icon_x >= icon_limit {
            break;
        }
        let hovered = app
            .mouse_pos
            .is_some_and(|(mx, my)| my == area.y && mx >= icon_x && mx < icon_x.saturating_add(1));
        // Hovering a square reveals that widget in the rain viewport. Skipped
        // when collapsed — the viewport only renders in the expanded panel, so
        // a preview would have nowhere to show.
        if hovered && !app.matrix_rain_hidden {
            app.matrix_widget_hover = Some(crate::app::MatrixWidgetHover {
                panel_id: panel.id.clone(),
                until: now + Duration::from_millis(crate::app::DYNAMIC_UI_HOVER_GRACE_MS),
            });
        }
        let pinned = app.matrix_widget_pinned.as_deref() == Some(panel.id.as_str());
        let glyph = if pinned { "■" } else { "□" };
        let style = if pinned {
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if hovered {
            Style::default()
                .fg(app.theme.matrix_flash_good)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(app.theme.dim)
        };
        f.buffer_mut().set_string(icon_x, area.y, glyph, style);
        let w = UnicodeWidthStr::width(glyph) as u16;
        app.layout
            .matrix_widget_hits
            .push(crate::app::MatrixWidgetHit {
                kind: crate::app::MatrixWidgetHitKind::Select {
                    panel_id: panel.id.clone(),
                },
                row: area.y,
                start_col: icon_x,
                end_col: icon_x.saturating_add(w),
            });
        icon_x = icon_x.saturating_add(w + 1);
    }

    let toggle_glyph = if app.matrix_rain_hidden {
        " + "
    } else {
        " − "
    };
    let toggle_x = area.x + area.width.saturating_sub(3);
    let toggle_hovered = app
        .mouse_pos
        .is_some_and(|(mx, my)| my == area.y && mx >= toggle_x && mx < toggle_x.saturating_add(3));
    let toggle_style = if toggle_hovered {
        Style::default()
            .fg(app.theme.matrix_flash_good)
            .add_modifier(Modifier::BOLD)
    } else {
        close_style
    };
    f.buffer_mut()
        .set_string(toggle_x, area.y, toggle_glyph, toggle_style);
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
        Selection::ArchivedRow(_) => 0x617263,
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

/// Build the shared right-side cluster of a pane title bar — session widget
/// indicators, the harness label, and the close (` x `) button — and add them to
/// `block` as right-aligned titles. Both the normal session view and the program
/// popup call this so their right clusters can't drift in layout, styling, or
/// geometry.
///
/// Order matters: ratatui stacks right-aligned titles left-to-right in the order
/// they're added, so widgets go FIRST (leftmost), harness SECOND, and the close
/// button LAST (rightmost edge, matching `view_close_button_range`, which
/// hit-tests the last 3 cells of the top border). Widget hit ranges are
/// registered as a side effect of `render_session_widget_title` (into
/// `dynamic_ui_widget_hits`); the close geometry is `view_close_button_range`.
fn apply_pane_title_right_cluster<'a>(
    app: &mut App,
    area: Rect,
    summary: Option<&agentd_protocol::SessionSummary>,
    border_style: Style,
    show_close: bool,
    session_actions: bool,
    focused: bool,
    menu_icon_color: Color,
    mut block: Block<'a>,
) -> Block<'a> {
    // Harness name right-aligned on the top border so it visually detaches from
    // the session-name title. Sits just left of the close button (or at the
    // right edge when no close is shown). Color matches the border so harness
    // reads as part of the title bar's frame, not as a separately-styled badge.
    let harness_label_text = summary.map(|s| format!(" {} ", harness_label(s)));
    let harness_width = harness_label_text
        .as_deref()
        .map(UnicodeWidthStr::width)
        .unwrap_or(0) as u16;
    let harness_right = harness_label_text.as_ref().map(|text| {
        Line::from(Span::styled(text.clone(), border_style))
            .alignment(ratatui::layout::Alignment::Right)
    });
    // The close / session-actions button is the rightmost right-aligned title.
    // Its on-screen width must be known up front so the widget cluster's
    // hit geometry can account for it — the ☰ glyph is two cells wide, so
    // `" ☰ "` is 4 cells, not 3. Measure with the same width function ratatui
    // uses when it lays the title out, so the two never disagree.
    let close_label = if session_actions { " ☰ " } else { " x " };
    let close_width = if show_close {
        UnicodeWidthStr::width(close_label) as u16
    } else {
        0
    };
    let widget_title = summary.and_then(|s| {
        let panels = session_sticky_widget_panels(app, &s.id);
        (!panels.is_empty()).then(|| {
            render_session_widget_title(
                app,
                area,
                s.id.clone(),
                panels,
                close_width,
                harness_width,
                border_style,
            )
        })
    });
    // Right-aligned close / session-actions button on the top border. Hover is
    // hit-tested against `app.mouse_pos` so the glyph bolds when the cursor is
    // over it — the click handlers in `app.rs` mirror the same
    // `view_close_button_range` geometry. When the pane is unfocused the glyph
    // dims to match the unfocused title-bar border, so an inactive pane's menu
    // icon no longer reads at full brightness. `menu_icon_color` sets the base
    // hue (program view passes its border color so the ☰ matches the frame).
    let close_hovered = show_close && hovered_view_close_button(app, area);
    let close_style = session_menu_icon_style(&app.theme, menu_icon_color, close_hovered, focused);
    let close = Line::from(Span::styled(close_label, close_style))
        .alignment(ratatui::layout::Alignment::Right);
    if let Some(ui) = widget_title {
        block = block.title(ui);
    }
    if let Some(h) = harness_right {
        block = block.title(h);
    }
    if show_close {
        block = block.title(close);
    }
    block
}

/// Style for the session-title actions glyph (the ` ☰ ` / ` x ` button at the
/// right edge of a pane title bar, shared by the chat/PTY session view and the
/// program view via `apply_pane_title_right_cluster`).
///
/// Hover wins: the glyph bolds in themed text color when the cursor is over it.
/// Otherwise it paints in `base` — the chat/PTY session view passes
/// `matrix_close`; the program view passes its border color so the icon reads as
/// part of the program frame instead of as a separately-hued badge. Either way it
/// dims (`Modifier::DIM`) when the pane is unfocused so it tracks the unfocused
/// title-bar border instead of staying at full brightness on an inactive pane.
fn session_menu_icon_style(theme: &Theme, base: Color, hovered: bool, focused: bool) -> Style {
    if hovered {
        Style::default().fg(theme.text).add_modifier(Modifier::BOLD)
    } else {
        let style = Style::default().fg(base);
        if focused {
            style
        } else {
            style.add_modifier(Modifier::DIM)
        }
    }
}

fn render_session_title_menu(f: &mut Frame, app: &App) {
    let Some(menu) = &app.session_title_menu else {
        return;
    };
    let area = menu.area;
    if area.width < 4 || area.height < 3 {
        return;
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border))
        .title(Span::styled(
            " session ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, area);
    f.render_widget(block, area);
    for (idx, action) in SessionTitleMenuAction::ALL.iter().copied().enumerate() {
        let row = area.y.saturating_add(1).saturating_add(idx as u16);
        if row >= area.y.saturating_add(area.height).saturating_sub(1) {
            break;
        }
        let hovered = app.mouse_pos.is_some_and(|(mx, my)| {
            my == row
                && mx > area.x
                && mx < area.x.saturating_add(area.width).saturating_sub(1)
        });
        let style = if hovered {
            Style::default()
                .fg(app.theme.text)
                .bg(app.theme.inactive_highlight_bg)
                .add_modifier(Modifier::BOLD)
        } else if matches!(action, SessionTitleMenuAction::Delete) {
            Style::default().fg(app.theme.danger)
        } else {
            Style::default().fg(app.theme.text)
        };
        let label_text =
            if matches!(action, SessionTitleMenuAction::Archive)
                && app
                    .sessions
                    .iter()
                    .find(|s| s.id == menu.session_id)
                    .is_some_and(|s| s.archived)
            {
                "unarchive"
            } else {
                action.label()
            };
        let label = format!(" {label_text} ");
        let text = truncate_to_width(&label, area.width.saturating_sub(2) as usize);
        f.buffer_mut()
            .set_string(area.x.saturating_add(1), row, text, style);
    }
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
    let program_open = summary
        .as_ref()
        .is_some_and(|s| app.open_program_session_ids().iter().any(|id| id == &s.id));
    let mode_glyph = summary.as_ref().map(|s| {
        if program_open {
            program_mode_glyph()
        } else {
            session_status_glyph(app, s)
        }
    });
    // Label budget = total − 2 corners − right-side blocks − fixed
    // title scaffolding (` <glyph> <label> ` is 3 spaces + glyph
    // width + label).
    let glyph_w = mode_glyph.map(UnicodeWidthStr::width).unwrap_or(0);
    let label_budget = total
        .saturating_sub(2)
        .saturating_sub(harness_w)
        .saturating_sub(close_w)
        .saturating_sub(3 + glyph_w);
    let title = match (summary.as_ref(), group.as_ref()) {
        (Some(s), _) => format!(
            " {} {} ",
            mode_glyph.unwrap_or(""),
            truncate_to_width(&primary_label(s), label_budget),
        ),
        (None, Some(g)) => format!(" project: {} ", g.name),
        (None, None) => " no session ".to_string(),
    };
    // Right-side cluster (widget indicators, harness label, close button) is
    // shared with the program popup so the two title bars can't drift. Close is
    // only shown when a session is actually selected (groups, "no session", and
    // the diff-overlay branch don't need it).
    let show_close = summary.is_some();
    let border_style = pane_border_style(&app.theme, focused);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let menu_icon_color = app.theme.matrix_close;
    let block = apply_pane_title_right_cluster(
        app,
        area,
        summary.as_ref(),
        border_style,
        show_close,
        true,
        focused,
        menu_icon_color,
        block,
    );
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
        ViewMode::Chat => render_chat(f, inner, app),
    }
    render_main_transition(f, inner, app, window_id);
}

fn render_empty_session_state(f: &mut Frame, area: Rect, app: &mut App) {
    let card = centered_rect(area, 72, 10);
    let label_style = Style::default().fg(app.theme.accent);
    let hover_style = label_style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mouse = app.mouse_pos;
    let shortcut_rows = [
        (4_u16, 2_u16, "C-x C-f", KeyAction::OpenNewSession),
        (5_u16, 2_u16, "C-x x", KeyAction::OpenCommandPalette),
        (6_u16, 2_u16, "?", KeyAction::ToggleHelp),
        (7_u16, 2_u16, "C-x C-c", KeyAction::Quit),
    ];
    let mut hovered = [false; 4];
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
            "Welcome to construct",
            Style::default()
                .fg(app.theme.text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "Start with a session. Sessions are the live terminals construct tracks.",
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
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "C-x C-c",
                if hovered[3] { hover_style } else { label_style },
            ),
            Span::raw("  exit TUI"),
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
    // Resolve the per-window scrollbar-reveal timer up front: the render below
    // holds a mutable borrow on `app.histories`, which would block this
    // immutable `&self` method call if deferred to the scrollbar call site.
    let scrollbar_visible_until = app.terminal_scrollbar_visible_until(window_id);
    // Only adapters that publish `SessionEvent::EditorState` (currently
    // smith interactive) get the fixed editor pane at the bottom.
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
    // The smith editor pane below grows/shrinks on nearly every
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
    // Only clear when the editor pane has SHUNK (chat area grew) this frame —
    // otherwise let vt100 overdraw normally to avoid background blinking.
    let need_clear = app
        .layout
        .last_chat_areas
        .get(&id)
        .map(|prev| chat_area.height > prev.height || chat_area.width != prev.width)
        .unwrap_or(true);
    if need_clear {
        f.render_widget(Clear, chat_area);
        // Fill only the newly-exposed rows (when taller); cheap bound.
        let prev_h = app
            .layout
            .last_chat_areas
            .get(&id)
            .map(|r| r.height)
            .unwrap_or(0);
        let start = prev_h.min(chat_area.height);
        for row in start..chat_area.height {
            let blank = " ".repeat(chat_area.width as usize);
            let r = Rect {
                x: chat_area.x,
                y: chat_area.y + row,
                width: chat_area.width,
                height: 1,
            };
            f.render_widget(Paragraph::new(Line::from(vec![Span::raw(blank)])), r);
        }
    }
    app.layout.last_chat_areas.insert(id.clone(), chat_area);

    // Gapless bottom-align for short smith chats: when the history content
    // is shorter than the chat viewport, paint it hugging the editor pane
    // instead of anchored at the top (which leaves a gap above the prompt).
    let mut paint_area = chat_area;
    let mut paint_row_offset = row_offset;
    let is_smith_like = app
        .sessions
        .iter()
        .find(|s| s.id == id)
        .map(|s| is_smith_like_harness(&s.harness))
        .unwrap_or(false);
    if editor_area.is_some() && scroll == 0 && is_smith_like {
        let content_rows = non_empty_row_span(out.screen);
        if content_rows > 0 && content_rows < chat_area.height {
            let top_pad = chat_area.height - content_rows;
            paint_area.y = paint_area.y.saturating_add(top_pad);
            paint_area.height = content_rows;
            paint_row_offset = 0;
        }
    }

    render_pty_screen(
        f,
        paint_area,
        out.screen,
        &app.theme,
        editor_area.is_none(),
        paint_row_offset,
    );
    app.block_hits.insert(
        id.clone(),
        translate_block_hits(out.blocks, paint_row_offset, paint_area.height),
    );
    let terminal_scrollbar = render_terminal_scrollbar(
        f,
        chat_area,
        &app.theme,
        scrollbar_visible_until,
        clamped_scrollback,
        out.max_scrollback,
    );
    app.set_scrollback_for_window(window_id, clamped_scrollback);
    app.layout.terminal_scrollbar = terminal_scrollbar;
    render_visible_dynamic_ui_panels(f, area, app, &id, &sticky_panels);
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
    session_id: &str,
    panels: &[agentd_protocol::UiPanel],
) {
    let now = std::time::Instant::now();
    app.dynamic_ui_temporary_until
        .retain(|_, until| *until > now);
    if app.dynamic_ui_hover.as_ref().is_some_and(|h| h.until <= now) {
        app.dynamic_ui_hover = None;
    }
    let mut visible: Vec<_> = panels
        .iter()
        .filter(|panel| app.dynamic_ui_panel_visible(session_id, &panel.id))
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
                let key = (session_id.to_string(), panel.id.clone());
                if app.dynamic_ui_temporary_until.contains_key(&key) {
                    app.dynamic_ui_temporary_until.insert(
                        key,
                        now + std::time::Duration::from_secs(crate::app::DYNAMIC_UI_AUTOHIDE_SECS),
                    );
                }
            }
            // Hovering the widget body holds a hover preview open, so the
            // pointer can rest on it after sliding off the title square.
            if let Some(hover) = app.dynamic_ui_hover.as_mut() {
                if hover.session_id == session_id {
                    hover.until = now
                        + std::time::Duration::from_millis(crate::app::DYNAMIC_UI_HOVER_GRACE_MS);
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
    let mut rendered = render_dynamic_ui_stack_lines(inner, app, session_id, &visible);
    let content_rows = rendered.len();
    let viewport_rows = inner.height as usize;
    let max_scroll = content_rows.saturating_sub(viewport_rows);
    let offset = app
        .dynamic_ui_scroll_offsets
        .entry(session_id.to_string())
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
    app.layout.dynamic_ui_scroll_metrics =
        Some((session_id.to_string(), content_rows, viewport_rows));
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
    // Index-based so block elements that need lookahead (GFM tables, whose
    // header row is only a table once the next line is a `| --- |` delimiter)
    // can consume several source lines at once.
    let src_lines: Vec<&str> = markdown.lines().collect();
    let mut li = 0;
    while li < src_lines.len() {
        let cur = li;
        let raw = src_lines[cur];
        li += 1;
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
        // GFM table: a header row immediately followed by a `| --- |`
        // delimiter row. Consumes the whole block (hence the index loop) and
        // is detected before the paragraph fallback so the pipes render as an
        // aligned grid instead of literal text.
        if let Some((table, consumed)) = parse_markdown_table(&src_lines, cur) {
            if !pending_action_spans.is_empty() {
                let flushed = Line::from(std::mem::take(&mut pending_action_spans));
                rendered_rows += visual_line_count(std::iter::once(&flushed), panel_area.width);
                lines.push(flushed);
            }
            let rendered = render_markdown_table(&table, theme, panel_area);
            rendered_rows += visual_line_count(rendered.iter(), panel_area.width);
            lines.extend(rendered);
            li = cur + consumed;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug)]
struct MarkdownTable {
    header: Vec<String>,
    aligns: Vec<CellAlign>,
    rows: Vec<Vec<String>>,
}

/// Detect a GFM table starting at `lines[start]`: a header row immediately
/// followed by a delimiter row (`| --- | :--: |`). Returns the parsed table
/// plus how many source lines it spans, or `None` when `start` isn't a table.
/// The delimiter-row requirement keeps a plain paragraph that merely contains
/// a `|` from being mistaken for a table.
fn parse_markdown_table(lines: &[&str], start: usize) -> Option<(MarkdownTable, usize)> {
    let header_line = lines.get(start)?.trim();
    let delim_line = lines.get(start + 1)?.trim();
    if !line_has_table_cells(header_line) || !line_has_table_cells(delim_line) {
        return None;
    }
    let delim_cells = split_table_cells(delim_line);
    if delim_cells.is_empty() || !delim_cells.iter().all(|c| is_delimiter_cell(c)) {
        return None;
    }
    let header = split_table_cells(header_line);
    let aligns = delim_cells.iter().map(|c| cell_align(c)).collect();
    let mut rows = Vec::new();
    let mut i = start + 2;
    while let Some(l) = lines.get(i) {
        let t = l.trim();
        if t.is_empty() || !line_has_table_cells(t) || is_delimiter_row(t) {
            break;
        }
        rows.push(split_table_cells(t));
        i += 1;
    }
    Some((
        MarkdownTable {
            header,
            aligns,
            rows,
        },
        i - start,
    ))
}

fn line_has_table_cells(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.contains('|')
}

fn is_delimiter_row(line: &str) -> bool {
    let cells = split_table_cells(line);
    !cells.is_empty() && cells.iter().all(|c| is_delimiter_cell(c))
}

/// Split one `| a | b |` row into trimmed cells, tolerating missing outer
/// pipes (`a | b`).
fn split_table_cells(line: &str) -> Vec<String> {
    let t = line.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn is_delimiter_cell(cell: &str) -> bool {
    let c = cell.trim();
    let inner = c.trim_start_matches(':').trim_end_matches(':');
    !inner.is_empty() && inner.chars().all(|ch| ch == '-')
}

fn cell_align(cell: &str) -> CellAlign {
    let c = cell.trim();
    match (c.starts_with(':'), c.ends_with(':')) {
        (true, true) => CellAlign::Center,
        (false, true) => CellAlign::Right,
        _ => CellAlign::Left,
    }
}

/// Reduce a cell's inline markdown to plain display text: strip `**` emphasis
/// and collapse `[label](target)` links to their label. TUI table cells render
/// as aligned plain text — an action link inside a cell shows as its label
/// rather than being clickable (the web UI renders cell links live).
fn table_cell_text(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find(']') else {
            out.push_str(&rest[open..]);
            return out.replace("**", "");
        };
        let label = &after[..close];
        let tail = &after[close + 1..];
        if let Some(paren_inner) = tail.strip_prefix('(') {
            if let Some(end) = paren_inner.find(')') {
                out.push_str(label);
                rest = &paren_inner[end + 1..];
                continue;
            }
        }
        out.push('[');
        out.push_str(label);
        out.push(']');
        rest = tail;
    }
    out.push_str(rest);
    out.replace("**", "")
}

fn render_markdown_table(
    table: &MarkdownTable,
    theme: &Theme,
    panel_area: Rect,
) -> Vec<Line<'static>> {
    let ncols = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    if ncols == 0 {
        return Vec::new();
    }
    // Natural column widths from the plain-text header + body cells.
    let mut widths = vec![0usize; ncols];
    let measure = |s: &str| UnicodeWidthStr::width(table_cell_text(s).as_str());
    for (c, h) in table.header.iter().enumerate() {
        if c < ncols {
            widths[c] = widths[c].max(measure(h));
        }
    }
    for row in &table.rows {
        for (c, cell) in row.iter().enumerate() {
            if c < ncols {
                widths[c] = widths[c].max(measure(cell));
            }
        }
    }
    // Fit to the panel: columns join with " │ " (width 3). Shrink the widest
    // column (floor 3) until the row fits the panel's inner width.
    const SEP_W: usize = 3;
    let avail = (panel_area.width as usize).saturating_sub(2);
    let overhead = SEP_W * ncols.saturating_sub(1);
    let mut total = widths.iter().sum::<usize>() + overhead;
    while total > avail {
        let Some((idx, &w)) = widths.iter().enumerate().max_by_key(|(_, w)| **w) else {
            break;
        };
        if w <= 3 {
            break;
        }
        widths[idx] -= 1;
        total -= 1;
    }
    let mut out = Vec::with_capacity(table.rows.len() + 2);
    out.push(table_row_line(
        &table.header,
        &widths,
        &table.aligns,
        theme,
        true,
    ));
    out.push(table_rule_line(&widths, theme));
    for row in &table.rows {
        out.push(table_row_line(row, &widths, &table.aligns, theme, false));
    }
    out
}

fn table_row_line(
    cells: &[String],
    widths: &[usize],
    aligns: &[CellAlign],
    theme: &Theme,
    header: bool,
) -> Line<'static> {
    let cell_style = if header {
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text)
    };
    let sep_style = Style::default().fg(theme.dim);
    let mut spans = Vec::with_capacity(widths.len() * 2);
    for (c, &width) in widths.iter().enumerate() {
        if c > 0 {
            spans.push(Span::styled(" │ ".to_string(), sep_style));
        }
        let raw = cells.get(c).map(String::as_str).unwrap_or("");
        let align = aligns.get(c).copied().unwrap_or(CellAlign::Left);
        spans.push(Span::styled(pad_table_cell(raw, width, align), cell_style));
    }
    Line::from(spans)
}

fn table_rule_line(widths: &[usize], theme: &Theme) -> Line<'static> {
    let mut s = String::new();
    for (c, &width) in widths.iter().enumerate() {
        if c > 0 {
            s.push_str("─┼─");
        }
        s.push_str(&"─".repeat(width));
    }
    Line::from(Span::styled(s, Style::default().fg(theme.dim)))
}

/// Plain-text cell content padded (or truncated with `…`) to `width` columns
/// per `align`.
fn pad_table_cell(text: &str, width: usize, align: CellAlign) -> String {
    let content = table_cell_text(text);
    let w = UnicodeWidthStr::width(content.as_str());
    if w > width {
        // Reuse the shared truncator, then pad to the exact column width so
        // the grid stays aligned even when truncation lands on a wide-char
        // boundary.
        let mut truncated = truncate_to_width(&content, width);
        let tw = UnicodeWidthStr::width(truncated.as_str());
        if tw < width {
            truncated.push_str(&" ".repeat(width - tw));
        }
        return truncated;
    }
    let pad = width - w;
    match align {
        CellAlign::Left => format!("{content}{}", " ".repeat(pad)),
        CellAlign::Right => format!("{}{content}", " ".repeat(pad)),
        CellAlign::Center => {
            let left = pad / 2;
            format!("{}{content}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
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
const SMITH_READY_HINT: &str = "type your prompt and press Enter";

fn editor_ready_hint(
    state: Option<&crate::app::EditorState>,
    agent_status: Option<&agentd_protocol::AgentStatus>,
) -> Option<&'static str> {
    if let Some(agent_status) = agent_status.filter(|s| s.active) {
        if agent_status.active {
            return None;
        }
    }

    match state {
        Some(s) if s.buf.is_empty() && s.queued.is_empty() && s.completions.is_empty() => {
            Some(SMITH_READY_HINT)
        }
        None => Some(SMITH_READY_HINT),
        _ => None,
    }
}

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
    let ready_hint = editor_ready_hint(Some(state), agent_status);

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

    let mut cursor_pos: Option<(u16, u16)> = None;

    // Active editor — multiline and width-wrapped.
    if let Some(hint) = ready_hint {
        let wrapped = wrap_text(hint, text_width);
        for visual in wrapped {
            if remaining == 0 {
                break;
            }
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            let para = Paragraph::new(Line::from(vec![
                Span::styled("❯ ", active_glyph_style),
                Span::styled(visual.text.clone(), Style::default().fg(theme.dim)),
            ]));
            f.render_widget(para, row);
            y = y.saturating_add(1);
            remaining -= 1;
            if cursor_pos.is_none() && state.cursor == 0 {
                cursor_pos = Some((area.x.saturating_add(prompt_w), row.y));
            }
        }
        if set_cursor {
            if let Some((x, y)) = cursor_pos {
                render_editor_cursor(f, Position { x, y }, theme);
            }
        }
        return;
    }

    let buf_lines = split_preserve_empty_lines(&state.buf);
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
    let ready_hint = editor_ready_hint(state, agent_status);
    let queued_lines: usize = state
        .map(|s| {
            s.queued
                .iter()
                .map(|q| wrapped_text_rows(q, text_width))
                .sum()
        })
        .unwrap_or(0);
    let completion_lines = state.map(|s| s.completions.len()).unwrap_or(0);
    let buf_lines = if ready_hint.is_some() {
        wrapped_text_rows(SMITH_READY_HINT, text_width)
    } else {
        state
            .map(|s| wrapped_text_rows(&s.buf, text_width))
            .unwrap_or(1)
    };
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

fn render_chat(f: &mut Frame, area: Rect, app: &App) {
    // Structured-event chat mode. This intentionally ignores PTY bytes and
    // terminal-derived snapshots; Terminal view owns terminal rendering. C-x t
    // and headless sessions both use this path so transcript inspection and
    // non-PTY sessions share the same chat presentation.
    let chat_lines = chat_lines(&app.theme, &app.transcript);
    let total = chat_lines.len();
    let height = area.height as usize;
    let max_scroll = total.saturating_sub(height);
    let scroll_start = if app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        (app.transcript_scroll as usize).min(max_scroll)
    };
    let end = (scroll_start + height).min(total);
    let mut lines = chat_lines[scroll_start..end].to_vec();
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "No structured chat events for this session. Use Terminal Mode to view PTY output.",
            Style::default().fg(app.theme.dim),
        )));
    }
    let para = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(para, area);
}

fn render_modeline(f: &mut Frame, area: Rect, app: &mut App) {
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
    let approval_mode_label = s.and_then(approval_mode_modeline_label);
    let approval_mode_badge = approval_mode_label.map(|badge| format!("[{badge}]"));
    // "● remote: N" badge when at least one phone / remote client is
    // attached to the daemon. Visible signal that another surface
    // is also driving sessions, so the local user doesn't get
    // surprised by a session changing under them.
    let remote_badge = if app.remote_clients > 0 {
        format!("[● remote: {}]  ", app.remote_clients)
    } else {
        String::new()
    };
    let mut search_status = None;
    if let Some(search) = app.program_popup.as_ref().and_then(|popup| popup.search.as_ref()) {
        let selected = if search.matches.is_empty() {
            0
        } else {
            search.selected.min(search.matches.len().saturating_sub(1)) + 1
        };
        search_status = Some(if search.matches.is_empty() {
            if search.query.is_empty() {
                "I-search: ".to_string()
            } else {
                format!("Failing I-search: {}", search.query)
            }
        } else if search.query.is_empty() {
            format!("I-search ({selected}/{})", search.matches.len())
        } else {
            format!("I-search: {} ({selected}/{})", search.query, search.matches.len())
        });
    }
    let status = search_status
        .as_deref()
        .unwrap_or_else(|| app.status.as_ref().map(|(m, _)| m.as_str()).unwrap_or(""));
    let empty_hint = if s.is_none() && app.list_items().is_empty() && status.is_empty() {
        "new: C-x C-f  help: ?  palette: C-x x"
    } else {
        ""
    };
    let modeline_before_approval_mode = format!(
        " construct  focus:{focus}  {sel}  {model}  {remote}",
        focus = focus_label,
        remote = remote_badge,
        sel = match s {
            Some(s) => format!("\"{}\"", primary_label(s)),
            None => "-".into(),
        },
        model = match s {
            Some(s) => s.model.clone().unwrap_or_else(|| "-".into()),
            None => "-".into(),
        },
    );
    if approval_mode_label.is_some() {
        let start_col = area
            .x
            .saturating_add(UnicodeWidthStr::width(modeline_before_approval_mode.as_str()) as u16);
        let width = approval_mode_badge
            .as_deref()
            .map(UnicodeWidthStr::width)
            .unwrap_or(0) as u16;
        if width > 0 && start_col < area.x.saturating_add(area.width) {
            let end_col = start_col
                .saturating_add(width)
                .min(area.x.saturating_add(area.width));
            if end_col > start_col {
                app.layout.modeline_approval_mode_hit = Some(crate::app::ModelineApprovalModeHit {
                    row: area.y,
                    start_col,
                    end_col,
                });
            }
        }
    }
    let modeline_after_approval_mode = format!(
        "{scrollback}{chord}{empty_hint}{status}{conn} ",
        scrollback = scrollback_label,
        chord = if app.chord_label.is_empty() {
            String::new()
        } else {
            format!("({})  ", app.chord_label)
        },
        empty_hint = empty_hint,
        status = status,
    );
    let mut spans = Vec::new();
    spans.push(Span::raw(modeline_before_approval_mode));
    if let Some(badge) = approval_mode_badge {
        let hovered = app
            .mouse_pos
            .zip(app.layout.modeline_approval_mode_hit)
            .is_some_and(|((col, row), hit)| hit.contains(col, row));
        let badge_style = Style::default()
            .bg(app.theme.modeline_bg)
            .fg(if hovered {
                app.theme.text
            } else {
                app.theme.modeline_fg
            })
            .add_modifier(if hovered {
                Modifier::BOLD | Modifier::UNDERLINED
            } else {
                Modifier::UNDERLINED
            });
        spans.push(Span::styled(badge, badge_style));
        spans.push(Span::raw("  "));
    }
    spans.push(Span::raw(modeline_after_approval_mode));
    let para = Paragraph::new(Line::from(spans)).style(
        Style::default()
            .bg(app.theme.modeline_bg)
            .fg(app.theme.modeline_fg),
    );
    f.render_widget(para, area);

    // Persistent "update available" notice, right-aligned at the far edge of
    // the status bar so it stays visible until upgrade without crowding the
    // left-aligned modeline (transient `status` messages still render inline).
    if let Some(notice) = app.update_notice.as_deref() {
        use unicode_width::UnicodeWidthStr;
        let text = format!(" {notice} ");
        let w = UnicodeWidthStr::width(text.as_str()) as u16;
        if w > 0 && w < area.width {
            let nrect = Rect {
                x: area.x + area.width - w,
                y: area.y,
                width: w,
                height: area.height,
            };
            let np = Paragraph::new(text).style(
                Style::default()
                    .bg(app.theme.modeline_bg)
                    .fg(app.theme.modeline_fg),
            );
            f.render_widget(np, nrect);
        }
    }
}

fn approval_mode_modeline_label(s: &SessionSummary) -> Option<&'static str> {
    s.approval_mode
        .badge()
        .or_else(|| is_smith_like_harness(&s.harness).then_some("manual"))
}

fn is_smith_like_harness(name: &str) -> bool {
    matches!(name, "smith")
}

fn render_modeline_approval_mode_tooltip(f: &mut Frame, app: &App) {
    let Some(hit) = app.layout.modeline_approval_mode_hit else {
        return;
    };
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    if !hit.contains(mx, my) {
        return;
    }
    let Some(s) = app.selected_session() else {
        return;
    };
    let Some(label) = approval_mode_modeline_label(s) else {
        return;
    };
    render_button_tooltip(
        f,
        &app.theme,
        &format!(" Approval mode: {label}. Click to cycle "),
        hit.start_col,
        hit.row.saturating_sub(2),
    );
}

/// Compute how many rows the minibuffer footer occupies this frame.
/// The default footer is 1 row (palette / hints / intent prompts).
/// When the orchestrator panel is focused (its `MinibufferIntent`
/// active) it expands to a fixed cap so the embedded smith REPL has
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
        if matches!(
            mb.intent,
            MinibufferIntent::NewSessionHarness | MinibufferIntent::ForkSessionHarness { .. }
        ) {
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
                ("C-x Space program", KeyAction::OpenProgram),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o list", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::List => (
            "zoomed: list — ",
            vec![
                ("C-x x operator", KeyAction::OpenCommandPalette),
                ("C-x Space program", KeyAction::OpenProgram),
                ("C-x z unzoom", KeyAction::ToggleZoom),
                ("C-x o view", KeyAction::SwitchFocus),
            ],
        ),
        ZoomMode::None => (
            "",
            vec![
                ("C-x x operator", KeyAction::OpenCommandPalette),
                ("C-x Space program", KeyAction::OpenProgram),
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
emacs keymap (default; CONSTRUCT_KEYMAP=vim for vim profile)

  getting started
    A session is one live task or terminal that construct keeps in the list.
    A harness is the runtime for a session: smith, codex, claude, or shell.
    The left pane selects sessions; the right pane shows the selected session.
    Use C-x C-f to create a session, then choose a harness.
    Use C-x x for the command palette when you forget a shortcut.

  focus + view
    C-x o           other window (list → windows → list)
    C-2 .. C-5      focus split window 1..4 directly (C-2 = first window)
    Shift+arrow     focus the adjacent split window (in a split layout)
    RET (on list)   focus the selected session's view
    C-x 2 / C-x 3   split current main window below / right
    C-x 0 / C-x 1   delete current window / delete other windows
    C-x ^           make current window taller
    C-x } / C-x {   make current window wider / narrower
    C-x t           toggle chat ↔ terminal view
    C-x z           zoom: fill the screen with the session view
    C-n / down      next session
    C-p / up        prev session

  session actions
    C-x C-f         new session
    C-x b           switch focused window to an existing session
    C-x i           send input to selected session
    C-x k           delete selected session (confirms; kills if running)
    C-x Space       open selected session's program
    C-x d           show diff
    C-x r           rename selected session (clears title on empty submit)
    C-x f           fork selected session into a new harness (seeded w/ history)
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
    Shift-up/down   same, when the list is focused, in terminals that pass
                    Shift to arrows (iTerm2/WezTerm/Alacritty yes; macOS
                    Terminal.app no). In a focused split, Shift+arrow moves
                    focus between panes instead (see focus + view).

  mouse
    drag text       select visible TUI text and copy to terminal clipboard
    C-x m           toggle mouse capture off/on for native selection fallback

  global
    M-x / C-x x     command palette (C-x x is Meta-free)
                    palette commands: new fork send delete rename program diff border
                                      zoom interrupt refresh harnesses help
    ?               toggle this help
    C-x C-c          quit

When the right pane is showing a PTY-backed session (shell / interactive
claude / interactive codex) and focus is on the view, keystrokes go to the
child. `C-x` is the escape prefix — start any `C-x …` chord above to run
an construct command without changing focus.
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatEventKind {
    Hidden,
    AssistantMessage,
    Message(MessageRole),
    Reasoning,
    Tool,
    Metadata,
}

fn chat_event_kind(ev: &SessionEvent) -> ChatEventKind {
    match ev {
        SessionEvent::Pty { .. }
        | SessionEvent::PtyResize { .. }
        | SessionEvent::EditorState { .. }
        // Prototype: hidden like today's `tui` ToolUse. The follow-up reads
        // `slash::COMMANDS[id].render` and shows SystemNote breadcrumbs.
        | SessionEvent::ClientCommand { .. }
        | SessionEvent::ToolApprovalResolved { .. }
        | SessionEvent::ApprovalModeChanged { .. }
        | SessionEvent::OperatorLoopChanged { .. }
        | SessionEvent::ModelChanged { .. }
        | SessionEvent::AgentStatus(_) => ChatEventKind::Hidden,
        SessionEvent::Message { role, text } if should_render_chat_message(*role, text) => {
            if *role == MessageRole::Assistant {
                ChatEventKind::AssistantMessage
            } else {
                ChatEventKind::Message(*role)
            }
        }
        SessionEvent::Message { .. } => ChatEventKind::Hidden,
        SessionEvent::Reasoning { .. } => ChatEventKind::Reasoning,
        SessionEvent::ToolUse { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::ToolApprovalRequest { .. }
        | SessionEvent::TaskStart { .. }
        | SessionEvent::TaskBackgrounded { .. }
        | SessionEvent::TaskEnd { .. } => ChatEventKind::Tool,
        SessionEvent::Status { .. }
        | SessionEvent::AwaitingInput { .. }
        | SessionEvent::Cost { .. }
        | SessionEvent::Diff { .. }
        | SessionEvent::Error { .. }
        | SessionEvent::Reset
        | SessionEvent::Done { .. }
        | SessionEvent::UiPanel(_)
        | SessionEvent::UiDelete { .. }
        | SessionEvent::BrowserPreview(_)
        | SessionEvent::ContextCompacted { .. } => ChatEventKind::Metadata,
    }
}

fn chat_event_needs_gap(previous: ChatEventKind, current: ChatEventKind) -> bool {
    !matches!(
        (previous, current),
        (ChatEventKind::Tool, ChatEventKind::Tool)
            | (ChatEventKind::Metadata, ChatEventKind::Metadata)
            | (ChatEventKind::Reasoning, ChatEventKind::Reasoning)
            | (
                ChatEventKind::AssistantMessage,
                ChatEventKind::AssistantMessage
            )
    )
}

fn should_render_chat_message(role: MessageRole, text: &str) -> bool {
    let trimmed = text.trim_start();
    if role == MessageRole::Assistant && trimmed.starts_with("<permissions instructions>") {
        return false;
    }
    if role == MessageRole::User
        && trimmed.starts_with("# AGENTS.md instructions for ")
        && trimmed.contains("\n<INSTRUCTIONS>")
    {
        return false;
    }
    true
}

fn chat_lines(theme: &Theme, events: &[TimestampedEvent]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut previous_kind = ChatEventKind::Hidden;

    for ev in events {
        let kind = chat_event_kind(&ev.event);
        if kind == ChatEventKind::Hidden {
            continue;
        }
        if kind == ChatEventKind::AssistantMessage
            && previous_kind == ChatEventKind::AssistantMessage
        {
            append_chat_text_chunk(&mut lines, &ev.event);
            continue;
        }
        if kind == ChatEventKind::Reasoning && previous_kind == ChatEventKind::Reasoning {
            append_chat_text_chunk(&mut lines, &ev.event);
            continue;
        }
        if !lines.is_empty() && chat_event_needs_gap(previous_kind, kind) {
            lines.push(Line::raw(""));
        }
        lines.push(format_chat_event(theme, ev));
        previous_kind = kind;
    }

    lines
}

fn append_chat_text_chunk(lines: &mut [Line<'static>], event: &SessionEvent) {
    let Some(last) = lines.last_mut() else {
        return;
    };
    match event {
        SessionEvent::Message { text, .. } => last.spans.push(Span::raw(text.clone())),
        SessionEvent::Reasoning { text } => {
            let style = last.spans.last().map(|span| span.style).unwrap_or_default();
            last.spans.push(Span::styled(text.clone(), style));
        }
        _ => {}
    }
}

fn format_chat_event(theme: &Theme, ev: &TimestampedEvent) -> Line<'static> {
    let ts = ev.at.format("%H:%M:%S").to_string();
    let mut spans = vec![Span::styled(
        format!("[{ts}] "),
        Style::default().fg(theme.dim),
    )];
    spans.extend(format_chat_event_body(theme, &ev.event));
    Line::from(spans)
}

fn format_chat_event_body(theme: &Theme, ev: &SessionEvent) -> Vec<Span<'static>> {
    match ev {
        // Hidden events are filtered before formatting.
        SessionEvent::Pty { .. }
        | SessionEvent::PtyResize { .. }
        | SessionEvent::EditorState { .. }
        | SessionEvent::ClientCommand { .. }
        | SessionEvent::ToolApprovalResolved { .. }
        | SessionEvent::ApprovalModeChanged { .. }
        | SessionEvent::OperatorLoopChanged { .. }
        | SessionEvent::ModelChanged { .. }
        | SessionEvent::AgentStatus(_) => Vec::new(),
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
        SessionEvent::ToolUse { tool, args, .. } => {
            let args_s = serde_json::to_string(args).unwrap_or_default();
            vec![
                Span::styled("   tool: ", Style::default().fg(theme.tool)),
                Span::raw(format!("{tool}({})", shorten(&args_s, 120))),
            ]
        }
        SessionEvent::ToolResult {
            tool, ok, output, ..
        } => {
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
        SessionEvent::Cost {
            usd,
            tokens_in,
            tokens_out,
            tokens_cached,
        } => vec![Span::styled(
            format!(
                "   $ ${:.4} (in={} out={} cached={})",
                usd, tokens_in, tokens_out, tokens_cached
            ),
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
        let (main_cols, main_rows) = app.terminal_pane_size;
        if let Some(history) = app.histories.get_mut(id) {
            // Render the pin tile at the parser's CURRENT cached size when
            // it has one. Each `ItemHistory` is shared between the main
            // view and the pin tile, and `replay` resizes the cached vt100
            // parser to the requested dims — so rendering the pin at a
            // different width than the main view just used re-feeds the
            // pending chunk through a freshly-sized grid every frame
            // (~45000x slower than a no-op resize; see the regression test
            // `pin_tile_reuses_cached_size_to_avoid_split_thrash`).
            //
            // The main/split render runs earlier this frame and leaves the
            // parser sized to whichever pane is showing this session, so
            // reusing that size makes the pin's replay a no-op resize.
            // Forcing a single "main view" size used to be safe — but split
            // view gives each pane its own width, so a session shown in a
            // split pane (width A) and a pin tile (width B) thrashed the
            // shared parser on every frame: the split+pin lag. Fall back to
            // the main-view size only to seed a session with no cached
            // parser yet (pin-only, never opened in the main view).
            // `render_pty_tail` crops the rendered screen to `inner`.
            let (cols, rows) = history.cached_dims().unwrap_or((
                main_cols.max(inner.width).max(1),
                main_rows.max(inner.height).max(1),
            ));
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
/// harness status/input bars (smith, codex, claude all park them in the
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
/// area — i.e. a smith editor pane is carved out below. We keep the
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
    // Paint a slice of the vt100 screen into `area`, starting at `row_offset`.
    // Caller is responsible for clearing the target area if needed.
    if area.width == 0 || area.height == 0 {
        return;
    }
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

/// Count the number of content-bearing rows on the screen (exclusive end row).
/// Scans from the bottom up and returns the index of the last row that has
/// any cell with visible contents, plus one. Returns 0 when the screen is empty.
fn non_empty_row_span(screen: &vt100::Screen) -> u16 {
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 {
        return 0;
    }
    for r in (0..rows).rev() {
        for c in 0..cols {
            if let Some(cell) = screen.cell(r, c) {
                if cell.has_contents() {
                    return r.saturating_add(1);
                }
            }
        }
    }
    0
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
    // styled-dim text (e.g. smith's `[+N lines — click to expand]`
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
    // present, active entry means smith is working right now.
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
    // Smith reports an explicit agent-turn signal (`AgentStatus`):
    // active=true at turn start, active=false at every turn end. A smith
    // session can linger in `Running` while idle (e.g. an interrupted turn
    // that returned without flipping back to AwaitingInput), so animate
    // strictly while that turn is active — not merely because the
    // lifecycle state reads `Running`. Animating on `Running` alone was
    // the bug: an idle session kept spinning.
    //
    // Shell / PTY-only harnesses have no agent-status signal and also sit
    // in `Running` while idle, so they keep the short PTY-activity gate.
    if is_smith_like_harness(&s.harness) {
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
        SessionEvent::ToolApprovalResolved { call_id } => {
            format!("approval-resolved {call_id}")
        }
        SessionEvent::ClientCommand { id, args } => {
            format!(
                "client-cmd {id:?}{}",
                args.as_deref().map(|a| format!(" {a}")).unwrap_or_default()
            )
        }
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
        SessionEvent::ApprovalModeChanged { mode } => {
            format!("approval-mode {}", mode.badge().unwrap_or("manual"))
        }
        SessionEvent::OperatorLoopChanged { enabled } => {
            format!("operator-loop {}", if *enabled { "enabled" } else { "disabled" })
        }
        SessionEvent::ModelChanged { model } => format!("model {model}"),
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
/// is a smith interactive session; the same items-model history that
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
    // Avoid per-frame clear to limit flicker; block draw overwrites borders.
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
    // and the editor was invisible (smith stopped painting it).
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

    // Clear and bottom-align short content so the last message hugs the input.
    f.render_widget(Clear, chat_area);
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

    let mut paint_area = chat_area;
    let mut paint_row_offset = row_offset;
    if editor_area.is_some() && app.orchestrator_scrollback == 0 {
        let content_rows = non_empty_row_span(out.screen);
        if content_rows > 0 && content_rows < chat_area.height {
            let top_pad = chat_area.height - content_rows;
            paint_area.y = paint_area.y.saturating_add(top_pad);
            paint_area.height = content_rows;
            paint_row_offset = 0;
        }
    }

    render_pty_screen(
        f,
        paint_area,
        out.screen,
        &app.theme,
        editor_area.is_none(),
        paint_row_offset,
    );
    app.block_hits.insert(
        id,
        translate_block_hits(out.blocks, paint_row_offset, paint_area.height),
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

fn render_program_popup(f: &mut Frame, app: &mut App) {
    let now = Instant::now();
    app.layout.program_title_run_hit = None;
    app.layout.program_title_toggle_hit = None;
    app.layout.program_title_close_hit = None;
    app.layout.program_selection_run_hit = None;
    app.layout.program_inner_area = None;
    app.layout.program_clip_hits.clear();
    if app
        .program_popup
        .as_ref()
        .is_some_and(|popup| popup.closing && now >= popup.hide_after)
    {
        app.program_popup = None;
    }

    let active_session_id = app
        .program_popup
        .as_ref()
        .map(|popup| popup.program.session_id.clone());
    let mut popups: Vec<(crate::app::ProgramPopup, Rect, bool)> = Vec::new();
    for hit in &app.layout.main_window_areas {
        let Some(crate::app::Selection::Session(session_id)) = app.selection_for_window(hit.id)
        else {
            continue;
        };
        if active_session_id.as_deref() == Some(session_id.as_str()) {
            continue;
        }
        if let Some(popup) = app.program_popups.get(&session_id) {
            popups.push((popup.clone(), hit.area, false));
        }
    }
    if let Some(popup) = app.program_popup.as_ref() {
        let base_rect = app
            .layout
            .main_window_areas
            .iter()
            .find(|hit| hit.id == app.active_window_id)
            .map(|hit| hit.area)
            .or(app.layout.view_area)
            .unwrap_or_else(|| f.area());
        app.layout.modal_area = Some(base_rect);
        popups.push((popup.clone(), base_rect, true));
    }
    for (popup, base_rect, active) in popups {
        render_program_popup_at(f, app, &popup, base_rect, active, now);
    }
}

/// Temporal speed of the program Run shimmer wave, in radians/sec.
const PROGRAM_SHIMMER_SPEED: f32 = 4.2;
/// Spatial frequency of the shimmer wave, in radians per character. The bright
/// band spans roughly `2π / DENSITY` characters, so ~0.18 gives a highlight
/// band ~35 chars wide travelling through the running region.
const PROGRAM_SHIMMER_DENSITY: f32 = 0.18;

/// Which source lines of a program are shimmering, plus the wave phase (spec
/// 0042). Derived from the session's `ProgramRun` at render time.
struct ProgramShimmer {
    /// Indexed by source-line; `true` => the line is in a still-running block.
    active_lines: Vec<bool>,
    /// Phase of the travelling highlight wave, in radians.
    phase: f32,
}

/// Build the shimmer overlay for a popup from its session's `ProgramRun`, or
/// `None` if no run is active, it has lapsed, or every block has settled. A
/// block shimmers while its stable id is in the run's pending set (spec 0053);
/// editing a block changes its id, taking it out of the animation by default.
fn program_run_shimmer(
    app: &App,
    popup: &crate::app::ProgramPopup,
    now: Instant,
) -> Option<ProgramShimmer> {
    let run = app.program_runs.get(&popup.program.session_id)?;
    if now >= run.deadline {
        return None;
    }
    let mut active_lines = vec![false; popup.buffer.lines().count()];
    let mut any = false;
    for block in crate::app::program_blocks(&popup.buffer) {
        if run.pending.contains(&block.id) {
            for slot in active_lines
                .iter_mut()
                .take(block.end_line)
                .skip(block.start_line)
            {
                *slot = true;
                any = true;
            }
        }
    }
    if !any {
        return None;
    }
    let phase =
        now.saturating_duration_since(run.started_at).as_secs_f32() * PROGRAM_SHIMMER_SPEED;
    Some(ProgramShimmer { active_lines, phase })
}

/// Overlay the Run shimmer onto already-rendered program lines: for each active
/// line, re-emit its text character-by-character with a brightness drawn from a
/// travelling wave, so a highlight band sweeps through the running region. The
/// global character index advances across active lines so the band is
/// continuous down the document. Spans carrying a background (smart-clip chips,
/// selection) are left intact but still advance the wave so its spacing holds.
fn apply_program_shimmer(lines: &mut [Line], shimmer: &ProgramShimmer, theme: &Theme) {
    let mut gidx: usize = 0;
    for (i, line) in lines.iter_mut().enumerate() {
        if !shimmer.active_lines.get(i).copied().unwrap_or(false) {
            continue;
        }
        let mut new_spans = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            if span.style.bg.is_some() {
                gidx += span.content.chars().count();
                new_spans.push(span);
                continue;
            }
            let style = span.style;
            for ch in span.content.chars() {
                let w = (shimmer.phase - gidx as f32 * PROGRAM_SHIMMER_DENSITY).sin();
                // 0..1, eased so most of the region rests dim and the crest pops.
                let t = (0.5 + 0.5 * w).clamp(0.0, 1.0);
                let eased = t * t * (3.0 - 2.0 * t);
                let mut st = style.fg(blend_color(theme.muted, theme.text, eased));
                if eased > 0.85 {
                    st = st.add_modifier(Modifier::BOLD);
                }
                new_spans.push(Span::styled(ch.to_string(), st));
                gidx += 1;
            }
        }
        line.spans = new_spans;
    }
}

/// Build the empty-program onboarding placeholder: a one-line description of what
/// the program is, every non-blank template as a clickable button wrapped across
/// as many rows as fit, a divider, and a smart-clip syntax reference.
/// Returns the lines to render plus the button hitboxes. Coordinates are absolute
/// screen cells — safe because an empty program never scrolls (offset is always 0)
/// and every line is kept within `inner.width`, so no wrapping shifts the rows.
/// Templates that don't fit the available height collapse into a "+N more"
/// indicator. Falls back to a plain description+syntax when the program is too
/// small for any button or no templates are available.
fn program_empty_placeholder(
    theme: &crate::theme::Theme,
    templates: &[agentd_protocol::ProgramTemplate],
    inner: Rect,
) -> (Vec<Line<'static>>, Vec<crate::app::ProgramTemplateHit>) {
    let dim = Style::default().fg(theme.dim);
    let width = inner.width as usize;
    const DESC: &str =
        "Program — a shared Markdown space you and your agents edit and run together.";
    // The syntax cheat mirrors what built-in templates demonstrate: harness
    // clips delegate, session clips embed, selection+Run dispatches, and `:::clip`
    // fences group output.
    const SYNTAX: &str =
        "Syntax: @{session:id} embeds a session · @{harness:name} delegates · select + Run dispatches · :::clip … ::: groups output.";

    let desc_line = Line::from(Span::styled(truncate_to_width(DESC, width), dim));
    let syntax_line = Line::from(Span::styled(truncate_to_width(SYNTAX, width), dim));
    let divider = Line::from(Span::styled("─".repeat(width), dim));

    let plain = || {
        let lines = vec![
            desc_line.clone(),
            Line::from(""),
            divider.clone(),
            Line::from(""),
            syntax_line.clone(),
        ];
        (lines, Vec::new())
    };

    const INDENT: usize = 2;
    const GAP: usize = 3;
    const MAX_LABEL: usize = 20;

    // Every non-blank template becomes a button — "blank" *is* the empty state, so
    // offering it would be a no-op. Order by name (case-insensitive).
    let mut ordered: Vec<&agentd_protocol::ProgramTemplate> =
        templates.iter().filter(|t| t.id != "blank").collect();
    ordered.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let total = ordered.len();

    // Greedily pack the buttons into rows no wider than the pane. A button too wide
    // to fit even alone on a row is dropped (it counts toward the overflow tally).
    struct Btn<'a> {
        label: String,
        inner_w: usize,
        t: &'a agentd_protocol::ProgramTemplate,
    }
    let mut rows: Vec<Vec<Btn>> = Vec::new();
    let mut cur: Vec<Btn> = Vec::new();
    let mut used = INDENT;
    for t in &ordered {
        let label = format!(" {} ", truncate_to_width(&t.name, MAX_LABEL));
        let inner_w = UnicodeWidthStr::width(label.as_str());
        let box_w = inner_w + 2; // side borders
        if INDENT + box_w > width {
            continue; // never fits — dropped, surfaced via "+N more"
        }
        let needed = if cur.is_empty() { box_w } else { GAP + box_w };
        if used + needed > width && !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            used = INDENT + box_w;
        } else {
            used += needed;
        }
        cur.push(Btn { label, inner_w, t });
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    if rows.is_empty() {
        return plain();
    }
    let packed: usize = rows.iter().map(|r| r.len()).sum();

    // Height budget. Header = desc + blank (2). Footer = blank + divider + blank +
    // syntax (4). Each button row spans 3 lines. Reserve one line for the "+N more"
    // indicator whenever some template can't be shown — whether dropped for width or
    // trimmed for height.
    let header = 2usize;
    let footer = 4usize;
    let avail = (inner.height as usize).saturating_sub(header + footer);
    if avail < 3 {
        return plain();
    }
    let max_rows = avail / 3;
    let fits_all = rows.len() <= max_rows && packed == total;
    let (shown_rows, reserve_overflow) = if fits_all {
        (rows.len(), false)
    } else {
        let with_overflow_rows = avail.saturating_sub(1) / 3;
        if with_overflow_rows == 0 {
            return plain();
        }
        (rows.len().min(with_overflow_rows), true)
    };

    let border = Style::default().fg(theme.accent);
    let label_style = Style::default().fg(theme.text).add_modifier(Modifier::BOLD);
    let indent = || Span::styled(" ".repeat(INDENT), Style::default());
    let gap = || Span::styled(" ".repeat(GAP), Style::default());

    let mut button_lines: Vec<Line<'static>> = Vec::new();
    let mut hits = Vec::new();
    let mut shown_buttons = 0usize;
    // Button rows begin at line index 2 (after the description + blank). Each row
    // occupies 3 contiguous lines, so row `r`'s box spans absolute rows
    // `inner.y + 2 + r*3 ..= +2`.
    for (r, row) in rows.iter().take(shown_rows).enumerate() {
        let mut top = vec![indent()];
        let mut mid = vec![indent()];
        let mut bot = vec![indent()];
        let row_start = inner.y + 2 + (r as u16) * 3;
        let row_end = row_start + 2;
        let mut col = inner.x as usize + INDENT;
        for (i, btn) in row.iter().enumerate() {
            if i > 0 {
                top.push(gap());
                mid.push(gap());
                bot.push(gap());
                col += GAP;
            }
            let bar = "─".repeat(btn.inner_w);
            top.push(Span::styled(format!("┌{bar}┐"), border));
            mid.push(Span::styled("│".to_string(), border));
            mid.push(Span::styled(btn.label.clone(), label_style));
            mid.push(Span::styled("│".to_string(), border));
            bot.push(Span::styled(format!("└{bar}┘"), border));
            let box_w = btn.inner_w + 2;
            hits.push(crate::app::ProgramTemplateHit {
                col_start: col as u16,
                col_end: (col + box_w) as u16,
                row_start,
                row_end,
                template_id: btn.t.id.clone(),
                markdown: btn.t.markdown.clone(),
            });
            col += box_w;
            shown_buttons += 1;
        }
        button_lines.push(Line::from(top));
        button_lines.push(Line::from(mid));
        button_lines.push(Line::from(bot));
    }

    let mut lines = vec![desc_line, Line::from("")];
    lines.append(&mut button_lines);
    if reserve_overflow {
        let remaining = total.saturating_sub(shown_buttons);
        if remaining > 0 {
            lines.push(Line::from(Span::styled(
                truncate_to_width(&format!("  +{remaining} more"), width),
                dim,
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(divider);
    lines.push(Line::from(""));
    lines.push(syntax_line);
    (lines, hits)
}

fn render_program_popup_at(
    f: &mut Frame,
    app: &mut App,
    popup: &crate::app::ProgramPopup,
    base_rect: Rect,
    active: bool,
    now: Instant,
) {
    if base_rect.width < 40 || base_rect.height < 8 {
        return;
    }

    let progress = if popup.closing {
        popup
            .hide_after
            .saturating_duration_since(now)
            .as_secs_f32()
            / PROGRAM_REVEAL_SECS
    } else {
        now.saturating_duration_since(popup.revealed_at)
            .as_secs_f32()
            / PROGRAM_REVEAL_SECS
    }
    .clamp(0.0, 1.0);
    if progress <= 0.0 {
        return;
    }
    let visible_h = ((base_rect.height as f32 * progress).ceil() as u16)
        .clamp(1, base_rect.height);
    if visible_h == 0 {
        return;
    }
    let rect = Rect {
        height: visible_h,
        ..base_rect
    };

    let summary = app
        .sessions
        .iter()
        .find(|s| s.id == popup.program.session_id)
        .cloned();
    let summary_ref = summary.as_ref();

    // Left cluster: mode glyph + session label + the Run button (now wedged
    // between the name and the dirty marker). Right cluster (widgets, harness,
    // close) is shared with the normal session view via
    // `apply_pane_title_right_cluster`, so the two title bars can't drift in
    // layout, styling, or geometry. The program can always be dismissed, so it
    // always offers a close button.
    let show_close = true;
    let dirty = popup.buffer != popup.saved_markdown;
    let left = program_title_left_layout(
        summary_ref,
        short_id(&popup.program.session_id),
        rect,
        dirty,
        show_close,
    );
    let title = program_title_line(app, popup, active, now, &left);
    let title_toggle_hit = program_title_toggle_button_range(summary_ref, rect);

    let border_style = program_border_style(&app.theme, active);
    // The session-actions ☰ icon should read as part of the program frame, so its
    // base hue tracks the program border color (accent_alt) rather than the
    // default session-view close color. Focus dimming + hover still compose via
    // `session_menu_icon_style` (focused → border hue, unfocused → dimmed, hover
    // wins).
    let menu_icon_color = border_style.fg.unwrap_or(app.theme.accent_alt);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(title);
    let block = apply_pane_title_right_cluster(
        app,
        rect,
        summary_ref,
        border_style,
        show_close,
        true,
        active,
        menu_icon_color,
        block,
    );
    if active {
        // Run lives in the left cluster; the close button and widget icons reuse
        // the shared session-view geometry (`view_close_button_range` and
        // `dynamic_ui_widget_hits` from `render_session_widget_title`) so the
        // program click handlers in `app.rs` line up with what's painted.
        app.layout.program_title_run_hit = left.run;
        app.layout.program_title_toggle_hit = title_toggle_hit;
        app.layout.program_title_close_hit = show_close.then(|| view_close_button_range(rect));
    }
    // Area inside the program's border (title bar excluded), mirroring the
    // session view's `block.inner(area)`. The content body adds extra padding on
    // top of this, but the sticky-widget popover must use the un-padded border
    // inner so its top sits exactly one row below the title bar — the same
    // y-position as the normal session view (see the widget reveal below).
    let block_inner = block.inner(rect);
    let inner = block_inner.inner(Margin {
        horizontal: PROGRAM_CONTENT_PADDING_X,
        vertical: PROGRAM_CONTENT_PADDING_Y,
    });
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let selection = program_selection_range(popup);
    let search = popup.search.as_ref().filter(|search| !search.matches.is_empty());
    let search_matches = search.map(|search| search.matches.as_slice());
    let search_selected = search.map(|search| search.selected);
    let mut lines = render_program_markdown_lines(
        app,
        &popup.buffer,
        selection,
        search_matches,
        search_selected,
    );
    // Run shimmer (spec 0042): while a program Run is executing for this session,
    // sweep a highlight through the blocks that have not settled yet.
    if let Some(shimmer) = program_run_shimmer(app, popup, now) {
        apply_program_shimmer(&mut lines, &shimmer, &app.theme);
    }
    // Empty program: replace the bare body with a richer onboarding placeholder —
    // a one-line description, clickable template buttons, a divider, and a tip.
    // The button hitboxes are returned so the active program can publish them for
    // the mouse handler. Non-empty programes get no hits.
    let placeholder_hits = if lines.is_empty() {
        let (placeholder_lines, hits) =
            program_empty_placeholder(&app.theme, &app.program_templates, inner);
        lines = placeholder_lines;
        hits
    } else {
        Vec::new()
    };
    // Vertical scroll: the body can exceed `inner.height` wrapped rows. Clamp
    // the popup's stored offset to the current geometry (content edits or a
    // resize may have shrunk the scrollable range) and skip that many wrapped
    // rows when rendering. `Paragraph::scroll` with `Wrap` skips *wrapped* rows,
    // matching the wrapped-row coordinate space the cursor math uses.
    let viewport_rows = inner.height as usize;
    let total_rows = program_total_visual_rows(Some(app), &popup.buffer, inner.width as usize);
    let max_scroll = total_rows.saturating_sub(viewport_rows);
    let scroll_offset = popup.scroll_offset.min(max_scroll);
    if active {
        // Remember the live viewport so cursor-move handlers can keep the caret
        // visible on the next keystroke, and persist the clamped offset.
        app.layout.program_inner_area = Some(inner);
        // Publish (or clear) the empty-state template buttons. Only the active
        // program owns the hitboxes, so a click never targets an inactive split.
        app.layout.program_template_hits = placeholder_hits;
        if let Some(real) = app.program_popup.as_mut() {
            real.scroll_offset = scroll_offset;
        }
    }
    let para = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((scroll_offset.min(u16::MAX as usize) as u16, 0));
    f.render_widget(para, inner);
    render_program_scroll_indicator(
        f,
        &app.theme,
        rect,
        inner,
        scroll_offset,
        total_rows,
        viewport_rows,
    );
    // Reveal the session's hovered/pinned sticky widgets on top of the program,
    // mirroring the normal session view. The title-bar squares are painted by
    // `apply_pane_title_right_cluster` above (which arms `dynamic_ui_hover` on
    // hover and registers pin hits), but the program's own `Clear` wipes the
    // widget body the session view drew underneath — so without re-rendering it
    // here the widget never appears while the program is shown. Only the active
    // program drives the single hover/scroll/popover layout state.
    if active {
        let panels = session_sticky_widget_panels(app, &popup.program.session_id);
        if !panels.is_empty() {
            // Pass the border-stripped inner rect (not the full `rect`) so the
            // popover starts below the title bar, matching the session view. Using
            // the full `rect` here put the widget's top on the title row, hiding the
            // □/■ squares and the other title-bar controls.
            render_visible_dynamic_ui_panels(
                f,
                block_inner,
                app,
                &popup.program.session_id,
                &panels,
            );
        }
    }
    // Capture session-clip hitboxes for this program so hover can work for any
    // visible program, even when another split is focused. Only the active program
    // publishes click hitboxes into layout state.
    let clip_hits = program_session_clip_hits(Some(app), &popup.buffer, scroll_offset, inner);
    if active {
        app.layout.program_clip_hits = clip_hits.clone();
    }
    if active && !popup.closing {
        if let Some(pos) =
            program_cursor_position(Some(app), &popup.buffer, popup.cursor, scroll_offset, inner)
        {
            render_editor_cursor(f, pos, &app.theme);
            render_program_smart_clip_picker(f, app, popup, pos, inner);
        }
    }
    if active && !popup.closing {
        render_program_selection_context_menu(f, app, popup, scroll_offset, inner);
    }
    render_program_title_tooltip(f, app, popup, summary_ref, rect);
    if !popup.closing {
        render_program_clip_hover(f, app, rect, &clip_hits);
    }
    if !popup.closing {
        render_program_shimmer_hover(f, app, popup, rect, scroll_offset, inner, now);
    }
}

/// Mini session-preview popover shown while the mouse hovers a `@{session:id}`
/// smart-clip in the program body. Reads the freshly captured clip hitboxes,
/// resolves the hovered session, and paints the shared session card anchored to
/// the hovered chip. Persists for as long as the chip is hovered.
fn render_program_clip_hover(
    f: &mut Frame,
    app: &mut App,
    modal: Rect,
    hits: &[crate::app::ProgramClipHit],
) {
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    let Some(session_id) = hits
        .iter()
        .find(|hit| hit.contains(mx, my))
        .map(|hit| hit.session_id.clone())
    else {
        return;
    };
    render_session_hover_card(f, app, modal, &session_id, mx, my);
}

/// Lay out the floating session hover card so it reads as a landscape tile: its
/// width always exceeds its height when the available `max_w` allows. Returns
/// `(width, height)` including borders.
fn session_hover_card_size(content_w: u16, content_h: u16, max_w: u16) -> (u16, u16) {
    let height = content_h.saturating_add(2);
    let cap = max_w.max(3);
    let base = content_w.saturating_add(2).clamp(3, cap);
    // Force landscape: at least one column wider than the card is tall, bounded
    // by the room available. When the modal is too narrow to satisfy this the
    // caller's fit check drops the card entirely.
    let width = base.max(height.saturating_add(1)).min(cap);
    (width, height)
}

fn session_hover_card_rect(
    modal: Rect,
    width: u16,
    height: u16,
    anchor_col: u16,
    anchor_row: u16,
) -> Option<Rect> {
    if modal.height < height || modal.width < width {
        return None;
    }
    let modal_bottom = modal.y.saturating_add(modal.height);
    let modal_right = modal.x.saturating_add(modal.width);
    let y = if anchor_row.saturating_add(1).saturating_add(height) <= modal_bottom {
        anchor_row.saturating_add(1)
    } else {
        anchor_row.saturating_sub(height)
    };
    let y = y.clamp(modal.y, modal_bottom.saturating_sub(height));
    let x = anchor_col
        .min(modal_right.saturating_sub(width))
        .max(modal.x);
    Some(Rect {
        x,
        y,
        width,
        height,
    })
}

/// Render the floating session hover card — a live tail of the session's PTY
/// output — anchored just below `(anchor_col, anchor_row)` (or above it when
/// there's no room) and kept inside `modal`. Clears its own area so it overlays
/// the program body without disturbing it. Shared by the clip-chip hover and the
/// shimmer-text hover; always laid out wider than it is tall.
fn render_session_hover_card(
    f: &mut Frame,
    app: &mut App,
    modal: Rect,
    session_id: &str,
    anchor_col: u16,
    anchor_row: u16,
) {
    let Some(_s) = app.sessions.iter().find(|s| s.id == session_id) else {
        return;
    };

    let max_w = modal
        .width
        .saturating_sub(2)
        .clamp(1, PROGRAM_CLIP_HOVER_PREVIEW_COLS);
    let preview_output = app
        .histories
        .get_mut(session_id)
        .map(|history| history.replay(max_w.max(1), PROGRAM_CLIP_HOVER_PREVIEW_ROWS, 0));
    let Some(out) = preview_output else {
        return;
    };
    if non_empty_row_span(out.screen) == 0 {
        return;
    }

    let content_w = max_w;
    let content_h = PROGRAM_CLIP_HOVER_PREVIEW_ROWS;
    let (width, height) = session_hover_card_size(content_w, content_h, max_w);
    let Some(area) = session_hover_card_rect(modal, width, height, anchor_col, anchor_row) else {
        return;
    };

    f.render_widget(Clear, area);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent_alt)),
        area,
    );
    let inner = Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    render_pty_tail(
        f,
        Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height,
        },
        out.screen,
        &app.theme,
    );
}

/// Session-preview popover shown while the mouse hovers *shimmering* program text
/// — a block still running under a program Run. Resolves the session clip inside
/// the shimmering block under the cursor and paints the shared session card with
/// that session's live output. Unlike the clip-chip hover, it self-dismisses
/// once the pointer has been still for [`PROGRAM_SHIMMER_HOVER_IDLE`].
fn render_program_shimmer_hover(
    f: &mut Frame,
    app: &mut App,
    popup: &crate::app::ProgramPopup,
    _modal: Rect,
    scroll_offset: usize,
    body: Rect,
    now: Instant,
) {
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    // Transient preview: only while the pointer is actively moving. Once it has
    // been still for the idle window, hide it.
    let moved_recently = app
        .last_mouse_move
        .is_some_and(|t| now.saturating_duration_since(t) < PROGRAM_SHIMMER_HOVER_IDLE);
    if !moved_recently {
        return;
    }
    // A clip chip under the cursor is owned by `render_program_clip_hover`; don't
    // double-render the card on top of it.
    if app
        .layout
        .program_clip_hits
        .iter()
        .any(|hit| hit.contains(mx, my))
    {
        return;
    }
    let Some(shimmer) = program_run_shimmer(app, popup, now) else {
        return;
    };
    let Some(block_id) = program_shimmer_block_at(
        Some(app),
        &popup.buffer,
        &shimmer.active_lines,
        scroll_offset,
        body,
        mx,
        my,
    ) else {
        return;
    };
    let tooltip = app
        .program_runs
        .get(&popup.program.session_id)
        .and_then(|run| run.pending_tooltips.get(&block_id))
        .map_or(agentd_protocol::PROGRAM_SHIMMER_FALLBACK_TOOLTIP, |t| t.as_str());
    render_tooltip_at(f, &app.theme, tooltip, mx, my, 2, -1);
}

/// For each source line of `markdown`, the session id of the shimmering block it
/// belongs to (resolved from the first `@{session:…}` clip in that block), or
/// `None` for lines outside a shimmering block and shimmering blocks with no
/// session clip. A block "shimmers" when any of its lines is set in
/// `active_lines` (the per-source-line mask from [`program_run_shimmer`]).
fn program_shimmer_line_sessions(
    app: Option<&App>,
    markdown: &str,
    active_lines: &[bool],
) -> Vec<Option<String>> {
    let lines: Vec<&str> = markdown.lines().collect();
    let mut out = vec![None; lines.len()];
    for block in crate::app::program_blocks(markdown) {
        let shimmering = (block.start_line..block.end_line)
            .any(|li| active_lines.get(li).copied().unwrap_or(false));
        if !shimmering {
            continue;
        }
        let sid = (block.start_line..block.end_line).find_map(|li| {
            let raw = lines.get(li)?;
            let (_rendered, clips) = program_rendered_line_with_clips(app, raw);
            clips.iter().find_map(|clip| {
                let (kind, id) = program_smart_clip_target(&clip.raw_clip);
                (kind == "session").then(|| id.to_string())
            })
        });
        if let Some(id) = sid {
            for li in block.start_line..block.end_line {
                if let Some(slot) = out.get_mut(li) {
                    *slot = Some(id.clone());
                }
            }
        }
    }
    out
}

/// Resolve a screen cell over the program body to the session id of the
/// shimmering block under it, or `None` when the cell isn't on a shimmering
/// block that references a session. Mirrors the wrap/scroll math of
/// [`program_session_clip_hits`] so hover targets line up with what's painted.
fn program_shimmer_session_at(
    app: Option<&App>,
    markdown: &str,
    line_sessions: &[Option<String>],
    scroll_offset: usize,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<String> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if col < area.x || col >= area.x.saturating_add(area.width) {
        return None;
    }
    if row < area.y || row >= area.y.saturating_add(area.height) {
        return None;
    }
    let target_abs_row = scroll_offset.saturating_add((row - area.y) as usize);
    let width = area.width as usize;
    let mut visual_row_base = 0usize;
    for (i, raw) in markdown.lines().enumerate() {
        let (rendered, _clips) = program_rendered_line_with_clips(app, raw);
        let rows = program_wrap_row_starts(&rendered, width).len();
        let next_base = visual_row_base.saturating_add(rows);
        if target_abs_row >= visual_row_base && target_abs_row < next_base {
            return line_sessions.get(i).cloned().flatten();
        }
        visual_row_base = next_base;
    }
    None
}

fn program_shimmer_block_at(
    app: Option<&App>,
    markdown: &str,
    active_lines: &[bool],
    scroll_offset: usize,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<String> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    if col < area.x || col >= area.x.saturating_add(area.width) {
        return None;
    }
    if row < area.y || row >= area.y.saturating_add(area.height) {
        return None;
    }
    let target_abs_row = scroll_offset.saturating_add((row - area.y) as usize);
    let width = area.width as usize;
    let mut visual_row_base = 0usize;
    let mut source_line = None;
    for (i, raw) in markdown.lines().enumerate() {
        let (rendered, _clips) = program_rendered_line_with_clips(app, raw);
        let rows = program_wrap_row_starts(&rendered, width).len();
        let next_base = visual_row_base.saturating_add(rows);
        if target_abs_row >= visual_row_base && target_abs_row < next_base {
            source_line = Some(i);
            break;
        }
        visual_row_base = next_base;
    }
    let source_line = source_line?;
    if !active_lines.get(source_line).copied().unwrap_or(false) {
        return None;
    }
    crate::app::program_blocks(markdown)
        .into_iter()
        .find(|block| (block.start_line..block.end_line).contains(&source_line))
        .map(|block| block.id)
}

/// Paint a slim vertical scroll thumb on the program popup's right border when
/// the body overflows its viewport. Like the terminal scrollback bar, it tints
/// only the cell background so the border glyph underneath stays intact, and it
/// sits on the border column so it never clobbers body text.
fn render_program_scroll_indicator(
    f: &mut Frame,
    theme: &Theme,
    rect: Rect,
    inner: Rect,
    scroll_offset: usize,
    total_rows: usize,
    viewport_rows: usize,
) {
    if viewport_rows == 0 || inner.height == 0 || rect.width == 0 || total_rows <= viewport_rows {
        return;
    }
    let track_h = inner.height as usize;
    let max_scroll = total_rows.saturating_sub(viewport_rows);
    let thumb_h = ((viewport_rows * track_h + total_rows - 1) / total_rows).clamp(1, track_h);
    let max_thumb_top = track_h.saturating_sub(thumb_h);
    let thumb_top = if max_scroll == 0 {
        0
    } else {
        (scroll_offset.min(max_scroll) * max_thumb_top) / max_scroll
    };
    let x = rect.x + rect.width.saturating_sub(1);
    let track_color = blend_color(Color::Black, theme.text, 0.30);
    let thumb_color = blend_color(Color::Black, theme.text, 0.80);
    for row in 0..track_h {
        let y = inner.y + row as u16;
        let color = if row >= thumb_top && row < thumb_top + thumb_h {
            thumb_color
        } else {
            track_color
        };
        if let Some(cell) = f.buffer_mut().cell_mut(Position { x, y }) {
            cell.set_bg(color);
        }
    }
}

fn program_border_style(theme: &Theme, active: bool) -> Style {
    if active {
        Style::default()
            .fg(theme.accent_alt)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(theme.accent_alt)
            .add_modifier(Modifier::DIM)
    }
}

/// Column geometry for the program title bar's LEFT cluster — the truncated
/// session label, the Run button (now wedged between the name and the dirty
/// marker), and the `modified` marker. Both the title renderer and the tooltip
/// hit-tester derive positions from this so they can't drift.
struct ProgramTitleLeft {
    /// Truncated session label (or short id when the session isn't summarized).
    label: String,
    /// Run-button hit range `(x_start, x_end_exclusive, y)`, or `None` when the
    /// pane is too narrow to fit it.
    run: Option<(u16, u16, u16)>,
    /// `modified` word hit range `(x_start, x_end_exclusive)` on row `rect.y`,
    /// or `None` when the program is not dirty.
    modified: Option<(u16, u16)>,
}

fn program_title_left_layout(
    summary: Option<&agentd_protocol::SessionSummary>,
    fallback_label: &str,
    rect: Rect,
    dirty: bool,
    show_close: bool,
) -> ProgramTitleLeft {
    let glyph_w = UnicodeWidthStr::width(program_mode_glyph());
    let run_w = UnicodeWidthStr::width(PROGRAM_RUN_BUTTON);
    let marker_w = if dirty {
        UnicodeWidthStr::width(" * modified")
    } else {
        0
    };
    let harness_w = summary
        .map(|s| 2 + UnicodeWidthStr::width(harness_label(s).as_str()))
        .unwrap_or(0);
    let close_w = if show_close { 3 } else { 0 };
    // Mirror the session view's title-label budget (corners + harness + close +
    // ` <glyph> <label> ` scaffolding), and additionally reserve the
    // program-only left-cluster extras: the Run button and the dirty marker.
    let label_budget = (rect.width as usize)
        .saturating_sub(2)
        .saturating_sub(harness_w)
        .saturating_sub(close_w)
        .saturating_sub(3 + glyph_w)
        .saturating_sub(run_w)
        .saturating_sub(marker_w);
    let label = match summary {
        Some(s) => truncate_to_width(&primary_label(s), label_budget),
        None => truncate_to_width(fallback_label, label_budget),
    };
    let label_w = UnicodeWidthStr::width(label.as_str());
    // The Run button starts right after ` <glyph> <label>`; the title is inset
    // one cell from the left border corner.
    let run_x_start = rect
        .x
        .saturating_add(1) // left border corner
        .saturating_add(1) // leading space
        .saturating_add(glyph_w as u16)
        .saturating_add(1) // space after glyph
        .saturating_add(label_w as u16);
    let run_x_end = run_x_start.saturating_add(run_w as u16);
    let pane_right = rect.x.saturating_add(rect.width);
    let run = (run_x_end < pane_right).then_some((run_x_start, run_x_end, rect.y));
    // The dirty marker trails the Run button (or a one-cell gap when Run didn't
    // fit): ` <run>* modified` / ` <label> * modified`.
    let gap_after_label = if run.is_some() { run_w as u16 } else { 1 };
    let modified = dirty.then(|| {
        let start = run_x_start
            .saturating_add(gap_after_label)
            .saturating_add(UnicodeWidthStr::width("* ") as u16);
        let end = start.saturating_add(UnicodeWidthStr::width("modified") as u16);
        (start, end)
    });
    ProgramTitleLeft {
        label,
        run,
        modified,
    }
}

fn program_title_line<'a>(
    app: &App,
    popup: &crate::app::ProgramPopup,
    active: bool,
    now: Instant,
    left: &ProgramTitleLeft,
) -> Line<'a> {
    let dirty = popup.buffer != popup.saved_markdown;
    let toggle_glyph = program_mode_glyph();
    let border_style = program_border_style(&app.theme, active);
    let program_style = program_toggle_style(app, popup, active);
    let modified_style = Style::default()
        .fg(app.theme.warning)
        .add_modifier(Modifier::BOLD);
    // Run button bolds while hovered, mirroring the other title controls.
    let run_hovered = left
        .run
        .zip(app.mouse_pos)
        .is_some_and(|((xs, xe, y), (mx, my))| my == y && mx >= xs && mx < xe);
    // A run in flight pulses the Run glyph, so there's a running cue even once
    // every block has settled but the owning turn is still going (spec 0042).
    let run_started = app
        .program_runs
        .get(&popup.program.session_id)
        .filter(|run| now < run.deadline && !run.first_output_seen)
        .map(|run| run.started_at);
    let run_style = if run_hovered {
        Style::default()
            .fg(app.theme.text)
            .add_modifier(Modifier::BOLD)
    } else if let Some(started) = run_started {
        let phase = now.saturating_duration_since(started).as_secs_f32() * PROGRAM_SHIMMER_SPEED;
        let t = (0.5 + 0.5 * phase.sin()).clamp(0.0, 1.0);
        Style::default()
            .fg(blend_color(app.theme.accent, app.theme.text, t))
            .add_modifier(Modifier::BOLD)
    } else {
        let fg = if active {
            app.theme.accent
        } else {
            app.theme.muted
        };
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    };

    let mut spans = vec![
        Span::styled(" ", border_style),
        Span::styled(toggle_glyph.to_string(), program_style),
        Span::styled(" ", border_style),
        Span::styled(left.label.clone(), border_style),
    ];
    // Run button sits in the left cluster, between the session name and the
    // dirty marker (rendered only when it actually fits the pane).
    if left.run.is_some() {
        let run_button = if run_started.is_some() {
            format!(" {} ", app.spinner_frame())
        } else {
            PROGRAM_RUN_BUTTON.to_string()
        };
        spans.push(Span::styled(run_button, run_style));
    } else {
        spans.push(Span::styled(" ", border_style));
    }
    if dirty {
        spans.push(Span::styled("* ", border_style));
        spans.push(Span::styled("modified", modified_style));
    }
    spans.push(Span::styled(" ", border_style));
    Line::from(spans)
}

fn program_mode_glyph() -> &'static str {
    "▣"
}

fn program_toggle_style(
    app: &App,
    popup: &crate::app::ProgramPopup,
    active: bool,
) -> Style {
    let style = if popup.closing {
        Style::default().fg(app.theme.muted)
    } else if active {
        Style::default().fg(app.theme.accent_alt)
    } else {
        Style::default()
            .fg(app.theme.accent_alt)
            .add_modifier(Modifier::DIM)
    };
    style.add_modifier(Modifier::BOLD)
}

fn program_title_toggle_button_range(
    summary: Option<&agentd_protocol::SessionSummary>,
    rect: Rect,
) -> Option<(u16, u16, u16)> {
    let toggle_w = UnicodeWidthStr::width(program_mode_glyph()) as u16;
    if toggle_w == 0 || rect.width < toggle_w.saturating_add(2) {
        return None;
    }
    let harness_w = summary
        .map(|s| 2 + UnicodeWidthStr::width(harness_label(s).as_str()) as u16)
        .unwrap_or(0);
    let x_start = rect.x.saturating_add(2);
    let x_end = x_start.saturating_add(toggle_w);
    let max_x = rect
        .x
        .saturating_add(rect.width)
        .saturating_sub(1)
        .saturating_sub(harness_w);
    if x_end >= max_x {
        return None;
    }
    Some((x_start, x_end, rect.y))
}

fn render_program_title_tooltip(
    f: &mut Frame,
    app: &App,
    popup: &crate::app::ProgramPopup,
    summary: Option<&agentd_protocol::SessionSummary>,
    rect: Rect,
) {
    let Some((mx, my)) = app.mouse_pos else {
        return;
    };
    if my != rect.y {
        return;
    }
    if let Some((xs, xe, y)) = app.layout.program_title_toggle_hit {
        if my == y && mx >= xs && mx < xe {
            let mode = if popup.closing { "Chat" } else { "Program" };
            let action = if popup.closing {
                "open program"
            } else {
                "return to chat"
            };
            render_button_tooltip(
                f,
                &app.theme,
                &format!(" {mode} mode. Click to {action}. C-x Space "),
                mx,
                my,
            );
            return;
        }
    }
    if let Some((xs, xe, y)) = app.layout.program_title_run_hit {
        if my == y && mx >= xs && mx < xe {
            render_button_tooltip(f, &app.theme, " Run program · C-x C-r ", mx, my);
            return;
        }
    }
    if let Some((xs, xe, y)) = app.layout.program_title_close_hit {
        if my == y && mx >= xs && mx < xe {
            if app.session_title_menu.is_some() {
                return;
            }
            render_button_tooltip(f, &app.theme, " Session actions ", mx, my);
            return;
        }
    }
    let dirty = popup.buffer != popup.saved_markdown;
    let left = program_title_left_layout(
        summary,
        short_id(&popup.program.session_id),
        rect,
        dirty,
        true,
    );
    if let Some((start, end)) = left.modified {
        if mx >= start && mx < end {
            render_button_tooltip(f, &app.theme, " C-x C-s save ", mx, my);
        }
    }
}

fn render_program_selection_context_menu(
    f: &mut Frame,
    app: &mut App,
    popup: &crate::app::ProgramPopup,
    scroll_offset: usize,
    program_area: Rect,
) {
    if program_selection_range(popup).is_none() {
        app.layout.program_selection_run_hit = None;
        return;
    }
    let Some(pos) =
        program_cursor_position(Some(app), &popup.buffer, popup.cursor, scroll_offset, program_area)
    else {
        app.layout.program_selection_run_hit = None;
        return;
    };
    let rect = program_selection_context_menu_rect(pos, program_area);
    let hit = (
        rect.x.saturating_add(1),
        rect.x.saturating_add(rect.width.saturating_sub(1)),
        rect.y.saturating_add(1),
    );
    app.layout.program_selection_run_hit = Some(hit);
    let hovered = app
        .mouse_pos
        .is_some_and(|(mx, my)| my == hit.2 && mx >= hit.0 && mx < hit.1);
    let style = if hovered {
        Style::default()
            .fg(app.theme.text)
            .bg(app.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(app.theme.accent)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.border));
    let para = Paragraph::new(Line::from(Span::styled("▶ Run", style)));
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);
    f.render_widget(
        para,
        Rect {
            x: hit.0,
            y: hit.2,
            width: hit.1.saturating_sub(hit.0),
            height: 1,
        },
    );
}

fn program_selection_context_menu_rect(pos: Position, total: Rect) -> Rect {
    let max_x = total
        .x
        .saturating_add(total.width)
        .saturating_sub(PROGRAM_SELECTION_RUN_MENU_W);
    let max_y = total
        .y
        .saturating_add(total.height)
        .saturating_sub(PROGRAM_SELECTION_RUN_MENU_H);
    Rect {
        x: pos.x.saturating_add(1).min(max_x),
        y: pos.y.saturating_add(1).min(max_y),
        width: PROGRAM_SELECTION_RUN_MENU_W.min(total.width),
        height: PROGRAM_SELECTION_RUN_MENU_H.min(total.height),
    }
}


fn render_program_smart_clip_picker(
    f: &mut Frame,
    app: &App,
    popup: &crate::app::ProgramPopup,
    cursor_pos: Position,
    program_area: Rect,
) {
    let Some(search) = popup.smart_clip.as_ref() else {
        return;
    };
    if program_area.width == 0 || program_area.height < 3 {
        return;
    }
    let rows = app.program_smart_clip_rows(popup);

    // Raw indices of the selectable rows, and the raw index currently highlighted.
    let selectable_raw: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .map(|(i, _)| i)
        .collect();
    let sel_raw = if selectable_raw.is_empty() {
        None
    } else {
        Some(selectable_raw[search.selected.min(selectable_raw.len() - 1)])
    };

    let total = rows.len().max(1);
    let max_rows = program_area.height.saturating_sub(2).min(14);
    let row_count = (total as u16).min(max_rows).max(1);

    // Scroll the visible window so the highlighted row stays on screen.
    let mut offset = 0usize;
    if let Some(sr) = sel_raw {
        if sr >= row_count as usize {
            offset = sr + 1 - row_count as usize;
        }
        offset = offset.min(total.saturating_sub(row_count as usize));
    }

    let title = match search.view {
        crate::app::ProgramSmartClipView::Root => " smart clip ".to_string(),
        crate::app::ProgramSmartClipView::Submenu(group) => {
            format!(" smart clip › {} ", group.label())
        }
    };

    let width = 46u16.min(program_area.width.max(1));
    let x = cursor_pos
        .x
        .min(program_area.x.saturating_add(program_area.width.saturating_sub(width)));
    let below_y = cursor_pos.y.saturating_add(1);
    let above_y = cursor_pos.y.saturating_sub(row_count.saturating_add(2));
    let y = if below_y.saturating_add(row_count).saturating_add(2)
        <= program_area.y.saturating_add(program_area.height)
    {
        below_y
    } else {
        above_y.max(program_area.y)
    };
    let rect = Rect {
        x,
        y,
        width,
        height: row_count.saturating_add(2),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent_alt))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(app.theme.accent_alt)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(rect);
    f.render_widget(Clear, rect);
    f.render_widget(block, rect);

    let inner_w = inner.width as usize;
    let mut lines = Vec::new();
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "No matches",
            Style::default().fg(app.theme.dim),
        )));
    } else {
        for (raw_idx, row) in rows.iter().enumerate().skip(offset).take(row_count as usize) {
            let selected = sel_raw == Some(raw_idx);
            lines.push(render_program_smart_clip_row(app, row, selected, inner_w));
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// One line of the smart-clip picker: a relevance/submenu clip, a divider, an
/// expandable category header, or a project/group header inside the session
/// submenu.
fn render_program_smart_clip_row(
    app: &App,
    row: &crate::app::ProgramSmartClipRow,
    selected: bool,
    width: usize,
) -> Line<'static> {
    use crate::app::ProgramSmartClipRow;
    match row {
        ProgramSmartClipRow::Separator => Line::from(Span::styled(
            "─".repeat(width.max(1)),
            Style::default().fg(app.theme.dim),
        )),
        ProgramSmartClipRow::Header(label) => Line::from(Span::styled(
            label.clone(),
            Style::default()
                .fg(app.theme.muted)
                .add_modifier(Modifier::BOLD),
        )),
        ProgramSmartClipRow::Category { group, count } => {
            let base = if selected {
                Style::default()
                    .fg(app.theme.highlight_fg)
                    .bg(app.theme.highlight_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(app.theme.text)
                    .add_modifier(Modifier::BOLD)
            };
            let count_style = if selected {
                base
            } else {
                Style::default().fg(app.theme.muted)
            };
            let left = format!("{} {}", if selected { ">" } else { " " }, group.label());
            let count_str = format!("({count})");
            // Right-align the "(n) ▸" affordance; the chevron marks a submenu.
            let right_len = count_str.chars().count() + 2; // " " + "▸"
            let pad = width.saturating_sub(left.chars().count() + right_len);
            Line::from(vec![
                Span::styled(left, base),
                Span::styled(" ".repeat(pad), base),
                Span::styled(format!("{count_str} "), count_style),
                Span::styled("▸".to_string(), base),
            ])
        }
        ProgramSmartClipRow::Clip { candidate, dimmed } => {
            let (label_style, detail_style) = if selected {
                let s = Style::default()
                    .fg(app.theme.highlight_fg)
                    .bg(app.theme.highlight_bg)
                    .add_modifier(Modifier::BOLD);
                (s, s)
            } else if *dimmed {
                let s = Style::default().fg(app.theme.dim);
                (s, s)
            } else {
                (
                    Style::default().fg(app.theme.text),
                    Style::default().fg(app.theme.muted),
                )
            };
            let mut spans = vec![
                Span::styled(
                    format!("{} ", if selected { ">" } else { " " }),
                    label_style,
                ),
                Span::styled(candidate.label.clone(), label_style),
            ];
            if !candidate.detail.is_empty() {
                spans.push(Span::styled("  ", label_style));
                spans.push(Span::styled(candidate.detail.clone(), detail_style));
            }
            Line::from(spans)
        }
    }
}

/// Absolute wrapped position of the cursor within the program body:
/// `(visual_row, column_within_row)`, both in the `Wrap { trim: false }`
/// word-wrap coordinate space the body is laid out with (see
/// [`program_wrap_row_starts`] / [`program_wrap_locate`]). `width` is the inner
/// content width in cells; a zero width collapses the whole buffer onto row 0,
/// column 0.
pub(crate) fn program_cursor_visual_pos(
    app: Option<&App>,
    markdown: &str,
    cursor: usize,
    width: usize,
) -> (usize, usize) {
    if width == 0 {
        return (0, 0);
    }
    let (line, col) = program_line_col(markdown, cursor);

    // The program body is rendered with `Wrap { trim: false }`, which WORD-wraps
    // at whitespace (and hard-breaks words longer than the width) rather than
    // slicing every `width` characters. A logical line containing spaces breaks
    // earlier than naive char-division predicts, so the cursor must reuse the
    // same word-wrap — both to count the rows consumed by every line BEFORE the
    // cursor's line and to place the cursor within its own line. Anything else
    // drifts the moment a line wraps mid-word and compounds for lines below.
    let mut visual_row = 0usize;
    for raw in markdown.lines().take(line) {
        let text = program_rendered_line_text(app, raw);
        visual_row = visual_row.saturating_add(program_wrap_row_starts(&text, width).len());
    }

    let cur_raw = markdown.lines().nth(line).unwrap_or("");
    let visual_col = program_visual_col_for_line(app, cur_raw, col);
    let starts = program_wrap_row_starts(&program_rendered_line_text(app, cur_raw), width);
    let (row_in_line, col_in_row) = program_wrap_locate(&starts, visual_col, width);
    let visual_row = visual_row.saturating_add(row_in_line);
    (visual_row, col_in_row)
}

/// Wrapped visual row of the cursor (see [`program_cursor_visual_pos`]). Drives
/// the cursor-follow scroll so the caret stays inside the visible window.
pub(crate) fn program_cursor_visual_row(
    app: Option<&App>,
    markdown: &str,
    cursor: usize,
    width: usize,
) -> usize {
    program_cursor_visual_pos(app, markdown, cursor, width).0
}

/// Total number of wrapped visual rows the whole buffer occupies at `width`,
/// including the trailing empty row the cursor can sit on when the buffer ends
/// in a newline (or is empty). Bounds the scroll offset and drives the scroll
/// indicator. Defined as "the cursor's row at the very end of the buffer, plus
/// one" so the last reachable caret row is always `< total`.
pub(crate) fn program_total_visual_rows(app: Option<&App>, markdown: &str, width: usize) -> usize {
    if width == 0 {
        return markdown.matches('\n').count() + 1;
    }
    program_cursor_visual_pos(app, markdown, markdown.chars().count(), width)
        .0
        .saturating_add(1)
}

/// New vertical scroll offset (in wrapped rows) that keeps `cursor_row` inside a
/// `viewport_height`-row window. Scrolls up so the cursor is the top row when it
/// sits above the window, and down so it is the bottom row when it sits below;
/// otherwise the offset is left unchanged.
pub(crate) fn program_follow_scroll(
    scroll_offset: usize,
    cursor_row: usize,
    viewport_height: usize,
) -> usize {
    if viewport_height == 0 {
        return scroll_offset;
    }
    if cursor_row < scroll_offset {
        cursor_row
    } else if cursor_row >= scroll_offset + viewport_height {
        cursor_row - viewport_height + 1
    } else {
        scroll_offset
    }
}

/// Inverse of [`program_cursor_visual_pos`]: the buffer char offset whose cursor
/// paints at absolute visual `(target_row, target_col)` in the word-wrapped
/// body. Used by vertical navigation (land on a visual row while keeping a
/// preferred column) and mouse hit-testing (place the cursor where a click fell,
/// including on a wrapped continuation row). `target_row` is absolute in the
/// wrapped-row space — the caller folds in any scroll offset before calling.
///
/// A `target_row` past the end of the content resolves to the buffer's end; a
/// `target_col` left of or past a row's content clamps to that row's first or
/// last offset. Forward visual position is monotonic in char offset, so the
/// landing offset is the last column on the target row at or before the target.
pub(crate) fn program_visual_to_cursor(
    app: Option<&App>,
    markdown: &str,
    target_row: usize,
    target_col: usize,
    width: usize,
) -> usize {
    let width = width.max(1);

    // Walk logical lines (split on '\n' so a trailing empty line is kept the
    // same way the cursor's line math counts it), accumulating each line's
    // wrapped-row count until the line that owns `target_row` is found.
    let mut rows_before = 0usize;
    let mut line_start = 0usize; // char offset of the current line's first char
    let mut owner: Option<(usize, Vec<usize>, &str, usize)> = None;
    for raw in markdown.split('\n') {
        let rendered = program_rendered_line_text(app, raw);
        let starts = program_wrap_row_starts(&rendered, width);
        let row_count = starts.len();
        if target_row < rows_before + row_count {
            owner = Some((line_start, starts, raw, rows_before));
            break;
        }
        rows_before += row_count;
        line_start += raw.chars().count() + 1; // + the '\n'
    }
    let Some((line_start, starts, raw, rows_before)) = owner else {
        // Below all content → end of buffer.
        return markdown.chars().count();
    };
    let row_in_line = target_row - rows_before; // < starts.len() by construction

    // Largest raw column on this line whose forward visual position is at or
    // before (row_in_line, target_col) in row-major order.
    let line_len = raw.chars().count();
    let mut best_col = 0usize;
    for raw_col in 0..=line_len {
        let visual_col = program_visual_col_for_line(app, raw, raw_col);
        let (r, c) = program_wrap_locate(&starts, visual_col, width);
        if r < row_in_line || (r == row_in_line && c <= target_col) {
            best_col = raw_col;
        } else {
            break;
        }
    }
    line_start + best_col
}

/// The inner content width available to the program body, derived from the
/// popup's outer modal rect: the bordered block removes one cell per side and
/// the content margin removes [`PROGRAM_CONTENT_PADDING_X`] more per side. Mouse
/// hit-testing reuses this so it word-wraps on the exact width
/// [`render_program_popup_at`] paints.
pub(crate) fn program_modal_inner_width(modal: Rect) -> usize {
    (modal.width as usize)
        .saturating_sub(2)
        .saturating_sub(2 * PROGRAM_CONTENT_PADDING_X as usize)
}

fn program_cursor_position(
    app: Option<&App>,
    markdown: &str,
    cursor: usize,
    scroll_offset: usize,
    area: Rect,
) -> Option<Position> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    let width = area.width as usize;
    let (visual_row, x) = program_cursor_visual_pos(app, markdown, cursor, width);
    // Translate the absolute wrapped row into a row within the scrolled window;
    // a cursor scrolled above the top or below the bottom has no on-screen cell.
    let visual_row = visual_row.checked_sub(scroll_offset)?;
    if visual_row >= area.height as usize {
        return None;
    }
    Some(Position {
        x: area.x.saturating_add(x as u16),
        y: area.y.saturating_add(visual_row as u16),
    })
}

/// Locate a display column `visual_col` within a word-wrapped line: return the
/// `(row, col)` of the wrapped row that holds it, given the per-row starting
/// display offsets from [`program_wrap_row_starts`]. The row is the last one
/// whose start is at or before `visual_col`; the column is the remainder. A
/// cursor parked exactly at the right edge of a full row (or inside a run of
/// collapsed break-whitespace) is rolled onto the next row so it never paints
/// past the editor edge.
fn program_wrap_locate(starts: &[usize], visual_col: usize, width: usize) -> (usize, usize) {
    let width = width.max(1);
    let mut row = 0usize;
    for (idx, &start) in starts.iter().enumerate() {
        if start <= visual_col {
            row = idx;
        } else {
            break;
        }
    }
    let start = starts.get(row).copied().unwrap_or(0);
    let col = visual_col.saturating_sub(start);
    (row.saturating_add(col / width), col % width)
}

/// Word-wrap `text` exactly as ratatui's `Wrap { trim: false }` does and return,
/// for each resulting visual row, the display-column offset (within the
/// unwrapped line, counting collapsed break-whitespace) at which that row's
/// first painted cell begins. The number of entries is the visual row count.
///
/// This is a faithful port of ratatui's `WordWrapper` for the `trim == false`
/// path: a finished word (or a word that on its own overflows the width) is
/// flushed onto the pending row together with the whitespace that preceded it;
/// once the row is full the whitespace sitting at the break is dropped so the
/// next word starts the following row. Reusing the renderer's wrap rule keeps
/// the cursor's row count and intra-line column on the same glyphs the body
/// paints. Verified against ratatui's `TestBackend` output for word breaks,
/// hard breaks, trailing/leading whitespace, and collapsed multi-space runs.
fn program_wrap_row_starts(text: &str, width: usize) -> Vec<usize> {
    let max = width.max(1);
    // Each buffered glyph carries `(origin, glyph_width)` where `origin` is its
    // display offset in the unwrapped line, so a finished row reports where it
    // started even after break-whitespace between words is dropped.
    let mut rows: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut pending_line: Vec<(usize, usize)> = Vec::new();
    let mut line_width = 0usize;
    let mut pending_word: Vec<(usize, usize)> = Vec::new();
    let mut word_width = 0usize;
    let mut pending_ws: std::collections::VecDeque<(usize, usize)> =
        std::collections::VecDeque::new();
    let mut ws_width = 0usize;
    let mut non_ws_previous = false;
    let mut origin = 0usize;

    for ch in text.chars() {
        let sw = UnicodeWidthChar::width(ch).unwrap_or(0);
        let here = origin;
        origin = origin.saturating_add(sw);
        // ratatui ignores glyphs wider than the whole line.
        if sw > max {
            continue;
        }
        let is_ws = ch.is_whitespace();

        let word_found = non_ws_previous && is_ws;
        let untrimmed_overflow = pending_line.is_empty() && word_width + ws_width + sw > max;

        // A segment finished (word boundary) or the buffered word can no longer
        // share a row: commit the pending whitespace + word onto the row.
        if word_found || untrimmed_overflow {
            pending_line.extend(pending_ws.drain(..));
            line_width += ws_width;
            ws_width = 0;
            pending_line.append(&mut pending_word);
            line_width += word_width;
            word_width = 0;
        }

        let line_full = line_width >= max;
        let pending_word_overflow = sw > 0 && line_width + ws_width + word_width >= max;
        if line_full || pending_word_overflow {
            let mut remaining = max.saturating_sub(line_width);
            rows.push(std::mem::take(&mut pending_line));
            line_width = 0;
            // Drop whitespace that ran up to the row's edge — it does not carry
            // over as leading space on the next row.
            while let Some(&(_, w)) = pending_ws.front() {
                if w > remaining {
                    break;
                }
                ws_width -= w;
                remaining -= w;
                pending_ws.pop_front();
            }
            // The break whitespace itself is consumed, not re-counted.
            if is_ws && pending_ws.is_empty() {
                continue;
            }
        }

        if is_ws {
            ws_width += sw;
            pending_ws.push_back((here, sw));
        } else {
            word_width += sw;
            pending_word.push((here, sw));
        }
        non_ws_previous = !is_ws;
    }

    // Flush whatever is left into a final row (trim == false keeps trailing ws).
    pending_line.extend(pending_ws.drain(..));
    pending_line.append(&mut pending_word);
    if !pending_line.is_empty() {
        rows.push(pending_line);
    }
    if rows.is_empty() {
        rows.push(Vec::new());
    }

    // Convert each row to its starting display offset; an empty row (no painted
    // glyph) inherits the previous row's end so offsets stay monotonic.
    let mut starts = Vec::with_capacity(rows.len());
    let mut carry = 0usize;
    for row in &rows {
        let start = row.first().map(|&(o, _)| o).unwrap_or(carry);
        starts.push(start);
        if let Some(&(o, w)) = row.last() {
            carry = o.saturating_add(w);
        }
    }
    starts
}

/// For a markdown list-item line, return `(leading_indent_chars, content)` where
/// `content` is the text after the `- `/`* ` marker WITH its trailing whitespace
/// preserved, or `None` when the line isn't a bullet. Detection matches the
/// renderer (a fully-trimmed `* `/`- ` prefix, so a lone "* " with no content
/// yet stays literal), but the returned content is sliced from the raw line so a
/// trailing space the user just typed survives — `raw.trim()` would drop it,
/// stranding the cursor at the end of the line because the rendered glyphs and
/// the cursor column would then disagree on the line's width.
fn program_list_item_content(raw: &str) -> Option<(usize, &str)> {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))?;
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    let after_indent = raw
        .char_indices()
        .nth(leading)
        .map(|(idx, _)| &raw[idx..])
        .unwrap_or("");
    let rest = after_indent
        .strip_prefix("- ")
        .or_else(|| after_indent.strip_prefix("* "))
        .unwrap_or(after_indent);
    Some((leading, rest))
}

/// For a markdown heading line, return `(leading_indent_chars, content)` where
/// `content` is the heading text — the `#` markers are painted literally, so
/// they stay in the slice — taken from the raw line WITH its trailing whitespace
/// preserved, or `None` when the line isn't a heading. Detection matches the
/// renderer (`program_heading_level` on the fully-trimmed line), but the content
/// is sliced from the raw line so a trailing space the user just typed survives —
/// `raw.trim()` would drop it, stranding the cursor at the end of the line
/// because the rendered glyphs and the cursor column would then disagree on the
/// line's width. Headings don't render their leading indent, so only the indent
/// is stripped from the front, mirroring the trimmed text the renderer used.
fn program_heading_content(raw: &str) -> Option<(usize, &str)> {
    let trimmed = raw.trim();
    program_heading_level(trimmed)?;
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    let content = raw
        .char_indices()
        .nth(leading)
        .map(|(idx, _)| &raw[idx..])
        .unwrap_or("");
    Some((leading, content))
}

/// The plain text the program body paints for one logical markdown line, before
/// ratatui word-wraps it. Mirrors the per-line transformation in
/// [`render_program_markdown_lines`] / [`program_visual_col_for_line`] — kept
/// heading markers, the `  • ` list prefix, and expanded smart-clip chips — so
/// the cursor's wrap math sees exactly the glyphs (and their spaces) ratatui
/// wraps.
fn program_rendered_line_text(app: Option<&App>, raw: &str) -> String {
    let trimmed = raw.trim();
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    if trimmed.is_empty() {
        String::new()
    } else if let Some((_, content)) = program_heading_content(raw) {
        program_inline_rendered_text(app, content)
    } else if let Some((_, rest)) = program_list_item_content(raw) {
        format!(
            "{}  • {}",
            " ".repeat(leading),
            program_inline_rendered_text(app, rest)
        )
    } else {
        // Normal line: the renderer keeps the raw leading whitespace and
        // expands any inline chips in the remainder.
        let body = raw
            .char_indices()
            .nth(leading)
            .map(|(idx, _)| &raw[idx..])
            .unwrap_or("");
        let mut out: String = raw.chars().take(leading).collect();
        out.push_str(&program_inline_rendered_text(app, body));
        out
    }
}

/// Expand inline smart-clip chips (`@{…}`) in `text` to the ` label ` form the
/// renderer paints, leaving the surrounding text untouched. The label comes
/// from the same source as [`program_smart_clip_visual_width`], so the rendered
/// text and the cursor column stay width-consistent.
fn program_inline_rendered_text(app: Option<&App>, text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("@{") {
        out.push_str(&rest[..start]);
        let after_marker = &rest[start + 2..];
        let Some(end) = after_marker.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let raw_clip = &after_marker[..end];
        let (_, label) = program_smart_clip_label(app, raw_clip);
        out.push(' ');
        out.push_str(&label);
        out.push(' ');
        rest = &after_marker[end + 1..];
    }
    out.push_str(rest);
    out
}

/// One smart-clip located within a rendered program line: its display-column
/// `visual_start` (counting collapsed break-whitespace, before word-wrap), its
/// `visual_width`, and the raw clip body so the kind/id can be resolved.
struct LineClip {
    visual_start: usize,
    visual_width: usize,
    raw_clip: String,
}

/// Like [`program_inline_rendered_text`] but also reports each smart-clip's
/// display-column span within the produced text. `base` is the visual column at
/// which `text` begins on the rendered line (e.g. a bullet's `  • ` prefix), so
/// the returned spans are in the same coordinate space as
/// [`program_wrap_row_starts`]. The produced string is byte-for-byte what
/// `program_inline_rendered_text` returns.
fn program_inline_with_clips(app: Option<&App>, text: &str, base: usize) -> (String, Vec<LineClip>) {
    let mut out = String::new();
    let mut clips = Vec::new();
    let mut visual = base;
    let mut rest = text;
    while let Some(start) = rest.find("@{") {
        let before = &rest[..start];
        out.push_str(before);
        visual += UnicodeWidthStr::width(before);
        let after_marker = &rest[start + 2..];
        let Some(end) = after_marker.find('}') else {
            out.push_str(&rest[start..]);
            return (out, clips);
        };
        let raw_clip = &after_marker[..end];
        let width = program_smart_clip_visual_width(app, raw_clip);
        let (_, label) = program_smart_clip_label(app, raw_clip);
        clips.push(LineClip {
            visual_start: visual,
            visual_width: width,
            raw_clip: raw_clip.to_string(),
        });
        out.push(' ');
        out.push_str(&label);
        out.push(' ');
        visual += width;
        rest = &after_marker[end + 1..];
    }
    out.push_str(rest);
    (out, clips)
}

/// The plain text the program body paints for one logical line (identical to
/// [`program_rendered_line_text`]) paired with the display-column spans of every
/// smart-clip in it. Computing both from one pass keeps the clip offsets and the
/// wrapped text perfectly consistent for hit-testing.
fn program_rendered_line_with_clips(app: Option<&App>, raw: &str) -> (String, Vec<LineClip>) {
    let trimmed = raw.trim();
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    if trimmed.is_empty() {
        (String::new(), Vec::new())
    } else if let Some((_, content)) = program_heading_content(raw) {
        program_inline_with_clips(app, content, 0)
    } else if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        let (body, clips) = program_inline_with_clips(app, rest, leading + 4);
        (format!("{}  • {body}", " ".repeat(leading)), clips)
    } else {
        let body = raw
            .char_indices()
            .nth(leading)
            .map(|(idx, _)| &raw[idx..])
            .unwrap_or("");
        let lead: String = raw.chars().take(leading).collect();
        let (body_text, clips) = program_inline_with_clips(app, body, leading);
        (format!("{lead}{body_text}"), clips)
    }
}

/// On-screen cell ranges of every session smart-clip in `markdown`, laid out in
/// `area` with the same word-wrap as [`program_cursor_position`] (ratatui's
/// `Wrap { trim: false }`). Each session clip maps to one or more
/// [`ProgramClipHit`]s (one per wrapped-row segment) so the mouse handler can
/// resolve a cell → session id for hover-preview and click-to-focus. Clips of
/// other kinds (harness, response) are skipped.
pub(crate) fn program_session_clip_hits(
    app: Option<&App>,
    markdown: &str,
    scroll_offset: usize,
    area: Rect,
) -> Vec<crate::app::ProgramClipHit> {
    let mut hits = Vec::new();
    if area.width == 0 || area.height == 0 {
        return hits;
    }
    let width = area.width as usize;
    // The body paints wrapped rows `[scroll_offset, viewport_end)`. Rows above
    // the fold are counted (to advance the row base) but produce no hits.
    let viewport_end = scroll_offset.saturating_add(area.height as usize);
    let mut visual_row_base = 0usize;
    for raw in markdown.lines() {
        if visual_row_base >= viewport_end {
            break;
        }
        let (rendered, clips) = program_rendered_line_with_clips(app, raw);
        let starts = program_wrap_row_starts(&rendered, width);
        for clip in &clips {
            let (kind, id) = program_smart_clip_target(&clip.raw_clip);
            if kind != "session" {
                continue;
            }
            // Walk the clip's display columns, mapping each to a wrapped row and
            // merging contiguous same-row cells into one hit.
            let mut segment: Option<(u16, u16, u16)> = None; // (row, start, end)
            for vcol in clip.visual_start..clip.visual_start.saturating_add(clip.visual_width) {
                let (row_in_line, col_in_row) = program_wrap_locate(&starts, vcol, width);
                let abs_row = visual_row_base.saturating_add(row_in_line);
                if abs_row < scroll_offset {
                    continue; // above the fold (rows grow with the column)
                }
                if abs_row >= viewport_end {
                    break; // below the fold; later cells only sit lower
                }
                let screen_row = area.y.saturating_add((abs_row - scroll_offset) as u16);
                let screen_col = area.x.saturating_add(col_in_row as u16);
                match segment.as_mut() {
                    // A collapsed break-whitespace cell can map back onto a column
                    // already inside the current segment — leave it covered.
                    Some((r, s, e)) if *r == screen_row && screen_col >= *s && screen_col < *e => {}
                    Some((r, _s, e)) if *r == screen_row && *e == screen_col => {
                        *e = screen_col.saturating_add(1);
                    }
                    _ => {
                        if let Some((row, col_start, col_end)) = segment.take() {
                            hits.push(crate::app::ProgramClipHit {
                                col_start,
                                col_end,
                                row,
                                session_id: id.to_string(),
                            });
                        }
                        segment = Some((screen_row, screen_col, screen_col.saturating_add(1)));
                    }
                }
            }
            if let Some((row, col_start, col_end)) = segment.take() {
                hits.push(crate::app::ProgramClipHit {
                    col_start,
                    col_end,
                    row,
                    session_id: id.to_string(),
                });
            }
        }
        visual_row_base = visual_row_base.saturating_add(starts.len());
    }
    hits
}

fn program_line_col(markdown: &str, cursor: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col = 0usize;
    for (idx, ch) in markdown.chars().enumerate() {
        if idx >= cursor {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (line, col)
}

fn program_visual_col_for_line(app: Option<&App>, raw: &str, raw_col: usize) -> usize {
    let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
    let trimmed = raw.trim();
    let col = raw_col.saturating_sub(leading);
    if let Some((_, content)) = program_heading_content(raw) {
        // `content` keeps any trailing space (sliced from the raw line, leading
        // indent stripped) so the cursor column advances past a space typed at the
        // end of the heading — matching the glyphs the renderer paints.
        program_inline_visual_width(app, content, col)
    } else if let Some((_, rest)) = program_list_item_content(raw) {
        // Mirror the proportional indent rendered for nested bullets: the bullet
        // glyph and text sit `leading` columns further right than a top-level
        // item, so the cursor column must account for the same offset. The `- `/
        // `* ` marker is always two chars; the rendered `  • ` prefix is 4 wide.
        // `rest` keeps any trailing space so the column advances past it.
        leading + 4 + program_inline_visual_width(app, rest, col.saturating_sub(2))
    } else if raw_col <= leading {
        raw_col
    } else {
        let body = raw
            .char_indices()
            .nth(leading)
            .map(|(idx, _)| &raw[idx..])
            .unwrap_or("");
        leading + program_inline_visual_width(app, body, raw_col - leading)
    }
}

fn program_inline_visual_width(app: Option<&App>, text: &str, raw_col: usize) -> usize {
    // Display width of the first `n` chars of `s`, counting wide chars (emoji, CJK) as 2.
    fn chars_display_width(s: &str, n: usize) -> usize {
        s.chars()
            .take(n)
            .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
            .sum()
    }

    let mut visual = 0usize;
    let mut raw = 0usize;
    let mut rest = text;
    while let Some(start_b) = rest.find("@{") {
        let before = &rest[..start_b];
        let before_len = before.chars().count();
        if raw_col <= raw + before_len {
            return visual + chars_display_width(before, raw_col - raw);
        }
        visual += UnicodeWidthStr::width(before);
        raw += before_len;

        let after_marker = &rest[start_b + 2..];
        let Some(end_b) = after_marker.find('}') else {
            // Malformed @{ without closing }: treat remainder as plain text.
            return visual + chars_display_width(&rest[start_b..], raw_col.saturating_sub(raw));
        };
        let raw_clip = &after_marker[..end_b];
        let clip_len = 2 + raw_clip.chars().count() + 1;
        if raw_col <= raw + clip_len {
            return visual + program_smart_clip_visual_width(app, raw_clip);
        }
        visual += program_smart_clip_visual_width(app, raw_clip);
        raw += clip_len;
        rest = &after_marker[end_b + 1..];
    }
    visual + chars_display_width(rest, raw_col.saturating_sub(raw))
}

fn program_selection_range(popup: &crate::app::ProgramPopup) -> Option<(usize, usize)> {
    let selection = popup.selection.as_ref()?;
    let start = selection.anchor.min(selection.head);
    let end = selection.anchor.max(selection.head);
    (start != end).then_some((start, end))
}

fn render_program_markdown_lines<'a>(
    app: &App,
    markdown: &'a str,
    selection: Option<(usize, usize)>,
    search_matches: Option<&'a [(usize, usize)]>,
    search_selected: Option<usize>,
) -> Vec<Line<'a>> {
    let mut out = Vec::new();
    let mut line_start = 0usize;
    for raw in markdown.lines() {
        let trimmed = raw.trim();
        let leading = raw.chars().take_while(|ch| ch.is_whitespace()).count();
        if trimmed.is_empty() {
            out.push(Line::from(""));
        } else if let Some(level) = program_heading_level(trimmed) {
            // Slice the heading text from the raw line (leading indent stripped,
            // trailing whitespace kept) so a space typed at the end of the heading
            // paints a cell for the cursor to land on. `raw.trim()` would drop it,
            // desyncing the caret from the rendered glyphs.
            let content = program_heading_content(raw).map_or(trimmed, |(_, c)| c);
            out.push(render_program_heading_line(
                &app.theme,
                level,
                content,
                line_start + leading,
                selection,
                search_matches,
                search_selected,
            ));
        } else if let Some((_, rest)) = program_list_item_content(raw) {
            // Nesting is encoded as leading spaces on the source line; render it
            // as proportional indentation before the bullet so deeper items sit
            // visibly further right than their parents. `rest` keeps any trailing
            // whitespace so a space typed at the end of the bullet paints a cell
            // for the cursor to land on.
            let bullet = format!("{}  • ", " ".repeat(leading));
            let mut spans = vec![Span::styled(bullet, Style::default().fg(app.theme.accent))];
            spans.extend(render_program_inline_spans(
                app,
                rest,
                line_start + leading + 2,
                selection,
                search_matches,
                search_selected,
            ));
            out.push(Line::from(spans));
        } else if let Some(rest) = trimmed.strip_prefix(":::clip") {
            out.push(Line::from(vec![
                Span::raw("  "),
                program_chip_span(
                    format!("clip {}", rest.trim()).trim(),
                    app.theme.highlight_fg,
                    app.theme.info,
                ),
            ]));
        } else if trimmed == ":::" {
            out.push(Line::from(Span::styled(
                "  end clip",
                Style::default().fg(app.theme.dim),
            )));
        } else {
            out.push(Line::from(render_program_inline_spans(
                app,
                raw,
                line_start,
                selection,
                search_matches,
                search_selected,
            )));
        }
        line_start += raw.chars().count() + 1;
    }
    out
}

#[cfg(test)]
pub(crate) fn render_program_markdown_lines_for_test<'a>(
    app: &App,
    markdown: &'a str,
) -> Vec<Line<'a>> {
    render_program_markdown_lines(app, markdown, None, None, None)
}

fn program_heading_level(trimmed: &str) -> Option<u8> {
    if trimmed.starts_with("### ") {
        Some(3)
    } else if trimmed.starts_with("## ") {
        Some(2)
    } else if trimmed.starts_with("# ") {
        Some(1)
    } else {
        None
    }
}

fn render_program_heading_line<'a>(
    theme: &Theme,
    level: u8,
    text: &'a str,
    base: usize,
    selection: Option<(usize, usize)>,
    search_matches: Option<&'a [(usize, usize)]>,
    search_selected: Option<usize>,
) -> Line<'a> {
    let fg = match level {
        1 => theme.accent,
        2 => theme.accent_alt,
        _ => theme.info,
    };
    let style = Style::default().fg(fg).add_modifier(Modifier::BOLD);
    Line::from(program_text_spans(
        theme,
        text,
        base,
        style,
        selection,
        search_matches,
        search_selected,
    ))
}

fn render_program_inline_spans<'a>(
    app: &App,
    text: &'a str,
    base: usize,
    selection: Option<(usize, usize)>,
    search_matches: Option<&'a [(usize, usize)]>,
    search_selected: Option<usize>,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut rest = text;
    let mut offset = 0usize;
    while let Some(start) = rest.find("@{") {
        let (before, after_start) = rest.split_at(start);
        if !before.is_empty() {
            spans.extend(program_text_spans(
                &app.theme,
                before,
                base + offset,
                Style::default().fg(app.theme.text),
                selection,
                search_matches,
                search_selected,
            ));
        }
        let after_marker = &after_start[2..];
        let Some(end) = after_marker.find('}') else {
            spans.extend(program_text_spans(
                &app.theme,
                after_start,
                base + offset + before.chars().count(),
                Style::default().fg(app.theme.text),
                selection,
                search_matches,
                search_selected,
            ));
            return spans;
        };
        let raw_clip = &after_marker[..end];
        let before_chars = before.chars().count();
        let raw_clip_chars = raw_clip.chars().count();
        let clip_char_start = base + offset + before_chars;
        let clip_char_end = clip_char_start + 2 + raw_clip_chars + 1;
        let clip_match_idx = search_matches.and_then(|matches| {
            matches.iter().enumerate().find_map(|(i, &(ms, me))| {
                (ms < clip_char_end && me > clip_char_start).then_some(i)
            })
        });
        let clip_is_active_match = clip_match_idx
            .is_some_and(|idx| search_selected == Some(idx));
        spans.push(program_smart_clip_span(
            app,
            raw_clip,
            clip_match_idx.is_some(),
            clip_is_active_match,
        ));
        offset += before_chars + 2 + raw_clip_chars + 1;
        rest = &after_marker[end + 1..];
    }
    if !rest.is_empty() {
        spans.extend(program_text_spans(
            &app.theme,
        rest,
        base + offset,
        Style::default().fg(app.theme.text),
        selection,
        search_matches,
        search_selected,
    ));
    }
    spans
}

fn program_text_spans<'a>(
    theme: &Theme,
    text: &str,
    base: usize,
    style: Style,
    selection: Option<(usize, usize)>,
    search_matches: Option<&'a [(usize, usize)]>,
    search_selected: Option<usize>,
) -> Vec<Span<'a>> {
    let mut spans = Vec::new();
    let mut chunk = String::new();
    let mut chunk_selected: Option<bool> = None;
    let mut chunk_in_match: Option<bool> = None;
    let mut chunk_in_active_match: Option<bool> = None;
    for (idx, ch) in text.chars().enumerate() {
        let absolute_idx = base + idx;
        let match_idx = search_matches.and_then(|matches| program_search_match_index(matches, absolute_idx));
        let in_match = Some(match_idx.is_some());
        let in_active_match = Some(search_selected.is_some_and(|selected| Some(selected) == match_idx));
        let selected = selection.map(|(sel_start, sel_end)| absolute_idx >= sel_start && absolute_idx < sel_end);
        if chunk_selected.is_some_and(|current| Some(current) != selected)
            || chunk_in_match.is_some_and(|current| Some(current) != in_match)
            || chunk_in_active_match.is_some_and(|current| Some(current) != in_active_match)
        {
            if !chunk.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut chunk),
                    program_text_span_style(
                        theme,
                        style,
                        chunk_selected,
                        chunk_in_match,
                        chunk_in_active_match,
                    ),
                ));
            }
        }
        chunk_selected = selected;
        chunk_in_match = in_match;
        chunk_in_active_match = in_active_match;
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        spans.push(Span::styled(
            chunk,
            program_text_span_style(
                theme,
                style,
                chunk_selected,
                chunk_in_match,
                chunk_in_active_match,
            ),
        ));
    }
    spans
}

fn program_text_span_style(
    theme: &Theme,
    mut style: Style,
    selected: Option<bool>,
    in_match: Option<bool>,
    in_active_match: Option<bool>,
) -> Style {
    if in_active_match.unwrap_or(false) {
        style = style
            .fg(theme.highlight_fg)
            .bg(theme.highlight_bg)
            .add_modifier(Modifier::BOLD);
    } else if in_match.unwrap_or(false) {
        style = style.bg(theme.highlight_bg);
    }
    if selected.unwrap_or(false) {
        style = style.bg(theme.inactive_highlight_bg);
    }
    style
}

fn program_search_match_index(matches: &[(usize, usize)], idx: usize) -> Option<usize> {
    matches
        .iter()
        .enumerate()
        .find_map(|(i, &(start, end))| (idx >= start && idx < end).then_some(i))
}

fn program_smart_clip_span<'a>(app: &App, raw_clip: &str, in_match: bool, is_active_match: bool) -> Span<'a> {
    let (kind, label) = program_smart_clip_label(Some(app), raw_clip);
    let bg = if is_active_match {
        app.theme.highlight_bg
    } else if in_match {
        app.theme.highlight_bg
    } else {
        match kind {
            "session" => app.theme.accent_alt,
            "harness" => app.theme.harness,
            "session-response" => app.theme.info,
            _ => app.theme.inactive_highlight_bg,
        }
    };
    let mut style = Style::default().fg(app.theme.highlight_fg).bg(bg).add_modifier(Modifier::BOLD);
    if is_active_match {
        style = style.fg(app.theme.highlight_fg);
    }
    Span::styled(format!(" {} ", label), style)
}

fn program_smart_clip_visual_width(app: Option<&App>, raw_clip: &str) -> usize {
    let (_, label) = program_smart_clip_label(app, raw_clip);
    UnicodeWidthStr::width(label.as_str()) + 2
}

/// Parse a smart-clip body (`session:abc`, `harness:codex`, or
/// `session:abc clip_id=3`) into its `(kind, id)`. The kind selects the chip
/// styling and label; the id resolves the referenced session/harness.
fn program_smart_clip_target(raw_clip: &str) -> (&str, &str) {
    let first = raw_clip.split_whitespace().next().unwrap_or(raw_clip);
    first.split_once(':').unwrap_or(("clip", first))
}

/// The chip label for a session smart-clip: `<glyph> <name> · <harness>`.
/// Mirrors the session list's leading lifecycle glyph and name; shows the
/// harness (not the model) and drops the redundant "session" prefix and the
/// status word.
fn program_session_clip_label(s: &agentd_protocol::SessionSummary) -> String {
    format!("{} {} · {}", s.state.glyph(), primary_label(s), harness_label(s))
}

fn program_harness_clip_label(h: &agentd_protocol::HarnessInfo) -> String {
    let status_icon = if h.available { "✓" } else { "✗" };
    format!("{status_icon} {}", h.name)
}

pub(crate) fn program_smart_clip_label<'a>(app: Option<&App>, raw_clip: &'a str) -> (&'a str, String) {
    let (kind, id) = program_smart_clip_target(raw_clip);
    let label = match kind {
        "session" => app
            .and_then(|app| {
                app.sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(program_session_clip_label)
            })
            .unwrap_or_else(|| format!("session {id}")),
        "harness" => app
            .and_then(|app| {
                app.harnesses.iter().find(|h| h.name == id).map(program_harness_clip_label)
            })
            .unwrap_or_else(|| format!("harness {id}")),
        "session-response" => format!("response {id}"),
        _ => format!("{kind} {id}"),
    };
    (kind, label)
}

fn program_chip_span<'a>(
    label: impl AsRef<str>,
    fg: ratatui::style::Color,
    bg: ratatui::style::Color,
) -> Span<'a> {
    Span::styled(
        format!(" {} ", label.as_ref()),
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
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
    fn view_program_toggle_tooltip_stays_inside_session_view() {
        let view = Rect::new(30, 0, 90, 40);
        let total = Rect::new(0, 0, 120, 40);
        let (anchor_x, _, anchor_y) = view_program_toggle_button_range(view);
        let rect = view_program_toggle_tooltip_rect(view, total, anchor_x, anchor_y, 40, 3);

        assert!(
            rect.x >= view.x,
            "tooltip must not spill over the session list: {rect:?}"
        );
        assert!(
            rect.x.saturating_add(rect.width) <= view.x.saturating_add(view.width),
            "tooltip should fit within the session view when there is room: {rect:?}"
        );
    }

    #[test]
    fn session_hover_card_size_is_landscape() {
        // Short content: width is forced past height so the card
        // reads as a landscape tile rather than a portrait sliver.
        let (w, h) = session_hover_card_size(8, 6, 64);
        assert!(w > h, "expected landscape card, got {w}x{h}");
        // Wide content drives the width but stays bounded by the cap.
        let (w, h) = session_hover_card_size(200, 6, 64);
        assert_eq!(w, 64);
        assert!(w > h, "capped width should still exceed height: {w}x{h}");
        // Shorter card, still wider than tall.
        let (w, h) = session_hover_card_size(4, 3, 64);
        assert!(w > h, "expected landscape card without preview, got {w}x{h}");
    }

    #[test]
    fn session_hover_card_uses_narrower_taller_preview_geometry() {
        let (w, h) = session_hover_card_size(
            PROGRAM_CLIP_HOVER_PREVIEW_COLS,
            PROGRAM_CLIP_HOVER_PREVIEW_ROWS,
            PROGRAM_CLIP_HOVER_PREVIEW_COLS,
        );
        assert_eq!(w, 102, "outer width should be about 20% narrower than 128");
        assert_eq!(h, 14, "outer height should be about 40% taller than 10");
    }

    #[test]
    fn session_hover_card_rect_anchors_to_mouse_position() {
        let modal = Rect::new(10, 5, 120, 40);
        let rect = session_hover_card_rect(modal, 50, 14, 42, 12).expect("card fits");
        assert_eq!(rect.x, 42);
        assert_eq!(rect.y, 13);

        let rect = session_hover_card_rect(modal, 50, 14, 125, 42).expect("card fits");
        assert_eq!(rect.x, 80, "right edge should stay inside the modal");
        assert_eq!(rect.y, 28, "card should flip above the mouse near the bottom");
    }

    #[test]
    fn program_shimmer_line_sessions_maps_block_to_its_session() {
        // An item with a wrapped continuation line is one block; only it
        // shimmers. Every line of the shimmering block resolves to the session
        // its clip references; the settled item and the blank separator resolve
        // to nothing.
        let md = "- working @{session:s1}\n  detail line\n\n- idle @{session:s2}";
        let active = [true, true, false, false];
        let got = program_shimmer_line_sessions(None, md, &active);
        assert_eq!(
            got,
            vec![Some("s1".to_string()), Some("s1".to_string()), None, None]
        );
    }

    #[test]
    fn program_shimmer_line_sessions_ignores_blocks_without_a_session_clip() {
        // A shimmering block with no session clip yields no hover target.
        let md = "just running prose\nmore prose";
        let active = [true, true];
        assert_eq!(
            program_shimmer_line_sessions(None, md, &active),
            vec![None, None]
        );
    }

    #[test]
    fn program_shimmer_session_at_resolves_cursor_to_block_session() {
        let area = Rect::new(0, 0, 80, 6);
        let md = "- working @{session:s1}\n  detail line\n\n- idle @{session:s2}";
        let line_sessions = vec![Some("s1".to_string()), Some("s1".to_string()), None, None];
        // Both lines of the shimmering block resolve to s1.
        assert_eq!(
            program_shimmer_session_at(None, md, &line_sessions, 0, area, 5, 0),
            Some("s1".to_string())
        );
        assert_eq!(
            program_shimmer_session_at(None, md, &line_sessions, 0, area, 0, 1),
            Some("s1".to_string())
        );
        // The settled block and out-of-body cells resolve to nothing.
        assert_eq!(program_shimmer_session_at(None, md, &line_sessions, 0, area, 0, 3), None);
        assert_eq!(program_shimmer_session_at(None, md, &line_sessions, 0, area, 0, 99), None);
    }

    #[test]
    fn program_shimmer_session_at_tracks_scroll_offset() {
        // A target on the third logical row follows the viewport when scrolled.
        let area = Rect::new(0, 0, 80, 6);
        let md = "l0\nl1\n@{session:s2}";
        let line_sessions = vec![None, None, Some("s2".to_string())];
        assert_eq!(
            program_shimmer_session_at(None, md, &line_sessions, 0, area, 0, 2),
            Some("s2".to_string())
        );
        assert_eq!(
            program_shimmer_session_at(None, md, &line_sessions, 2, area, 0, 0),
            Some("s2".to_string())
        );
    }

    #[test]
    fn program_cursor_position_targets_current_character_cell() {
        let area = Rect::new(10, 2, 20, 4);
        assert_eq!(
            program_cursor_position(None, "abc", 1, 0, area),
            Some(Position { x: 11, y: 2 })
        );
    }

    #[test]
    fn program_cursor_position_accounts_for_wrapped_lines() {
        let area = Rect::new(10, 2, 5, 4);
        assert_eq!(
            program_cursor_position(None, "abcdef", 6, 0, area),
            Some(Position { x: 11, y: 3 })
        );
    }

    #[test]
    fn program_cursor_position_uses_rendered_smart_clip_width() {
        let area = Rect::new(10, 2, 80, 4);
        let markdown = "run @{harness:codex} now";
        let cursor = "run @{harness:codex}".chars().count();
        let chip_width = " harness codex ".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position {
                x: 10 + "run ".chars().count() as u16 + chip_width as u16,
                y: 2,
            })
        );
    }

    fn clip_test_session(
        id: &str,
        title: Option<&str>,
        harness: &str,
        state: SessionState,
    ) -> SessionSummary {
        SessionSummary {
            id: id.into(),
            harness: harness.into(),
            cwd: "/tmp".into(),
            title: title.map(|t| t.to_string()),
            state,
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

    #[test]
    fn program_session_clip_label_shows_glyph_name_and_harness() {
        let s = clip_test_session("abc123", Some("My Task"), "codex", SessionState::Running);
        // `<glyph> <name> · <harness>` — no "session" prefix, no model, no status word.
        assert_eq!(program_session_clip_label(&s), "● My Task · codex");
        let label = program_session_clip_label(&s);
        assert!(!label.contains("session"), "dropped the session prefix: {label}");
        assert!(!label.contains("running"), "dropped the status word: {label}");
    }

    #[test]
    fn program_session_clip_label_used_by_smart_clip_label() {
        // The chip label routes through the shared session-label helper when the
        // session resolves against the app.
        let s = clip_test_session("s9", Some("Build"), "claude", SessionState::Done);
        let (kind, label) = (
            program_smart_clip_target("session:s9").0,
            program_session_clip_label(&s),
        );
        assert_eq!(kind, "session");
        assert_eq!(label, "✓ Build · claude");
    }

    #[test]
    fn program_harness_clip_label_shows_status_icon_and_name() {
        let available = agentd_protocol::HarnessInfo {
            name: "codex".into(),
            available: true,
            binary: None,
            description: None,
            capabilities: Default::default(),
        };
        let missing = agentd_protocol::HarnessInfo {
            name: "claude".into(),
            available: false,
            binary: None,
            description: None,
            capabilities: Default::default(),
        };

        assert_eq!(program_harness_clip_label(&available), "✓ codex");
        assert_eq!(program_harness_clip_label(&missing), "✗ claude");
    }

    #[test]
    fn program_session_clip_hits_map_cells_to_session_ids() {
        // Two session clips with a harness clip between them: only the session
        // clips produce hits, each over the chip's painted cells (incl. padding).
        let area = Rect::new(0, 0, 80, 6);
        let md = "@{session:s1} mid @{harness:codex} @{session:s2}";
        let hits = program_session_clip_hits(None, md, 0, area);
        assert_eq!(
            hits,
            vec![
                crate::app::ProgramClipHit {
                    col_start: 0,
                    col_end: 12,
                    row: 0,
                    session_id: "s1".into(),
                },
                crate::app::ProgramClipHit {
                    col_start: 33,
                    col_end: 45,
                    row: 0,
                    session_id: "s2".into(),
                },
            ]
        );
        // A cell inside the first chip resolves to s1; the gap between chips does not.
        assert!(hits.iter().any(|h| h.contains(5, 0) && h.session_id == "s1"));
        assert!(!hits.iter().any(|h| h.contains(20, 0)));
    }

    #[test]
    fn program_session_clip_hits_use_terminal_display_width() {
        // Hit-testing must track terminal cells, not Unicode scalar counts. Wide
        // glyphs before the chip shift its painted start, and wide glyphs inside
        // the fallback session label expand its painted end.
        let area = Rect::new(0, 0, 80, 6);
        let md = "🚀 @{session:火}";
        let hits = program_session_clip_hits(None, md, 0, area);
        assert_eq!(
            hits,
            vec![crate::app::ProgramClipHit {
                col_start: UnicodeWidthStr::width("🚀 ") as u16,
                col_end: UnicodeWidthStr::width("🚀  session 火 ") as u16,
                row: 0,
                session_id: "火".into(),
            }]
        );
        let hit = &hits[0];
        assert!(
            hit.contains(hit.col_end - 1, 0),
            "rightmost painted wide-label cell must hover"
        );
        assert!(!hit.contains(hit.col_end, 0));
    }

    #[test]
    fn program_session_clip_hits_span_wrapped_rows() {
        // A chip wider than the body wraps; the clip still maps entirely to its
        // session across every row it occupies, with no foreign ids.
        let area = Rect::new(0, 0, 8, 6);
        let hits = program_session_clip_hits(None, "@{session:s1}", 0, area);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.session_id == "s1"));
        let rows: std::collections::BTreeSet<u16> = hits.iter().map(|h| h.row).collect();
        assert!(
            rows.len() >= 2,
            "a chip wider than the body should wrap across rows: {hits:?}"
        );
    }

    #[test]
    fn program_session_clip_hits_empty_without_clips() {
        let area = Rect::new(0, 0, 40, 4);
        assert!(program_session_clip_hits(None, "just prose, no clips", 0, area).is_empty());
    }

    fn placeholder_template(id: &str, name: &str) -> agentd_protocol::ProgramTemplate {
        agentd_protocol::ProgramTemplate {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            markdown: format!("# {name}\n"),
            built_in: true,
        }
    }

    #[test]
    fn program_empty_placeholder_offers_clickable_template_buttons() {
        let theme = crate::theme::Theme::default();
        let templates = vec![
            placeholder_template("blank", "Blank"),
            placeholder_template("tasks", "Tasks"),
            placeholder_template("investigation", "Investigation"),
        ];
        // Inner rect offset from origin to confirm hits use absolute coordinates.
        let inner = Rect::new(2, 1, 76, 20);
        let (lines, hits) = program_empty_placeholder(&theme, &templates, inner);

        // Two buttons — "blank" is the empty state itself, so it's filtered out.
        // Ordered by name (case-insensitive): Investigation before Tasks.
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].template_id, "investigation");
        assert_eq!(hits[1].template_id, "tasks");
        assert_eq!(hits[0].markdown, "# Investigation\n");

        // Buttons occupy the three rows after the description + blank line, in the
        // inner rect's coordinate space (rows 1+2 ..= 1+4).
        assert_eq!(hits[0].row_start, inner.y + 2);
        assert_eq!(hits[0].row_end, inner.y + 4);
        assert!(hits[0].col_start >= inner.x);
        // The button box label-row click lands inside the first button.
        assert!(hits[0].contains(hits[0].col_start, hits[0].row_start + 1));
        // The two buttons never overlap.
        assert!(hits[0].col_end <= hits[1].col_start);
        // No placeholder line exceeds the inner width, so nothing wraps and the
        // absolute hit rows stay correct.
        for line in &lines {
            assert!(line.width() <= inner.width as usize);
        }
    }

    #[test]
    fn program_empty_placeholder_orders_buttons_by_name() {
        let theme = crate::theme::Theme::default();
        // Deliberately out of order, mixed case, with "blank" mixed in.
        let templates = vec![
            placeholder_template("zeta", "zeta"),
            placeholder_template("blank", "Blank"),
            placeholder_template("alpha", "Alpha"),
            placeholder_template("mid", "mid"),
        ];
        let (_, hits) = program_empty_placeholder(&theme, &templates, Rect::new(0, 0, 80, 30));
        let ids: Vec<&str> = hits.iter().map(|h| h.template_id.as_str()).collect();
        // Case-insensitive name order; "blank" excluded.
        assert_eq!(ids, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn program_empty_placeholder_wraps_many_buttons_across_rows() {
        let theme = crate::theme::Theme::default();
        // Five short-named templates in a narrow-ish pane force wrapping.
        let templates = vec![
            placeholder_template("aaa", "Aaa"),
            placeholder_template("bbb", "Bbb"),
            placeholder_template("ccc", "Ccc"),
            placeholder_template("ddd", "Ddd"),
            placeholder_template("eee", "Eee"),
        ];
        let inner = Rect::new(2, 1, 20, 30);
        let (lines, hits) = program_empty_placeholder(&theme, &templates, inner);

        // All five buttons rendered and clickable.
        assert_eq!(hits.len(), 5);
        // They span more than one row.
        let rows: std::collections::BTreeSet<u16> = hits.iter().map(|h| h.row_start).collect();
        assert!(rows.len() >= 2, "expected buttons across multiple rows");
        // Every rendered button is hittable at its own label row, and no two
        // overlap within a row.
        for h in &hits {
            assert!(h.contains(h.col_start, h.row_start + 1));
        }
        // No line exceeds inner width, so absolute hit rows can't desync.
        for line in &lines {
            assert!(line.width() <= inner.width as usize);
        }
        // Button rows are 3 lines tall, stacked contiguously from row +2.
        for h in &hits {
            assert!((h.row_start - inner.y - 2) % 3 == 0);
            assert_eq!(h.row_end, h.row_start + 2);
        }
    }

    #[test]
    fn program_empty_placeholder_truncates_with_overflow_indicator() {
        let theme = crate::theme::Theme::default();
        let templates = vec![
            placeholder_template("aaa", "Aaa"),
            placeholder_template("bbb", "Bbb"),
            placeholder_template("ccc", "Ccc"),
            placeholder_template("ddd", "Ddd"),
            placeholder_template("eee", "Eee"),
            placeholder_template("fff", "Fff"),
        ];
        // width 20 packs ~2 buttons/row; height 10 leaves room for a single
        // button row plus an overflow indicator.
        let inner = Rect::new(0, 0, 20, 10);
        let (lines, hits) = program_empty_placeholder(&theme, &templates, inner);

        assert!(hits.len() < 6, "some buttons should be hidden");
        assert!(!hits.is_empty(), "at least one row should render");
        let hidden = 6 - hits.len();
        let overflow = format!("+{hidden} more");
        let rendered: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains(&overflow),
            "expected overflow indicator {overflow:?} in:\n{rendered}"
        );
        for line in &lines {
            assert!(line.width() <= inner.width as usize);
        }
    }

    #[test]
    fn program_empty_placeholder_falls_back_when_narrow() {
        let theme = crate::theme::Theme::default();
        let templates = vec![placeholder_template("tasks", "Tasks")];
        // Too narrow for an indented bordered button: plain description + syntax only.
        let (_, hits) = program_empty_placeholder(&theme, &templates, Rect::new(0, 0, 6, 20));
        assert!(hits.is_empty());
    }

    #[test]
    fn program_empty_placeholder_has_no_buttons_without_templates() {
        let theme = crate::theme::Theme::default();
        let (lines, hits) = program_empty_placeholder(&theme, &[], Rect::new(0, 0, 80, 20));
        assert!(hits.is_empty());
        // Still shows the description and syntax prose.
        assert!(!lines.is_empty());
    }

    #[test]
    fn program_session_clip_hits_track_scroll_offset() {
        // A clip on the third logical row (abs visual row 2) shifts up by the
        // scroll offset so its hitbox follows the visible viewport.
        let area = Rect::new(0, 0, 80, 6);
        let md = "l0\nl1\n@{session:s1}\nl3";
        let unscrolled = program_session_clip_hits(None, md, 0, area);
        assert_eq!(unscrolled.len(), 1);
        assert_eq!(unscrolled[0].row, 2);
        assert_eq!(unscrolled[0].session_id, "s1");

        let scrolled = program_session_clip_hits(None, md, 2, area);
        assert_eq!(
            scrolled,
            vec![crate::app::ProgramClipHit {
                col_start: 0,
                col_end: 12,
                row: 0,
                session_id: "s1".into(),
            }]
        );

        // Scrolled entirely past the clip: no hit remains.
        assert!(program_session_clip_hits(None, md, 3, area).is_empty());
    }

    #[test]
    fn program_cursor_position_accounts_for_preceding_wrapped_line() {
        // "abcdef" wraps to two visual rows at width 5, so the next logical
        // line ("XY") starts on the third row (y offset 2), not the second.
        let area = Rect::new(10, 2, 5, 6);
        let markdown = "abcdef\nXY";
        let cursor = "abcdef\n".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 10, y: 4 })
        );
    }

    #[test]
    fn program_cursor_position_combines_preceding_wrap_and_intra_line_offset() {
        // The preceding line wraps (2 rows) AND the cursor sits past a wrap
        // boundary within its own line: both offsets must accumulate.
        let area = Rect::new(10, 2, 5, 8);
        let markdown = "abcdef\nghijklmn";
        let cursor = "abcdef\nghijklm".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 12, y: 5 })
        );
    }

    #[test]
    fn program_cursor_position_offsets_normal_line_below_wrapped_line() {
        // "longlineAAAA" (12 cols) wraps to three rows at width 5, so the
        // following non-wrapping line ("short") starts on the fourth row.
        let area = Rect::new(10, 2, 5, 8);
        let markdown = "longlineAAAA\nshort";
        let cursor = "longlineAAAA\nsh".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 12, y: 5 })
        );
    }

    #[test]
    fn program_cursor_position_word_wraps_line_with_spaces() {
        // The program body renders with `Wrap { trim: false }`, which WORD-wraps
        // at spaces. "hello world foo" at width 8 lays out as three rows
        // ("hello" / "world" / "foo"), so a cursor before "foo" sits at the
        // start of the third row. Naive char-division (col / width) would put
        // it at row 1 col 4 — inside "world" — which is the residual bug.
        let area = Rect::new(10, 2, 8, 6);
        let markdown = "hello world foo";
        let cursor = "hello world ".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 10, y: 4 })
        );
    }

    #[test]
    fn program_cursor_position_word_wrapped_line_offsets_following_line() {
        // A word-wrapped line consumes the right number of visual rows, so a
        // normal line below it lands on the correct row. "hello world foo" at
        // width 8 is three rows; "next" starts on the fourth. Char-division
        // (ceil(15/8) = 2) would undercount and pull the line up a row.
        let area = Rect::new(10, 2, 8, 8);
        let markdown = "hello world foo\nnext";
        let cursor = "hello world foo\n".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 10, y: 5 })
        );
    }

    #[test]
    fn program_cursor_position_hard_break_then_space_no_phantom_row() {
        // "abcd efgh" at width 4: "abcd" exactly fills row 0, the space is the
        // break point (consumed), and "efgh" is row 1 — two rows, not three.
        // A cursor before 'e' sits at row 1 col 0. (A naive `wrap_to_width`
        // reuse would emit a spurious empty middle row here and also misplace
        // the column; ratatui collapses the break space instead.)
        let area = Rect::new(10, 2, 4, 8);
        let markdown = "abcd efgh";
        let cursor = "abcd ".chars().count();

        assert_eq!(
            program_cursor_position(None, markdown, cursor, 0, area),
            Some(Position { x: 10, y: 3 })
        );
    }

    #[test]
    fn program_cursor_position_matches_painted_glyph_on_wrapped_line() {
        // Cross-check the computed cursor cell against the glyph ratatui
        // actually paints, using the exact `Paragraph::wrap(Wrap{trim:false})`
        // the program body uses. The cursor before "foo" must land on the
        // painted 'f' at the start of the wrapped row — not somewhere in the
        // middle of "world" as char-division would compute.
        let w = 8u16;
        let h = 6u16;
        let area = Rect::new(0, 0, w, h);
        let markdown = "hello world foo";
        let cursor = "hello world ".chars().count();

        let pos = program_cursor_position(None, markdown, cursor, 0, area).expect("cursor pos");

        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut term = ratatui::Terminal::new(backend).expect("terminal");
        term.draw(|f| {
            // Plain markdown renders one Line == the raw text, so this matches
            // what `render_program_popup_at` feeds the Paragraph for this input.
            let para = Paragraph::new(markdown).wrap(Wrap { trim: false });
            f.render_widget(para, area);
        })
        .expect("draw");
        let buf = term.backend().buffer();
        let glyph = buf
            .cell((pos.x, pos.y))
            .map(|c| c.symbol().to_string())
            .unwrap_or_default();

        assert_eq!(
            glyph, "f",
            "computed cursor {pos:?} should sit on the painted 'f' starting the wrapped row"
        );
    }

    #[test]
    fn program_cursor_position_accounts_for_wide_emoji() {
        // ⏳ (U+23F3, HOURGLASS WITH FLOWING SAND) is a double-width character
        // (display width 2). The cursor placed just after it must sit at column 2,
        // not column 1, and the character after it must sit at column 3, not 2.
        let area = Rect::new(10, 2, 40, 4);
        let markdown = "⏳abc";

        // Cursor at char index 0 (before ⏳) → display col 0.
        assert_eq!(
            program_cursor_position(None, markdown, 0, 0, area),
            Some(Position { x: 10, y: 2 }),
            "cursor before ⏳ should be at col 0"
        );
        // Cursor at char index 1 (after ⏳) → display col 2 (emoji is 2 wide).
        assert_eq!(
            program_cursor_position(None, markdown, 1, 0, area),
            Some(Position { x: 12, y: 2 }),
            "cursor after ⏳ should be at col 2 (emoji is double-width)"
        );
        // Cursor at char index 2 (after ⏳ + 'a') → display col 3.
        assert_eq!(
            program_cursor_position(None, markdown, 2, 0, area),
            Some(Position { x: 13, y: 2 }),
            "cursor after ⏳a should be at col 3"
        );
    }

    #[test]
    fn program_visual_to_cursor_accounts_for_wide_emoji() {
        // Inverse: clicking at display column 2 on a line starting with ⏳ should
        // resolve to char offset 1 (just after the emoji), not char offset 2.
        let markdown = "⏳abc";
        // Display col 2 on row 0 (just after ⏳) → char offset 1.
        assert_eq!(
            program_visual_to_cursor(None, markdown, 0, 2, 40),
            1,
            "click at display col 2 should land at char offset 1 (after ⏳)"
        );
        // Display col 3 on row 0 (after 'a') → char offset 2.
        assert_eq!(
            program_visual_to_cursor(None, markdown, 0, 3, 40),
            2,
            "click at display col 3 should land at char offset 2 (after ⏳a)"
        );
    }

    #[test]
    fn program_follow_scroll_advances_when_cursor_below_window() {
        // Cursor on visual row 19 with a 5-row window anchored at offset 0 must
        // scroll down so the cursor becomes the bottom visible row (offset 15).
        assert_eq!(program_follow_scroll(0, 19, 5), 15);
    }

    #[test]
    fn program_follow_scroll_returns_to_top_when_cursor_above_window() {
        // Cursor back on row 0 while scrolled to 15 snaps the window to the top.
        assert_eq!(program_follow_scroll(15, 0, 5), 0);
    }

    #[test]
    fn program_follow_scroll_unchanged_when_cursor_already_visible() {
        assert_eq!(program_follow_scroll(0, 2, 5), 0);
        assert_eq!(program_follow_scroll(10, 12, 5), 10);
    }

    #[test]
    fn program_cursor_position_subtracts_scroll_offset() {
        // Ten single-row lines at width 20; the cursor sits on logical line 7.
        let area = Rect::new(10, 0, 20, 5);
        let markdown = (0..10).map(|i| format!("L{i}")).collect::<Vec<_>>().join("\n");
        let cursor = markdown.find("L7").unwrap();
        // Scrolled past the first 5 rows, row 7 renders two rows into the view.
        assert_eq!(
            program_cursor_position(None, &markdown, cursor, 5, area),
            Some(Position { x: 10, y: 2 })
        );
        // Without scrolling, that row is below the 5-row window: no cell to draw.
        assert_eq!(program_cursor_position(None, &markdown, cursor, 0, area), None);
    }

    #[test]
    fn program_total_visual_rows_counts_trailing_empty_line() {
        // "a\n" is two rows: the text row and the trailing empty row the cursor
        // can sit on. The count must include that final row so the scroll clamp
        // keeps it reachable.
        assert_eq!(program_total_visual_rows(None, "a\n", 20), 2);
        assert_eq!(program_total_visual_rows(None, "", 20), 1);
        // "abcdef" wraps to two rows at width 5.
        assert_eq!(program_total_visual_rows(None, "abcdef", 5), 2);
    }

    #[test]
    fn program_heading_rendering_keeps_markdown_marker() {
        let theme = Theme::default();
        assert_eq!(
            line_text(&render_program_heading_line(&theme, 1, "# Todo", 0, None, None, None)),
            "# Todo"
        );
        assert_eq!(
            line_text(&render_program_heading_line(
                &theme,
                2,
                "## Progress",
                0,
                None,
                None,
                None
            )),
            "## Progress"
        );
    }

    #[test]
    fn program_text_spans_highlights_search_matches() {
        let theme = Theme::default();
        let spans = program_text_spans(
            &theme,
            "alpha alpha",
            0,
            Style::default().fg(theme.text),
            None,
            Some(&[(0, 5), (6, 11)]),
            Some(1),
        );
        let mut inactive_highlight = false;
        let mut active_highlight = false;
        for span in spans {
            if span.content.as_ref() != "alpha" {
                continue;
            }
            if span.style.bg == Some(theme.highlight_bg) && span.style.fg == Some(theme.highlight_fg) {
                active_highlight = true;
            } else if span.style.bg == Some(theme.highlight_bg) {
                inactive_highlight = true;
            }
        }
        assert!(active_highlight, "selected match should be bold + highlighted");
        assert!(inactive_highlight, "non-active match should still be highlighted");
    }

    #[test]
    fn program_focus_styles_are_distinct_from_session_focus() {
        let theme = Theme::default();
        let active_program = program_border_style(&theme, true);
        let inactive_program = program_border_style(&theme, false);

        assert_eq!(pane_border_style(&theme, true).fg, Some(theme.border_focused));
        assert_eq!(active_program.fg, Some(theme.accent_alt));
        assert_eq!(inactive_program.fg, active_program.fg);
        assert_ne!(inactive_program.fg, Some(theme.border));
        assert!(active_program.add_modifier.contains(Modifier::BOLD));
        assert!(!inactive_program.add_modifier.contains(Modifier::BOLD));
        assert!(
            inactive_program.add_modifier.contains(Modifier::DIM),
            "inactive program border should dim without switching hue"
        );
        assert_ne!(active_program.fg, pane_border_style(&theme, true).fg);
    }

    #[test]
    fn session_menu_icon_dims_when_pane_unfocused() {
        // The session-actions menu glyph (` ☰ `) at the right of the pane title
        // bar is shared by both the chat/PTY session view (`render_detail`) and
        // the program view via `apply_pane_title_right_cluster`. When the pane is
        // focused it stays at full brightness; when unfocused it dims to match
        // the unfocused title-bar border. Hover always wins regardless of focus.
        // The chat/PTY session view passes `matrix_close` as the base hue.
        let theme = Theme::default();
        let base = theme.matrix_close;

        let focused = session_menu_icon_style(&theme, base, false, true);
        let unfocused = session_menu_icon_style(&theme, base, false, false);
        let hovered_focused = session_menu_icon_style(&theme, base, true, true);
        let hovered_unfocused = session_menu_icon_style(&theme, base, true, false);

        // Same base color whether focused or not — only brightness changes.
        assert_eq!(focused.fg, Some(theme.matrix_close));
        assert_eq!(unfocused.fg, Some(theme.matrix_close));

        // Focused: bright (no DIM). Unfocused: dimmed.
        assert!(
            !focused.add_modifier.contains(Modifier::DIM),
            "focused menu icon should stay at full brightness"
        );
        assert!(
            unfocused.add_modifier.contains(Modifier::DIM),
            "unfocused menu icon should be dimmed"
        );

        // Hover overrides focus state entirely: bold themed text, never dimmed.
        for hovered in [hovered_focused, hovered_unfocused] {
            assert_eq!(hovered.fg, Some(theme.text));
            assert!(hovered.add_modifier.contains(Modifier::BOLD));
            assert!(!hovered.add_modifier.contains(Modifier::DIM));
        }
    }

    #[test]
    fn program_title_menu_icon_matches_program_border_color() {
        // In the PROGRAM view's title bar the session-actions ☰ glyph should be
        // drawn in the program border color (the cyan accent the program frame
        // uses) rather than the default chat/PTY session-view close hue. The
        // unfocused-dim and hover behavior from #551 must still compose: focused
        // → border hue, unfocused → border hue + DIM, hover → bold themed text.
        let theme = Theme::default();

        // Derive the base hue the same way the program render path does, so the
        // icon can't drift from the border color it's meant to match.
        let focused_border = program_border_style(&theme, true);
        let unfocused_border = program_border_style(&theme, false);
        let base = focused_border.fg.unwrap_or(theme.accent_alt);

        // The base IS the program border color, and it's distinct from the
        // session-view default (matrix_close) — otherwise this would be a no-op.
        assert_eq!(Some(base), focused_border.fg);
        assert_eq!(
            focused_border.fg, unfocused_border.fg,
            "program border hue is focus-independent"
        );
        assert_ne!(
            base, theme.matrix_close,
            "program icon must not reuse the session-view close hue"
        );

        let focused = session_menu_icon_style(&theme, base, false, true);
        let unfocused = session_menu_icon_style(&theme, base, false, false);
        let hovered = session_menu_icon_style(&theme, base, true, true);

        // Focused: program border hue at full brightness (matches the frame).
        assert_eq!(focused.fg, focused_border.fg);
        assert!(!focused.add_modifier.contains(Modifier::DIM));

        // Unfocused: same hue, dimmed (tracks the dimmed program border).
        assert_eq!(unfocused.fg, focused_border.fg);
        assert!(unfocused.add_modifier.contains(Modifier::DIM));

        // Hover still wins: bold themed text, never the border hue, never dimmed.
        assert_eq!(hovered.fg, Some(theme.text));
        assert!(hovered.add_modifier.contains(Modifier::BOLD));
        assert!(!hovered.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn program_title_left_layout_places_run_between_name_and_marker() {
        // The Run button now lives in the LEFT cluster: directly after the
        // ` <glyph> <label>` prefix and left of the ` * modified` marker.
        let rect = Rect::new(0, 0, 100, 12);
        let summary = summary_with_mode("smith", Some("interactive"));
        let summary_ref = Some(&summary);

        let layout = program_title_left_layout(summary_ref, "sess", rect, true, true);
        let run = layout.run.expect("run button fits at this width");
        let modified = layout.modified.expect("dirty marker present");

        assert_eq!(run.2, rect.y, "run sits on the title row");
        let glyph_w = UnicodeWidthStr::width(program_mode_glyph()) as u16;
        let label_w = UnicodeWidthStr::width(layout.label.as_str()) as u16;
        assert_eq!(
            run.0,
            rect.x + 3 + glyph_w + label_w,
            "run starts right after ` <glyph> <label>`"
        );
        assert_eq!(
            run.1 - run.0,
            UnicodeWidthStr::width(PROGRAM_RUN_BUTTON) as u16,
            "run hit spans the ▶ button width"
        );
        assert!(
            run.1 <= modified.0,
            "run {run:?} must sit left of the modified marker {modified:?}"
        );

        // The mode toggle stays far left of the Run button.
        let toggle = program_title_toggle_button_range(summary_ref, rect).expect("toggle range");
        assert!(
            toggle.1 <= run.0,
            "toggle {toggle:?} sits left of run {run:?}"
        );
    }

    #[test]
    fn program_title_left_layout_clears_shared_right_cluster() {
        // The left cluster (label + Run + dirty marker) is budgeted so it never
        // overruns the space reserved for the shared right cluster (harness +
        // close), mirroring how the session view budgets its title label. Use a
        // narrow pane so the label budget actually bites.
        let rect = Rect::new(0, 0, 40, 12);
        let summary = summary_with_mode("smith", Some("interactive"));
        let summary_ref = Some(&summary);

        let layout = program_title_left_layout(summary_ref, "sess", rect, true, true);
        let run = layout.run.expect("run fits");
        let modified = layout.modified.expect("dirty marker present");
        let left_extent = modified.1.max(run.1);

        let harness_w = (2 + UnicodeWidthStr::width(harness_label(&summary).as_str())) as u16;
        let close_w = 3u16;
        let right_cluster_left = rect.x + rect.width - harness_w - close_w;
        assert!(
            left_extent <= right_cluster_left,
            "left cluster (ends {left_extent}) must clear the right cluster (begins {right_cluster_left})"
        );
    }

    #[test]
    fn chat_mode_ignores_pty_events() {
        let pty = SessionEvent::Pty {
            data: "AQID".into(),
        };
        let resize = SessionEvent::PtyResize { cols: 80, rows: 24 };
        assert_eq!(chat_event_kind(&pty), ChatEventKind::Hidden);
        assert_eq!(chat_event_kind(&resize), ChatEventKind::Hidden);
    }

    #[test]
    fn chat_mode_filters_codex_bootstrap_messages() {
        assert_eq!(
            chat_event_kind(&SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "<permissions instructions>hide me".into(),
            }),
            ChatEventKind::Hidden
        );
        assert_eq!(
            chat_event_kind(&SessionEvent::Message {
                role: MessageRole::User,
                text: "# AGENTS.md instructions for /tmp/repo\n<INSTRUCTIONS>hide me".into(),
            }),
            ChatEventKind::Hidden
        );
        assert_eq!(
            chat_event_kind(&SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "hello".into(),
            }),
            ChatEventKind::AssistantMessage
        );
    }

    #[test]
    fn chat_mode_aggregates_streaming_assistant_chunks() {
        let at = chrono::Utc::now();
        let events = vec![
            TimestampedEvent {
                seq: 1,
                at,
                event: SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: "hel".into(),
                },
            },
            TimestampedEvent {
                seq: 2,
                at,
                event: SessionEvent::Message {
                    role: MessageRole::Assistant,
                    text: "lo".into(),
                },
            },
        ];
        let lines = chat_lines(&Theme::default(), &events);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("agent: hello"));
    }

    #[test]
    fn chat_mode_aggregates_streaming_reasoning_chunks() {
        let at = chrono::Utc::now();
        let events = vec![
            TimestampedEvent {
                seq: 1,
                at,
                event: SessionEvent::Reasoning {
                    text: "thin".into(),
                },
            },
            TimestampedEvent {
                seq: 2,
                at,
                event: SessionEvent::Reasoning {
                    text: "king".into(),
                },
            },
        ];
        let lines = chat_lines(&Theme::default(), &events);
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("thinking: thinking"));
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

    /// User-reported regression: smith emits `\x1b[2m` (DIM/faint)
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
             tile renders smith's gray markers at full intensity"
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
    fn split_list_pane_keeps_title_bar_when_collapsed() {
        // Collapsed: the rain panel shrinks to just its 1-row title bar,
        // pinned at the bottom of the list pane, while the list keeps the rest.
        let inner = Rect::new(0, 0, 20, 30);
        let (list, matrix) = split_list_pane(inner, true, None);
        assert_eq!(list.height, inner.height - 1);
        assert_eq!(matrix.height, 1);
        assert_eq!(matrix.y, list.y + list.height);
        assert_eq!(list.x, inner.x);
        assert_eq!(matrix.x, inner.x);
    }

    #[test]
    fn split_list_pane_drops_collapsed_title_bar_when_pane_too_short() {
        // When the pane can't keep the list's minimum height above the title
        // bar, the rain goes fully out of view and the list takes everything.
        let inner = Rect::new(0, 0, 20, crate::app::SESSION_LIST_H_MIN);
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
    fn editor_pane_renders_ready_hint_when_idle() {
        let state = crate::app::EditorState {
            queued: Vec::new(),
            buf: String::new(),
            cursor: 0,
            completions: Vec::new(),
        };
        let theme = Theme::default();
        let backend = ratatui::backend::TestBackend::new(24, 3);
        let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");

        terminal
            .draw(|f| {
                render_editor_pane(f, Rect::new(0, 0, 24, 3), Some(&state), None, &theme, false);
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        let glyph_cell = buffer.cell((0, 1)).expect("glyph cell");
        let hint_cell = buffer.cell((2, 1)).expect("hint cell");

        assert_eq!(glyph_cell.style().fg, Some(theme.accent));
        assert_eq!(hint_cell.symbol(), "t");
        assert_eq!(hint_cell.style().fg, Some(theme.dim));
    }

    #[test]
    fn editor_pane_rows_uses_ready_hint() {
        let state = crate::app::EditorState {
            queued: Vec::new(),
            buf: String::new(),
            cursor: 0,
            completions: Vec::new(),
        };
        assert_eq!(editor_pane_rows(Some(&state), None, 24), 3);
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
        assert!(is_headless(&summary_with_mode("smith", Some("headless"))));
        assert!(!is_headless(&summary_with_mode(
            "smith",
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
            harness_label(&summary_with_mode("smith", Some("headless"))),
            "(headless) smith"
        );
        // Interactive and mode-less sessions render the bare harness.
        assert_eq!(
            harness_label(&summary_with_mode("smith", Some("interactive"))),
            "smith"
        );
        assert_eq!(harness_label(&summary_with_mode("shell", None)), "shell");
    }

    #[test]
    fn approval_mode_modeline_label_shows_manual_for_smith() {
        let s = summary_with_mode("smith", Some("interactive"));
        assert_eq!(approval_mode_modeline_label(&s), Some("manual"));
    }

    #[test]
    fn approval_mode_modeline_label_hides_manual_for_shell() {
        let s = summary_with_mode("shell", Some("interactive"));
        assert_eq!(approval_mode_modeline_label(&s), None);
    }

    #[test]
    fn approval_mode_modeline_label_uses_non_manual_badge() {
        let mut s = summary_with_mode("smith", Some("interactive"));
        s.approval_mode = agentd_protocol::ApprovalMode::UnsafeAuto;
        assert_eq!(approval_mode_modeline_label(&s), Some("unsafe-auto"));
    }

    #[test]
    fn smith_running_animates_only_while_agent_active() {
        let mut s = summary_with_mode("smith", Some("interactive"));
        s.state = SessionState::Running;
        // Mid-turn: agent active → animate, even with no recent PTY bytes.
        assert!(session_should_animate_status(&s, false, true));
        // Running but the turn has ended (agent inactive) → stay static,
        // even though the lifecycle state still reads Running. This is the
        // idle-smith regression: PR #179 spun the glyph here.
        assert!(!session_should_animate_status(&s, false, false));
        assert!(!session_should_animate_status(&s, true, false));
    }

    #[test]
    fn shell_running_status_uses_pty_activity_gate() {
        let mut s = summary_with_mode("shell", None);
        s.state = SessionState::Running;
        // Shell has no agent-status signal; gate on recent PTY bytes
        // (agent_active is irrelevant for non-smith harnesses).
        assert!(!session_should_animate_status(&s, false, false));
        assert!(session_should_animate_status(&s, true, false));
    }

    #[test]
    fn awaiting_input_status_stays_static() {
        let mut s = summary_with_mode("smith", Some("interactive"));
        s.state = SessionState::AwaitingInput;
        // Not Running → never animates, regardless of activity signals.
        assert!(!session_should_animate_status(&s, true, true));
    }

    fn widget(markdown: &str) -> agentd_protocol::UiPanel {
        agentd_protocol::UiPanel {
            id: "w".into(),
            source: None,
            title: None,
            created_at_ms: 0,
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

    #[test]
    fn parse_markdown_table_detects_gfm() {
        let lines = vec![
            "| Name | Status |",
            "| --- | :---: |",
            "| build | ok |",
            "| test | fail |",
        ];
        let (table, consumed) = parse_markdown_table(&lines, 0).expect("table");
        assert_eq!(consumed, 4);
        assert_eq!(table.header, vec!["Name", "Status"]);
        assert_eq!(table.aligns, vec![CellAlign::Left, CellAlign::Center]);
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[1], vec!["test", "fail"]);
    }

    #[test]
    fn parse_markdown_table_tolerates_missing_outer_pipes() {
        let lines = vec!["a | b", "--- | ---:", "1 | 2"];
        let (table, _) = parse_markdown_table(&lines, 0).expect("table");
        assert_eq!(table.header, vec!["a", "b"]);
        assert_eq!(table.aligns, vec![CellAlign::Left, CellAlign::Right]);
        assert_eq!(table.rows[0], vec!["1", "2"]);
    }

    #[test]
    fn parse_markdown_table_requires_a_delimiter_row() {
        // A paragraph that merely contains a pipe must not become a table.
        let lines = vec!["a | b is a sentence", "more prose"];
        assert!(parse_markdown_table(&lines, 0).is_none());
    }

    #[test]
    fn table_cell_text_collapses_links_and_emphasis() {
        assert_eq!(table_cell_text("**bold**"), "bold");
        assert_eq!(table_cell_text("[Run](agentd:action/run)"), "Run");
        assert_eq!(table_cell_text("see [x](y) end"), "see x end");
        assert_eq!(table_cell_text("[keep] me"), "[keep] me");
    }

    #[test]
    fn render_markdown_table_emits_header_rule_and_rows() {
        let lines = vec!["| A | B |", "| --- | --- |", "| 1 | 2 |"];
        let (table, _) = parse_markdown_table(&lines, 0).unwrap();
        let rendered = render_markdown_table(&table, &Theme::default(), Rect::new(0, 0, 40, 10));
        assert_eq!(rendered.len(), 3, "header + rule + one body row");
    }
}
