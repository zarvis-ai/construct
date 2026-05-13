//! Ratatui rendering for the TUI.

use crate::app::{App, ListItem as AppListItem, PaneFocus, Selection, ViewMode};
use agentd_protocol::{MessageRole, SessionEvent, SessionState, SessionSummary, TimestampedEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    if app.zoomed {
        render_zoomed(f, area, app);
        return;
    }
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let main_area = vertical[0];
    let modeline_area = vertical[1];
    let minibuffer_area = vertical[2];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(40), Constraint::Min(0)])
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
        let strip_h = pin_strip_height(right_area.height);
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
    for parser in app.terminals.values_mut() {
        parser
            .screen_mut()
            .set_size(inner_rows.max(1), inner_cols.max(1));
    }
    apply_focused_scrollback(app);

    render_sessions(f, cols[0], app);
    render_detail(f, detail_area, app);
    if let Some(strip) = pin_strip_area {
        render_pin_strip(f, strip, app, &pinned_ids);
    }
    render_modeline(f, modeline_area, app);
    render_minibuffer(f, minibuffer_area, app);
    if app.help_visible {
        render_help(f, area);
    }
}

fn pin_strip_height(total_h: u16) -> u16 {
    (total_h / 3).clamp(7, 18)
}

/// Apply the user's scrollback offset to the currently-focused session's
/// vt100 parser so the rendered view shows older content when the user
/// has scrolled up with the mouse wheel. vt100 0.16+ clamps internally,
/// so we just hand it whatever the user dialed in.
fn apply_focused_scrollback(app: &mut App) {
    let Some(id) = app.selected_id() else { return; };
    let Some(parser) = app.terminals.get_mut(&id) else { return; };
    parser.screen_mut().set_scrollback(app.view_scrollback);
}

/// Zoom layout: the session view takes the entire screen except for the
/// minibuffer line at the bottom. No list, no pin strip, no modeline, no
/// borders — edge-to-edge so the underlying TUI (vim / claude / htop /
/// whatever is running) gets the most real estate possible. Matches
/// tmux's `prefix z` zoomed-pane behavior.
fn render_zoomed(f: &mut Frame, area: Rect, app: &mut App) {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);
    let main_area = vertical[0];
    let minibuffer_area = vertical[1];

    app.terminal_pane_size = (main_area.width, main_area.height);
    // Match the parsers to the zoomed area before drawing (see comment in
    // the normal-layout branch).
    for parser in app.terminals.values_mut() {
        parser
            .screen_mut()
            .set_size(main_area.height.max(1), main_area.width.max(1));
    }
    apply_focused_scrollback(app);

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

