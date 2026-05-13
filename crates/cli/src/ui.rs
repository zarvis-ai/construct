//! Ratatui rendering for the TUI.

use crate::app::{App, PaneFocus, ViewMode};
use agentd_protocol::{MessageRole, SessionEvent, SessionState, TimestampedEvent};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
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
    let detail_area = cols[1];
    // Inner area inside the borders is the PTY's pane size.
    let inner_cols = detail_area.width.saturating_sub(2);
    let inner_rows = detail_area.height.saturating_sub(2);
    app.terminal_pane_size = (inner_cols, inner_rows);

    render_sessions(f, cols[0], app);
    render_detail(f, detail_area, app);
    render_modeline(f, modeline_area, app);
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
    let items: Vec<ListItem> = app
        .sessions
        .iter()
        .map(|s| {
            let glyph = s.state.glyph();
            let secondary = s
                .title
                .clone()
                .or_else(|| s.last_prompt.clone())
                .unwrap_or_default();
            let secondary = shorten(&secondary, 32);
            let line = Line::from(vec![
                Span::styled(format!(" {} ", glyph), state_style(s.state)),
                Span::styled(
                    short_id(&s.id).to_string(),
                    Style::default().fg(Color::White),
                ),
                Span::raw("  "),
                Span::styled(
                    format!("{:<7}", s.harness),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(" "),
                Span::styled(secondary, Style::default().fg(Color::Gray)),
            ]);
            ListItem::new(line)
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
    state.select(if app.sessions.is_empty() {
        None
    } else {
        Some(app.selected.min(app.sessions.len() - 1))
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
    let title_inner = match summary {
        Some(s) => format!(
            " {} {}  {}  {} ",
            s.state.glyph(),
            short_id(&s.id),
            s.harness,
            s.state.label()
        ),
        None => " no session ".to_string(),
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

    match app.view {
        ViewMode::Terminal => render_terminal(f, inner, app),
        ViewMode::Transcript => render_transcript(f, inner, app),
    }
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
    let modeline = format!(
        " agentd  [{profile}]  focus:{focus}  {sel}  {state}  {model}  {chord}{status}{conn} ",
        profile = app.profile.label(),
        focus = focus_label,
        sel = match s {
            Some(s) => short_id(&s.id).to_string(),
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
        let text = format!("{}{}", mb.prompt, mb.input);
        let para = Paragraph::new(text);
        f.render_widget(para, area);
        let x = area.x + mb.prompt.width() as u16 + mb.cursor as u16;
        f.set_cursor_position(Position { x, y: area.y });
    } else {
        // Help hint — when the PTY has the keys, all chords need C-x first.
        let hint = if app.help_visible {
            String::new()
        } else if matches!(app.focus, PaneFocus::View)
            && app.view == ViewMode::Terminal
            && app.selected_session().map(|s| s.has_pty).unwrap_or(false)
        {
            "C-x o focus list   C-x x palette   C-x t transcript   ? help".to_string()
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
    C-n / down      next session
    C-p / up        prev session

  session actions
    C-x C-f         new session
    C-x i           send input to selected session
    C-x k           delete selected session (confirms; kills if running)
    C-x d           show diff
    C-x r           refresh
    C-c C-c         interrupt

  scrollback
    C-v / M-v       scroll page down/up
    g g / G         scroll top / bottom

  global
    M-x / C-x x     command palette (C-x x is Meta-free)
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