fn render_sessions(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.focus == PaneFocus::List;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(" sessions ");

    let app_items = app.list_items();
    let mut selected_idx: Option<usize> = None;
    let items: Vec<ListItem> = app_items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            if item.matches(&app.selection) {
                selected_idx = Some(i);
            }
            match item {
                AppListItem::Session { summary: s, indented } => {
                    let pin_glyph = if s.pinned { "◆" } else { " " };
                    let indent_prefix = if *indented { "  " } else { "" };
                    let secondary = s.last_prompt.clone().unwrap_or_default();
                    let secondary = shorten(&secondary, 28);
                    ListItem::new(Line::from(vec![
                        Span::raw(indent_prefix.to_string()),
                        Span::styled(
                            pin_glyph.to_string(),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::styled(
                            format!(" {} ", session_status_glyph(app, s)),
                            state_style(s.state),
                        ),
                        Span::styled(primary_label(s), Style::default().fg(Color::White)),
                        Span::raw("  "),
                        Span::styled(
                            format!("{:<7}", s.harness),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::raw(" "),
                        Span::styled(secondary, Style::default().fg(Color::Gray)),
                    ]))
                }
                AppListItem::GroupHeader { group, member_count } => {
                    let glyph = if group.collapsed { "▶" } else { "▼" };
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("{glyph} "),
                            Style::default().fg(Color::Magenta),
                        ),
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
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White)
    };
    let mut state = ListState::default();
    state.select(if matches!(app.selection, Selection::None) {
        None
    } else {
        selected_idx
    });
    let list = List::new(items).block(block).highlight_style(highlight_style);
    f.render_stateful_widget(list, area, &mut state);
}

fn render_detail(f: &mut Frame, area: Rect, app: &App) {
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
    let title_inner = match (summary, app.selected_group()) {
        (Some(s), _) => format!(
            " {} {}  {}  {} ",
            session_status_glyph(app, s),
            primary_label(s),
            s.harness,
            s.state.label()
        ),
        (None, Some(g)) => format!(" group: {} ", g.name),
        (None, None) => " no session ".to_string(),
    };
    let view_label = match app.view {
        ViewMode::Terminal => "[terminal]",
        ViewMode::Transcript => "[transcript]",
    };
    let title = format!("{title_inner}{view_label} ");
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(pane_border_style(focused))
        .title(title);
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
    lines.push(Line::from(vec![
        Span::styled(
            format!("Group: {}", group.name),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
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

fn render_terminal(f: &mut Frame, area: Rect, app: &App) {
    let Some(id) = app.selected_id() else { return; };
    let Some(parser) = app.terminals.get(&id) else {
        let hint = Paragraph::new("(no PTY history yet — interact to populate)")
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(hint, area);
        return;
    };
    let screen = parser.screen();
    let term = tui_term::widget::PseudoTerminal::new(screen);
    f.render_widget(term, area);
}

fn render_transcript(f: &mut Frame, area: Rect, app: &App) {
    let lines: Vec<Line> = app.transcript.iter().map(format_event).collect();
    let total = lines.len() as u16;
    let height = area.height;
    let max_scroll = total.saturating_sub(height);
    let scroll = if app.transcript_scroll == u16::MAX {
        max_scroll
    } else {
        app.transcript_scroll.min(max_scroll)
    };
    let para = Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(Wrap { trim: false });
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
    let modeline = format!(
        " agentd  [{profile}]  focus:{focus}  {sel}  {state}  {model}  {scrollback}{chord}{status}{conn} ",
        profile = app.profile.label(),
        focus = focus_label,
        scrollback = scrollback_label,
        sel = match s {
            Some(s) => primary_label(s),
            None => "-".into(),
        },
        state = match s {
            Some(s) => s.state.label().to_string(),
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
        status = app
            .status
            .as_ref()
            .map(|(m, _)| m.as_str())
            .unwrap_or(""),
    );
    let para = Paragraph::new(modeline).style(
        Style::default()
            .bg(Color::DarkGray)
            .fg(Color::White),
    );
    f.render_widget(para, area);
}

fn render_minibuffer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(mb) = &app.minibuffer {
        let mut spans = vec![
            Span::raw(mb.prompt.clone()),
            Span::raw(mb.input.clone()),
        ];
        if let Some(err) = &mb.error {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            ));
        }
        let para = Paragraph::new(Line::from(spans));
        f.render_widget(para, area);
        let x = area.x + mb.prompt.width() as u16 + mb.cursor as u16;
        f.set_cursor_position(Position { x, y: area.y });
    } else {
        // Help hint — when the PTY has the keys, all chords need C-x first.
        let hint = if app.help_visible {
            String::new()
        } else if app.zoomed {
            "zoomed — C-x z to unzoom   C-x x palette   ? help".to_string()
        } else if matches!(app.focus, PaneFocus::View)
            && app.view == ViewMode::Terminal
            && app.selected_session().map(|s| s.has_pty).unwrap_or(false)
        {
            "C-x o focus list   C-x z zoom   C-x x palette   ? help".to_string()
        } else {
            "? for help   M-x or C-x x for commands   C-x o other-window".to_string()
        };
        let para = Paragraph::new(hint).style(Style::default().fg(Color::DarkGray));
        f.render_widget(para, area);
    }
}

fn render_help(f: &mut Frame, area: Rect) {
    let height = (HELP_TEXT.lines().count() as u16 + 2).min(area.height.saturating_sub(2));
    let width = 64u16.min(area.width.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help (any key to close) ");
    let para = Paragraph::new(HELP_TEXT)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(Clear, popup);
    f.render_widget(para, popup);
}

const HELP_TEXT: &str = "
emacs keymap (default; AGENTD_KEYMAP=vim for vim profile)

  focus + view
    C-x o / Tab     switch focus (list ↔ view)
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
        SessionEvent::Diff { patch } => vec![Span::raw(format!(
            "   Δ {}",
            shorten(patch, 200)
        ))],
        SessionEvent::Error { message } => vec![Span::styled(
            format!("   ! {message}"),
            Style::default().fg(Color::Red),
        )],
        SessionEvent::Done { exit_code } => vec![Span::styled(
            format!("   ▢ done (exit {exit_code})"),
            Style::default().fg(Color::Green),
        )],
        SessionEvent::Pty { data } => vec![Span::styled(
            format!("   ⌷ pty: {} bytes (switch to terminal view)", data.len()),
            Style::default().fg(Color::DarkGray),
        )],
    }
}

fn pane_border_style(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn render_pin_strip(f: &mut Frame, area: Rect, app: &App, pinned_ids: &[String]) {
    if pinned_ids.is_empty() || area.height < 3 || area.width < 6 {
        return;
    }
    let tiles = pin_tile_layout(area, pinned_ids.len());
    let selected_id = app.selected_id();
    for (tile_area, id) in tiles.iter().zip(pinned_ids.iter()) {
        let summary = app.sessions.iter().find(|s| &s.id == id);
        let is_selected = selected_id.as_deref() == Some(id.as_str());
        let title = match summary {
            Some(s) => format!(
                " ◆ {} {} {} ",
                session_status_glyph(app, s),
                primary_label(s),
                s.harness
            ),
            None => format!(" ◆ {} ", short_id(id)),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(pane_border_style(is_selected))
            .title(title);
        let inner = block.inner(*tile_area);
        f.render_widget(block, *tile_area);
        if let Some(parser) = app.terminals.get(id) {
            render_pty_tail(f, inner, parser.screen());
        } else {
            // No PTY data yet — show a placeholder.
            let p = Paragraph::new("(no data yet)")
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(p, inner);
        }
    }
}

fn pin_tile_layout(area: Rect, n: usize) -> Vec<Rect> {
    let n = n.max(1);
    let cols = n.min(4).max(1);
    let rows = (n + cols - 1) / cols;
    let row_constraints: Vec<Constraint> =
        (0..rows).map(|_| Constraint::Ratio(1, rows as u32)).collect();
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
/// attributes. The window is anchored at the cursor row so a fresh session
/// (cursor near the top) shows its prompt, and a busy session (cursor near
/// the bottom) shows its most recent activity. Used by the pin strip.
fn render_pty_tail(f: &mut Frame, area: Rect, screen: &vt100::Screen) {
    let (rows, cols) = screen.size();
    if rows == 0 || cols == 0 || area.width == 0 || area.height == 0 {
        return;
    }
    let visible_h = area.height.min(rows);
    let visible_w = area.width.min(cols);
    let (cursor_row, _) = screen.cursor_position();
    // End of window is exclusive; show at least visible_h rows starting from 0.
    let end_row = (cursor_row + 1).max(visible_h).min(rows);
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
        SessionState::AwaitingInput => Style::default().fg(Color::Magenta),
        SessionState::Paused => Style::default().fg(Color::Yellow),
        SessionState::Done => Style::default().fg(Color::Cyan),
        SessionState::Errored => Style::default().fg(Color::Red),
    }
}

fn role_style(role: MessageRole) -> Style {
    match role {
        MessageRole::User => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        MessageRole::Assistant => Style::default().fg(Color::LightGreen),
        MessageRole::System => Style::default().fg(Color::DarkGray),
        MessageRole::Tool => Style::default().fg(Color::Yellow),
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
        SessionEvent::Done { exit_code } => format!("done (exit {exit_code})"),
        SessionEvent::Pty { data } => format!("pty: {} bytes", data.len()),
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
