//! Interactive (PTY) mode for zarvis.
//!
//! Zarvis doesn't spawn a child — there's no CLI to attach a real PTY
//! to. Instead we synthesize a terminal session: we emit
//! `SessionEvent::Pty` bytes that look like a chat-style REPL (banner
//! + colored prompt + streaming assistant text + inline tool blocks +
//! inline approval prompts), and we read keystrokes from
//! `AdapterInboxMsg::PtyInput` through a minimal line editor.
//!
//! The TUI's `vt100`-backed terminal pane parses these bytes the same
//! way it parses any other PTY-backed adapter's output.

use crate::agent::{push_msg, system_prompt_for_env, ResolvedModel};
use crate::context;
use crate::persist::{self, Persist};
use crate::provider::{self, Content, Message, Role, StopReason, TextSink, ToolCall};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const TOOL_OUTPUT_BUDGET: usize = 8_000;

/// Wrapper around `EventEmitter` that writes raw bytes / styled text to
/// the session's PTY stream.
struct Terminal<'a> {
    emit: &'a EventEmitter,
}
impl<'a> Terminal<'a> {
    fn new(emit: &'a EventEmitter) -> Self {
        Self { emit }
    }
    fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.emit.emit(SessionEvent::pty(bytes));
    }
    fn print(&self, s: &str) {
        self.write(s.as_bytes());
    }
    fn newline(&self) {
        self.write(b"\r\n");
    }
    fn prompt(&self) {
        // Bold cyan `❯ `.
        self.write(b"\r\n\x1b[1;36m\xe2\x9d\xaf \x1b[0m");
    }
    /// Banner shown when the session starts.
    fn banner(&self, provider: &str, model: &str, automode: bool) {
        let mode_badge = if automode { "  [automode]" } else { "" };
        let banner = format!(
            "\r\n\x1b[1;35mzarvis\x1b[0m  \x1b[2m{provider}:{model}\x1b[0m{mode_badge}\r\n\
             \x1b[2mtype your prompt and press Enter. type `/` for commands, \
             Tab to complete. C-c interrupts a turn. C-d to end.\x1b[0m\r\n",
        );
        self.write(banner.as_bytes());
    }
    fn tool_use(&self, name: &str, args_summary: &str) {
        let line = format!(
            "\r\n\x1b[1;33m→ {name}\x1b[0m\x1b[2m({args_summary})\x1b[0m\r\n"
        );
        self.write(line.as_bytes());
    }
    fn tool_result(&self, ok: bool, output: &str) {
        let glyph = if ok { "\x1b[1;32m✓\x1b[0m" } else { "\x1b[1;31m✗\x1b[0m" };
        // Print a short single-line preview of the result; full content
        // is in the transcript (we also emit ToolResult).
        let one_line: String = output.lines().next().unwrap_or("").chars().take(160).collect();
        let line = format!("  {glyph}  \x1b[2m{one_line}\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
    /// Open a tool-block region in the PTY stream with a custom OSC
    /// marker. Ratatui clients use this as a fence: the bytes between
    /// the open and matching close are zarvis's truncated rendering,
    /// which the items-model renderer skips in favor of synthesizing
    /// its own representation from the structured `ToolUse` /
    /// `ToolResult` events (and which can therefore expand/collapse
    /// in place). Non-ratatui consumers (CLI tail, browser, MCP raw
    /// view) see only the inline bytes — the OSC is invisible — so
    /// their output stays sensible.
    fn tool_block_open(&self, call_id: &str) {
        let open = format!("\x1b]7700;open;call={}\x07", call_id);
        self.write(open.as_bytes());
    }
    fn tool_block_close(&self, call_id: &str) {
        let close = format!("\x1b]7700;close;call={}\x07", call_id);
        self.write(close.as_bytes());
    }
    /// Render a tool's result body (glyph row + truncated preview +
    /// optional `[+N lines — click to expand]` footer). Caller is
    /// responsible for wrapping with [`tool_block_open`] /
    /// [`tool_block_close`] — and typically writing a [`tool_use`]
    /// header line in between so the entire block is fenced.
    fn tool_result_body(&self, ok: bool, output: &str) {
        let glyph = if ok {
            "\x1b[1;32m✓\x1b[0m"
        } else {
            "\x1b[1;31m✗\x1b[0m"
        };
        let total_lines = output.lines().count();
        for (i, line) in output.lines().take(TOOL_BLOCK_MAX_LINES).enumerate() {
            let trimmed: String = line.chars().take(TOOL_BLOCK_MAX_COLS).collect();
            if i == 0 {
                let payload =
                    format!("  {glyph}  \x1b[2m{trimmed}\x1b[0m\r\n");
                self.write(payload.as_bytes());
            } else {
                let payload = format!("     \x1b[2m{trimmed}\x1b[0m\r\n");
                self.write(payload.as_bytes());
            }
        }
        if total_lines == 0 {
            let payload = format!("  {glyph}  \x1b[2m(no output)\x1b[0m\r\n");
            self.write(payload.as_bytes());
        }
        if total_lines > TOOL_BLOCK_MAX_LINES {
            let remaining = total_lines - TOOL_BLOCK_MAX_LINES;
            let footer = format!(
                "     \x1b[2;36m[+{remaining} lines — click to expand]\x1b[0m\r\n"
            );
            self.write(footer.as_bytes());
        }
    }
    fn approval(&self, tool: &str, args_summary: &str, risk: ToolRisk) {
        let risk_label = match risk {
            ToolRisk::Safe => "safe",
            ToolRisk::Risky => "risky",
        };
        let line = format!(
            "\r\n\x1b[1;33m? approve [{risk_label}]\x1b[0m {tool}\x1b[2m({args_summary})\x1b[0m\
             — \x1b[1m[y]\x1b[0mes / \x1b[1m[n]\x1b[0mo / \x1b[1m[a]\x1b[0mutomode: "
        );
        self.write(line.as_bytes());
    }
    fn note(&self, msg: &str) {
        let line = format!("\r\n\x1b[2m{msg}\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
    /// Inline acknowledgement that a user line was captured while the
    /// agent was mid-turn — it'll fire on the next AwaitingInput.
    /// Dim cyan so it reads as "future intent" rather than current
    /// agent activity. Includes a leading + trailing CRLF so the
    /// marker doesn't clobber whatever the agent is currently
    /// writing on its line (there's still some visual artifact on
    /// terminals with strict cursor tracking, but it's bounded to
    /// one line of scrollback).
    /// Recolor the `❯ ` prefix of the row immediately above the cursor
    /// to "queued" white. Called after the user submits during a tool
    /// run so the now-lifted line reads as pending in the queue. Uses
    /// DECSC/DECRC (`ESC 7` / `ESC 8`) — the SCO `\x1b[s` / `\x1b[u`
    /// pair isn't honored by every parser.
    fn retro_color_queued_above(&self) {
        // ESC 7 save, up 1, col 0, white ❯+space, ESC 8 restore.
        self.write(b"\x1b7\x1b[1A\r\x1b[1;37m\xe2\x9d\xaf \x1b[0m\x1b8");
    }
    /// Walk `rows` lines upward from cursor, recoloring each `❯ ` to
    /// gray (the "consumed / historical" color). Used when the agent
    /// dequeues the queue at the start of its next turn.
    fn retro_color_consumed(&self, rows: usize) {
        if rows == 0 {
            return;
        }
        let mut out: Vec<u8> = Vec::with_capacity(8 + rows * 16);
        out.extend_from_slice(b"\x1b7");
        for _ in 0..rows {
            out.extend_from_slice(b"\x1b[1A\r\x1b[90m\xe2\x9d\xaf \x1b[0m");
        }
        out.extend_from_slice(b"\x1b8");
        self.write(&out);
    }
    /// Wipe `rows` rendered queued lines plus the active editor line
    /// below them. After this call the cursor is at column 0 of the
    /// topmost erased row, ready for the editor's redraw to repaint
    /// the active prompt with recalled content.
    fn erase_queue_and_active(&self, rows: usize) {
        if rows == 0 {
            self.write(b"\r\x1b[2K");
            return;
        }
        let cmd = format!("\x1b[{rows}A\r\x1b[J");
        self.write(cmd.as_bytes());
    }
    /// Wipe the active editor row (so an agent-text stream below doesn't
    /// land beside an orphan `❯`) and back up onto the line above so
    /// the stream's leading `\r\n` lands on the just-cleared row.
    fn erase_active_for_stream(&self) {
        self.write(b"\r\x1b[2K\x1b[1A");
    }
    /// Echo a consumed line into the chat scrollback as a gray `❯`
    /// prompt — used at queue-dequeue to record "what the agent is
    /// answering right now" in the chat history (the editor pane sees
    /// the line vanish from `queued` at the same moment).
    fn echo_consumed_line(&self, line: &str) {
        self.write(&consumed_line_echo_bytes(line));
    }
}

fn consumed_line_echo_bytes(line: &str) -> Vec<u8> {
    let payload = if line.chars().count() > 240 {
        let head: String = line.chars().take(237).collect();
        format!("{head}...")
    } else {
        line.to_string()
    };
    let mut out: Vec<u8> = Vec::with_capacity(payload.len() + 32);
    // Two leading `\r\n`s leave a blank row above the echo so the
    // previous agent paragraph (or any other chat content) gets
    // visual separation before the user's submission.
    out.extend_from_slice(b"\r\n\r\n\x1b[90m\xe2\x9d\xaf \x1b[0m");
    let mut first = true;
    for logical in payload.split('\n') {
        if first {
            first = false;
        } else {
            out.extend_from_slice(b"\r\n\x1b[90m  \x1b[0m");
        }
        out.extend_from_slice(logical.as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn emit_agent_status(
    emit: &EventEmitter,
    started_at_ms: i64,
    status: &str,
) {
    emit.emit(SessionEvent::AgentStatus(agentd_protocol::AgentStatus {
        active: true,
        started_at_ms,
        status: status.to_string(),
    }));
}

fn finish_agent_status(emit: &EventEmitter, started_at_ms: i64, status: &str) {
    emit.emit(SessionEvent::AgentStatus(agentd_protocol::AgentStatus {
        active: false,
        started_at_ms,
        status: status.to_string(),
    }));
}

/// Emit a `SessionEvent::EditorState` snapshot reflecting the editor
/// buf, cursor, and pending queue. Called at every state change so the
/// TUI's input pane stays in sync. Each queued entry is sent as a
/// single string (newlines preserved); the TUI renders the first line
/// with the `❯` glyph and continuation lines with a matching indent.
fn emit_editor_state(
    emit: &EventEmitter,
    editor: &LineEditor,
    queue: &VecDeque<String>,
) {
    emit.emit(SessionEvent::EditorState {
        queued: queue.iter().cloned().collect(),
        buf: editor.buf.clone(),
        cursor: editor.cursor,
        completions: editor
            .slash_matches()
            .into_iter()
            .map(str::to_string)
            .collect(),
    });
}

/// Lines whose start matches one of these labels are dimmed in the PTY
/// (the structured Message event still carries the raw text — this is
/// purely a rendering tweak). Cheap to extend; keep the entries short
/// so the at-start-of-line buffer stays tiny.
const DIM_LINE_PREFIXES: &[&str] = &["Summary:"];

/// Available slash commands surfaced by inline ghost-completion and
/// Tab-accept in the interactive line editor. Order matters — when
/// multiple commands share a prefix, the first one wins as the ghost.
/// Keep in lockstep with the `match trimmed { ... }` block that handles
/// them after submit.
/// Commands surfaced by the `/` popup + tab completion. `/model` is
/// adapter-internal (zarvis switches its own provider). Everything
/// else is dispatched as a `tui` ToolUse for the TUI's slash table,
/// except `/loop` which is parsed inline and turned into an
/// `agentd_loop_create` call. Keep this in lockstep with the
/// after-submit match block + the TUI's `run_slash_command`.
const SLASH_COMMANDS: &[&str] = &[
    "/help",
    "/loop",
    "/model",
    "/new",
    "/quit",
    "/reset",
    "/exit",
    "/refresh",
    "/rename",
    "/send",
    "/tasks",
    "/zoom",
];

/// Padding around the assistant's streamed response. The response is
/// rendered as a chat bubble: `❯` marks user input (the line editor's
/// prompt), `●` marks the response's first line, continuation lines
/// indent under the text-after-the-dot. Top is a blank line; bottom
/// padding is zero because the TUI's editor pane already supplies the
/// visual separator above the active `❯`. Right is implemented as
/// soft-wrap at `width - PAD_RIGHT`.
const PAD_TOP: usize = 1;
const PAD_BOTTOM: usize = 0;
const PAD_RIGHT: usize = 2;
/// First-line marker for the response. `\xe2\x97\x8f` = `●`.
const RESPONSE_BULLET: &[u8] = b"\xe2\x97\x8f ";
/// Continuation indent that aligns the wrapped text under the text
/// after the bullet. Must visually match the bullet's cell width
/// (1 dot + 1 space = 2 cells).
const RESPONSE_INDENT: &[u8] = b"  ";
/// Left margin in visible cells (counts toward soft-wrap math).
const LEFT_MARGIN_CELLS: usize = 2;
/// Hard floor on usable width so a tiny pane doesn't crash the wrap
/// math.
const MIN_USABLE_WIDTH: usize = 20;

/// Update `pty_width` for a new terminal column count. Pure on purpose:
/// it takes no `Terminal` / emitter, so the resize handler structurally
/// cannot paint to PTY. The active `❯` lives in the TUI's bottom editor
/// pane (fed by `EditorState`); painting `editor.redraw()` here would
/// leave a stray cyan `❯` in chat scrollback duplicating the prompt.
fn apply_pty_resize(cols: u16, pty_width: &mut usize) {
    *pty_width = (cols as usize).max(MIN_USABLE_WIDTH + LEFT_MARGIN_CELLS + PAD_RIGHT);
}

/// Max preview lines per tool result rendered in the PTY. Output
/// beyond this lands behind a `[+N lines — click to expand]` footer
/// (the full output is still in the transcript and in the model's
/// context). Tuned to balance "I can see what the tool did" against
/// "every read_file blowing out the scrollback."
const TOOL_BLOCK_MAX_LINES: usize = 5;
/// Per-line truncation inside a tool block, before the wrap math
/// gets involved. Just a backstop against pathological single-line
/// outputs from `shell` calls.
const TOOL_BLOCK_MAX_COLS: usize = 200;

/// Sink for the interactive mode: deltas go directly to the PTY (with
/// dim-line styling for `Summary:` etc. + top/bottom/left/right padding
/// around the whole response) and as Message events so the transcript
/// still has the raw text.
struct PtySink<'a> {
    emit: &'a EventEmitter,
    at_line_start: bool,
    in_dim_line: bool,
    /// Buffered chars seen at the start of the current line while we
    /// decide whether they match a `DIM_LINE_PREFIXES` entry. Bounded
    /// by the longest prefix length, so streaming UX stays snappy.
    prefix_buf: String,
    /// Current pane width in cells; used to soft-wrap at the right
    /// gutter so the visible right margin matches `pad_right`.
    width: usize,
    /// True once `delta` has emitted anything — top padding fires once
    /// at first delta, bottom padding fires in `finalize`.
    emitted: bool,
    /// Visible column inside the padded block (0 = just past left
    /// padding). ASCII-counted; CJK chars may misalign.
    col: usize,
    /// When true, every delta also fires a `SessionEvent::Message` so
    /// the daemon's transcript view sees the streaming text. Off for
    /// replay paths where the message is already in the transcript.
    emit_messages: bool,
    /// Start time of the current live turn, used for `AgentStatus`.
    status_started_at_ms: i64,
}
impl<'a> PtySink<'a> {
    fn new(emit: &'a EventEmitter, width: usize, status_started_at_ms: i64) -> Self {
        Self {
            emit,
            at_line_start: true,
            in_dim_line: false,
            prefix_buf: String::new(),
            width,
            emitted: false,
            col: 0,
            emit_messages: true,
            status_started_at_ms,
        }
    }
    /// Replay constructor — emits PTY bytes only, never `Message`
    /// events. Used by the resize-redraw path where the messages are
    /// already in the transcript.
    fn new_replay(emit: &'a EventEmitter, width: usize) -> Self {
        Self {
            emit_messages: false,
            ..Self::new(emit, width, now_ms())
        }
    }

    fn usable_width(&self) -> usize {
        self.width
            .saturating_sub(LEFT_MARGIN_CELLS + PAD_RIGHT)
            .max(MIN_USABLE_WIDTH)
    }

    fn open_block(&mut self, out: &mut Vec<u8>) {
        for _ in 0..PAD_TOP {
            out.extend_from_slice(b"\r\n");
        }
        out.push(b'\r');
        out.extend_from_slice(RESPONSE_BULLET);
        self.col = 0;
        self.at_line_start = true;
        self.emitted = true;
    }

    fn newline(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(RESPONSE_INDENT);
        self.col = 0;
        self.at_line_start = true;
    }

    /// Emit any tail state + bottom padding. Called by the agent loop
    /// once `provider.complete` returns and the streamed text portion
    /// is done.
    fn finalize(&mut self) {
        if !self.emitted {
            return;
        }
        let mut out: Vec<u8> = Vec::with_capacity(32);
        // If we still have a partial dim-prefix candidate buffered
        // (e.g., the model ended on "Summa" without a colon), flush it
        // verbatim so we don't lose bytes.
        if !self.prefix_buf.is_empty() {
            out.extend_from_slice(self.prefix_buf.as_bytes());
            self.prefix_buf.clear();
        }
        if self.in_dim_line {
            out.extend_from_slice(b"\x1b[0m");
            self.in_dim_line = false;
        }
        for _ in 0..PAD_BOTTOM {
            out.extend_from_slice(b"\r\n");
        }
        if !out.is_empty() {
            self.emit.emit(SessionEvent::pty(&out));
        }
    }
}
impl<'a> TextSink for PtySink<'a> {
    fn delta(&mut self, text: &str) {
        if self.emit_messages {
            emit_agent_status(self.emit, self.status_started_at_ms, "Working");
        }
        let mut out: Vec<u8> = Vec::with_capacity(text.len() + 32);
        if !self.emitted {
            self.open_block(&mut out);
        }
        for c in text.chars() {
            if c == '\n' {
                if !self.prefix_buf.is_empty() {
                    out.extend_from_slice(self.prefix_buf.as_bytes());
                    self.prefix_buf.clear();
                }
                if self.in_dim_line {
                    out.extend_from_slice(b"\x1b[0m");
                    self.in_dim_line = false;
                }
                self.newline(&mut out);
                continue;
            }
            // Soft-wrap once we hit the right gutter. Buffered prefix
            // chars don't count toward col yet; once they're flushed
            // we'll wrap on subsequent chars as needed.
            if !self.at_line_start && self.col >= self.usable_width() {
                let was_dim = self.in_dim_line;
                if was_dim {
                    out.extend_from_slice(b"\x1b[0m");
                }
                self.newline(&mut out);
                if was_dim {
                    out.extend_from_slice(b"\x1b[2m");
                    self.in_dim_line = true; // newline() reset us to at_line_start
                    self.at_line_start = false;
                }
            }
            if self.at_line_start && !self.in_dim_line {
                self.prefix_buf.push(c);
                if let Some(matched) = DIM_LINE_PREFIXES
                    .iter()
                    .find(|p| **p == self.prefix_buf.as_str())
                {
                    out.extend_from_slice(b"\x1b[2m");
                    out.extend_from_slice(matched.as_bytes());
                    self.col += matched.chars().count();
                    self.prefix_buf.clear();
                    self.in_dim_line = true;
                    self.at_line_start = false;
                    continue;
                }
                let still_candidate = DIM_LINE_PREFIXES
                    .iter()
                    .any(|p| p.starts_with(self.prefix_buf.as_str()));
                if !still_candidate {
                    out.extend_from_slice(self.prefix_buf.as_bytes());
                    self.col += self.prefix_buf.chars().count();
                    self.prefix_buf.clear();
                    self.at_line_start = false;
                }
                continue;
            }
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            out.extend_from_slice(s.as_bytes());
            self.col += 1;
        }
        if !out.is_empty() {
            self.emit.emit(SessionEvent::pty(&out));
        }
        if self.emit_messages {
            // Transcript copy stays raw (unpadded).
            self.emit.emit(SessionEvent::Message {
                role: agentd_protocol::MessageRole::Assistant,
                text: text.to_string(),
            });
        }
    }
}

/// Readline-ish line editor — handles printable chars, cursor
/// navigation (arrows + C-a/C-e/C-b/C-f), history (↑/↓ + C-p/C-n),
/// killing (C-k/C-u/C-w/Backspace/Delete), Enter, C-c, C-d, plus a
/// slash-command popup below the prompt with Tab common-prefix
/// completion.
///
/// The editor consumes raw PTY bytes through [`feed_bytes`], returning
/// (a) bytes to write back to the PTY (incremental echoes or full
/// line redraws) and (b) any top-level events (Submit / Interrupt /
/// Eof) the caller should act on.
struct LineEditor {
    buf: String,
    /// Char index of the cursor within `buf` (0 = before first char).
    cursor: usize,
    history: Vec<String>,
    /// `None` = editing current line; `Some(i)` = viewing
    /// `history[history.len() - 1 - i]` with the editing buffer saved
    /// in `saved`.
    hist_pos: Option<usize>,
    saved: String,
    /// ANSI escape sequence state.
    esc: EscState,
    /// What we re-emit to redraw the prompt; produced fresh each frame
    /// from `(prompt_seq, buf, cursor)`.
    prompt_seq: &'static [u8],
    /// Visible cell width of `prompt_seq` (SGR escapes don't count).
    /// Used for absolute-column cursor positioning after the popup.
    prompt_visible_width: usize,
    /// Current prompt row width in cells. Used to make vertical cursor
    /// movement follow visual wraps as well as explicit newlines.
    width: usize,
    /// Number of popup lines rendered in the last redraw — we erase
    /// these many lines below the prompt at the start of the next
    /// redraw so a shrinking popup doesn't leave stale text.
    last_popup_lines: usize,
    /// "Queued recall": pending-input the user enqueued while the
    /// agent was busy, exposed to up-arrow so they can pull the
    /// whole batch back into the editor and edit it as one prompt
    /// before it executes. `None` when the queue is empty. Set by
    /// `enqueue_line` (caller-managed) and cleared on dequeue or
    /// after a recall.
    queued_recall: Option<String>,
}

#[derive(Default)]
enum EscState {
    #[default]
    Idle,
    /// Just saw ESC (0x1b).
    Esc,
    /// Saw ESC [ — collecting params until a final byte.
    Csi {
        params: String,
    },
    /// Saw ESC O — accept exactly one final byte.
    Ss3,
}

#[derive(Debug)]
enum LineEvent {
    Submit(String),
    Interrupt,
    Eof,
    /// Up-arrow recalled the pending input queue into the editor.
    /// The outer loop should drain its queue (its content is now
    /// in `editor.buf` for editing) so the recalled text doesn't
    /// double-execute.
    DequeueRecall,
}

impl LineEditor {
    fn new(prompt_seq: &'static [u8], prompt_visible_width: usize) -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_pos: None,
            saved: String::new(),
            esc: EscState::Idle,
            prompt_seq,
            prompt_visible_width,
            width: 80,
            last_popup_lines: 0,
            queued_recall: None,
        }
    }

    fn set_width(&mut self, width: usize) {
        self.width = width.max(self.prompt_visible_width + 1);
    }

    /// Caller-managed mirror of the input queue's current combined
    /// form. Set to `Some(joined)` when something is queued; cleared
    /// on dequeue or recall. Up-arrow hits this first.
    fn set_queued_recall(&mut self, recall: Option<String>) {
        self.queued_recall = recall;
    }

    /// Bytes the terminal needs to repaint the current line + the
    /// slash-command popup below it. Erases any popup rendered by the
    /// previous call (tracked in `last_popup_lines`) before redrawing.
    fn redraw(&mut self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(256);
        // 1. Erase the previous popup area, if any. Each `\r\n\x1b[K`
        //    advances to the next line and clears it; we then `\x1b[NA`
        //    back up to the prompt line.
        let n_prev = self.last_popup_lines;
        for _ in 0..n_prev {
            out.extend_from_slice(b"\r\n\x1b[K");
        }
        if n_prev > 0 {
            let mv = format!("\x1b[{n_prev}A");
            out.extend_from_slice(mv.as_bytes());
        }
        // 2. Repaint the prompt line.
        out.extend_from_slice(b"\r\x1b[K");
        out.extend_from_slice(self.prompt_seq);
        out.extend_from_slice(self.buf.as_bytes());
        // 3. Render the slash-command popup below.
        let matches = self.slash_matches();
        let n = matches.len();
        for m in &matches {
            out.extend_from_slice(b"\r\n\x1b[K");
            out.extend_from_slice(b"  \x1b[90m");
            out.extend_from_slice(m.as_bytes());
            out.extend_from_slice(b"\x1b[0m");
        }
        // 4. Move the cursor back up to the prompt row and to the
        //    column the user is editing at (absolute-column via
        //    `\x1b[<col>G`, 1-based).
        if n > 0 {
            let mv = format!("\x1b[{n}A");
            out.extend_from_slice(mv.as_bytes());
        }
        let target_col = self.prompt_visible_width + self.cursor + 1;
        let mv = format!("\x1b[{target_col}G");
        out.extend_from_slice(mv.as_bytes());
        self.last_popup_lines = n;
        out
    }

    /// Erase the popup (called before submit so the next prompt isn't
    /// painted on top of stale popup lines).
    fn clear_popup(&mut self, out: &mut Vec<u8>) {
        let n_prev = self.last_popup_lines;
        for _ in 0..n_prev {
            out.extend_from_slice(b"\r\n\x1b[K");
        }
        if n_prev > 0 {
            let mv = format!("\x1b[{n_prev}A");
            out.extend_from_slice(mv.as_bytes());
            // Re-anchor cursor at column = prompt + buf
            let target_col = self.prompt_visible_width + self.cursor + 1;
            let mv = format!("\x1b[{target_col}G");
            out.extend_from_slice(mv.as_bytes());
        }
        self.last_popup_lines = 0;
    }

    /// Slash-commands whose names start with the current buffer.
    /// Empty when the buf doesn't start with `/`.
    fn slash_matches(&self) -> Vec<&'static str> {
        if !self.buf.starts_with('/') {
            return Vec::new();
        }
        SLASH_COMMANDS
            .iter()
            .copied()
            .filter(|c| c.starts_with(self.buf.as_str()))
            .collect()
    }

    /// Tab handler: complete the buffer to the matches' common prefix.
    /// When only one match remains and the buffer already equals it,
    /// append a trailing space so the user can type the argument.
    /// Returns true when the buffer changed.
    fn accept_via_tab(&mut self) -> bool {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return false;
        }
        let prefix = common_prefix(&matches);
        if prefix.chars().count() > self.buf.chars().count() {
            self.buf = prefix;
            if matches.len() == 1 && self.buf == matches[0] {
                self.buf.push(' ');
            }
            self.cursor = self.buf.chars().count();
            return true;
        }
        if matches.len() == 1 && self.buf == matches[0] {
            self.buf.push(' ');
            self.cursor = self.buf.chars().count();
            return true;
        }
        false
    }

    fn submit(&mut self) -> LineEvent {
        let line = std::mem::take(&mut self.buf);
        self.cursor = 0;
        self.hist_pos = None;
        self.saved.clear();
        if !line.is_empty()
            && self.history.last().map(|s| s.as_str()) != Some(line.as_str())
        {
            self.history.push(line.clone());
        }
        LineEvent::Submit(line)
    }

    fn reset_history(&mut self) {
        self.history.clear();
        self.hist_pos = None;
        self.saved.clear();
        self.queued_recall = None;
    }

    fn ascii_at(&self, n: usize) -> Option<char> {
        self.buf.chars().nth(n)
    }

    fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }
    fn move_right(&mut self) {
        if self.cursor < self.buf.chars().count() {
            self.cursor += 1;
        }
    }
    fn move_home(&mut self) {
        self.cursor = 0;
    }
    fn move_end(&mut self) {
        self.cursor = self.buf.chars().count();
    }

    fn insert_char(&mut self, c: char) {
        let byte_idx = char_index_to_byte(&self.buf, self.cursor);
        self.buf.insert(byte_idx, c);
        self.cursor += 1;
    }
    fn insert_newline(&mut self) {
        self.insert_char('\n');
    }

    fn move_vertical(&mut self, up: bool) -> bool {
        let chars: Vec<char> = self.buf.chars().collect();
        let rows = visual_editor_rows(&chars, self.editor_text_width());
        let Some(row_idx) = current_visual_row(&rows, self.cursor) else {
            return false;
        };
        let target_idx = if up {
            row_idx.checked_sub(1)
        } else if row_idx + 1 < rows.len() {
            Some(row_idx + 1)
        } else {
            None
        };
        let Some(target_idx) = target_idx else {
            return false;
        };
        let col = visual_width_between(&chars, rows[row_idx].start, self.cursor);
        let target = &rows[target_idx];
        self.cursor = cursor_for_visual_col(&chars, target.start, target.end, col);
        true
    }

    fn editor_text_width(&self) -> usize {
        self.width
            .saturating_sub(self.prompt_visible_width)
            .max(1)
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev_byte = char_index_to_byte(&self.buf, self.cursor - 1);
        let cur_byte = char_index_to_byte(&self.buf, self.cursor);
        self.buf.replace_range(prev_byte..cur_byte, "");
        self.cursor -= 1;
    }
    fn delete_forward(&mut self) {
        let total = self.buf.chars().count();
        if self.cursor >= total {
            return;
        }
        let cur_byte = char_index_to_byte(&self.buf, self.cursor);
        let next_byte = char_index_to_byte(&self.buf, self.cursor + 1);
        self.buf.replace_range(cur_byte..next_byte, "");
    }
    fn kill_to_end(&mut self) {
        let cur_byte = char_index_to_byte(&self.buf, self.cursor);
        self.buf.truncate(cur_byte);
    }
    fn kill_to_home(&mut self) {
        let cur_byte = char_index_to_byte(&self.buf, self.cursor);
        self.buf.replace_range(..cur_byte, "");
        self.cursor = 0;
    }
    fn kill_word_back(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let chars: Vec<char> = self.buf.chars().collect();
        let mut i = self.cursor;
        // Skip trailing whitespace.
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        // Then skip the word.
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        let start_byte = char_index_to_byte(&self.buf, i);
        let cur_byte = char_index_to_byte(&self.buf, self.cursor);
        self.buf.replace_range(start_byte..cur_byte, "");
        self.cursor = i;
    }

    fn history_prev(&mut self, events: &mut Vec<LineEvent>) {
        // First up-arrow when there's pending-input recall pulls
        // the queued batch back into the editor. The outer loop
        // hears `DequeueRecall` and drains its queue accordingly.
        if let Some(recall) = self.queued_recall.take() {
            self.saved = self.buf.clone();
            self.hist_pos = None;
            self.buf = recall;
            self.cursor = self.buf.chars().count();
            events.push(LineEvent::DequeueRecall);
            return;
        }
        if self.history.is_empty() {
            return;
        }
        let new_pos = match self.hist_pos {
            None => 0,
            Some(i) if i + 1 < self.history.len() => i + 1,
            Some(i) => i,
        };
        if self.hist_pos.is_none() {
            self.saved = self.buf.clone();
        }
        self.hist_pos = Some(new_pos);
        self.buf = self.history[self.history.len() - 1 - new_pos].clone();
        self.cursor = self.buf.chars().count();
    }
    fn history_next(&mut self) {
        let Some(i) = self.hist_pos else { return };
        if i == 0 {
            self.hist_pos = None;
            self.buf = std::mem::take(&mut self.saved);
        } else {
            self.hist_pos = Some(i - 1);
            self.buf = self.history[self.history.len() - i].clone();
        }
        self.cursor = self.buf.chars().count();
    }

    /// Feed a chunk of raw PTY bytes from the user. Returns (bytes to
    /// write back to the terminal, top-level events).
    fn feed_bytes(&mut self, input: &[u8]) -> (Vec<u8>, Vec<LineEvent>) {
        let mut out: Vec<u8> = Vec::new();
        let mut events: Vec<LineEvent> = Vec::new();
        for &b in input {
            self.step_byte(b, &mut out, &mut events);
        }
        (out, events)
    }

    fn step_byte(&mut self, b: u8, out: &mut Vec<u8>, events: &mut Vec<LineEvent>) {
        // ESC sequence handling first.
        match &mut self.esc {
            EscState::Idle => {
                if b == 0x1b {
                    self.esc = EscState::Esc;
                    return;
                }
            }
            EscState::Esc => {
                match b {
                    b'[' => {
                        self.esc = EscState::Csi { params: String::new() };
                        return;
                    }
                    b'O' => {
                        self.esc = EscState::Ss3;
                        return;
                    }
                    _ => {
                        // ESC followed by something else — drop both.
                        self.esc = EscState::Idle;
                        return;
                    }
                }
            }
            EscState::Csi { params } => {
                // Parameter bytes are digits, `;`, or `?`. Final byte is
                // anything in 0x40..=0x7E.
                if (b'0'..=b'9').contains(&b) || b == b';' || b == b'?' {
                    params.push(b as char);
                    return;
                }
                let params = std::mem::take(params);
                self.esc = EscState::Idle;
                self.handle_csi_final(b, &params, out, events);
                return;
            }
            EscState::Ss3 => {
                self.esc = EscState::Idle;
                self.handle_ss3_final(b, out, events);
                return;
            }
        }

        // Plain (non-ESC) byte.
        match b {
            // Enter
            b'\r' => {
                // Wipe any open popup first so the caller's next prompt
                // doesn't paint on top of stale lines.
                self.clear_popup(out);
                events.push(self.submit());
                out.extend_from_slice(b"\r\n");
            }
            // Line feed inserts a prompt newline. The TUI emits LF for
            // modified Enter keys, and terminals naturally send it for C-j.
            b'\n' => {
                self.insert_newline();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-C
            0x03 => events.push(LineEvent::Interrupt),
            // Ctrl-D — Eof if empty buf, else forward-delete.
            0x04 => {
                if self.buf.is_empty() {
                    events.push(LineEvent::Eof);
                } else {
                    self.delete_forward();
                    out.extend_from_slice(&self.redraw());
                }
            }
            // Ctrl-A / Home
            0x01 => {
                self.move_home();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-E / End
            0x05 => {
                self.move_end();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-B
            0x02 => {
                self.move_left();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-F
            0x06 => {
                self.move_right();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-K (kill to end)
            0x0b => {
                self.kill_to_end();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-U (kill to start)
            0x15 => {
                self.kill_to_home();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-W (kill word back)
            0x17 => {
                self.kill_word_back();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-P (line up, then history prev)
            0x10 => {
                if !self.move_vertical(true) {
                    self.history_prev(events);
                }
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-N (line down, then history next)
            0x0e => {
                if !self.move_vertical(false) {
                    self.history_next();
                }
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-L — clear screen + redraw.
            0x0c => {
                out.extend_from_slice(b"\x1b[2J\x1b[H");
                out.extend_from_slice(&self.redraw());
            }
            // Tab — common-prefix completion against the slash popup.
            0x09 => {
                if self.accept_via_tab() {
                    out.extend_from_slice(&self.redraw());
                }
            }
            // Backspace / DEL
            0x08 | 0x7f => {
                if self.cursor > 0 {
                    self.backspace();
                    out.extend_from_slice(&self.redraw());
                }
            }
            // Other control bytes: ignore.
            b if b < 0x20 => {}
            // Printable ASCII / UTF-8 byte. UTF-8 continuation bytes get
            // collected by str::from_utf8 elsewhere; here we just treat
            // any 0x20+ byte as a char insert for simplicity (works for
            // ASCII; multi-byte UTF-8 sequences from a terminal will
            // each get inserted as separate chars, which mangles them —
            // acceptable for v1, fixable by buffering a UTF-8 decoder).
            b => {
                let c = b as char;
                self.insert_char(c);
                // Always full-redraw so the slash-command ghost
                // suggestion is recomputed against the new buf
                // (previously we just echoed the char when the cursor
                // was at the end — that left stale ghost bytes on
                // screen when the typed char diverged from the
                // suggestion).
                out.extend_from_slice(&self.redraw());
            }
        }
    }

    fn handle_csi_final(
        &mut self,
        final_byte: u8,
        params: &str,
        out: &mut Vec<u8>,
        events: &mut Vec<LineEvent>,
    ) {
        match final_byte {
            b'A' => {
                if !self.move_vertical(true) {
                    self.history_prev(events);
                }
            }
            b'B' => {
                if !self.move_vertical(false) {
                    self.history_next();
                }
            }
            b'C' => self.move_right(),
            b'D' => self.move_left(),
            b'H' => self.move_home(),
            b'F' => self.move_end(),
            // `\x1b[3~` = Delete; `\x1b[1~` = Home; `\x1b[4~` = End;
            // `\x1b[7~`/`\x1b[8~` are Linux-console variants.
            b'~' => match params {
                "1" | "7" => self.move_home(),
                "4" | "8" => self.move_end(),
                "3" => self.delete_forward(),
                _ => return, // unknown — no redraw
            },
            _ => return,
        }
        out.extend_from_slice(&self.redraw());
    }

    fn handle_ss3_final(
        &mut self,
        final_byte: u8,
        out: &mut Vec<u8>,
        events: &mut Vec<LineEvent>,
    ) {
        match final_byte {
            b'A' => {
                if !self.move_vertical(true) {
                    self.history_prev(events);
                }
            }
            b'B' => {
                if !self.move_vertical(false) {
                    self.history_next();
                }
            }
            b'C' => self.move_right(),
            b'D' => self.move_left(),
            b'H' => self.move_home(),
            b'F' => self.move_end(),
            _ => return,
        }
        out.extend_from_slice(&self.redraw());
    }
}

/// Longest common-character prefix across all entries. Returns an
/// empty string for an empty slice. Operates on chars, so it's
/// UTF-8-safe.
fn common_prefix(strs: &[&str]) -> String {
    let first = match strs.first() {
        Some(s) => *s,
        None => return String::new(),
    };
    let mut out = String::new();
    for (i, c) in first.chars().enumerate() {
        let all_match = strs[1..].iter().all(|s| s.chars().nth(i) == Some(c));
        if all_match {
            out.push(c);
        } else {
            break;
        }
    }
    out
}

fn char_index_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[derive(Debug, Clone, Copy)]
struct VisualEditorRow {
    start: usize,
    end: usize,
}

fn visual_editor_rows(chars: &[char], width: usize) -> Vec<VisualEditorRow> {
    use unicode_width::UnicodeWidthChar;

    let width = width.max(1);
    if chars.is_empty() {
        return vec![VisualEditorRow { start: 0, end: 0 }];
    }

    let mut rows = Vec::new();
    let mut start = 0usize;
    let mut col = 0usize;
    for (idx, ch) in chars.iter().enumerate() {
        if *ch == '\n' {
            rows.push(VisualEditorRow { start, end: idx });
            start = idx + 1;
            col = 0;
            continue;
        }

        let ch_width = UnicodeWidthChar::width(*ch).unwrap_or(0);
        if idx > start && col + ch_width > width {
            rows.push(VisualEditorRow {
                start,
                end: idx,
            });
            start = idx;
            col = 0;
        }
        col += ch_width;
    }
    rows.push(VisualEditorRow {
        start,
        end: chars.len(),
    });
    rows
}

fn current_visual_row(rows: &[VisualEditorRow], cursor: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .rev()
        .find(|(_, row)| cursor >= row.start && cursor <= row.end)
        .map(|(idx, _)| idx)
}

fn visual_width_between(chars: &[char], start: usize, end: usize) -> usize {
    use unicode_width::UnicodeWidthChar;

    chars[start..end]
        .iter()
        .map(|ch| UnicodeWidthChar::width(*ch).unwrap_or(0))
        .sum()
}

fn cursor_for_visual_col(chars: &[char], start: usize, end: usize, target_col: usize) -> usize {
    use unicode_width::UnicodeWidthChar;

    let mut col = 0usize;
    for idx in start..end {
        let ch_width = UnicodeWidthChar::width(chars[idx]).unwrap_or(0);
        if col + ch_width > target_col {
            return idx;
        }
        col += ch_width;
    }
    end
}

fn pty_input_requests_interrupt(bytes: &[u8]) -> bool {
    bytes.contains(&0x03) || bytes == [0x1b]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> LineEditor {
        LineEditor::new(b"> ", 2)
    }

    #[test]
    fn apply_pty_resize_updates_width_for_normal_columns() {
        let mut width = 80;
        apply_pty_resize(120, &mut width);
        assert_eq!(width, 120);
        apply_pty_resize(40, &mut width);
        assert_eq!(width, 40);
    }

    #[test]
    fn apply_pty_resize_clamps_below_minimum() {
        let min = MIN_USABLE_WIDTH + LEFT_MARGIN_CELLS + PAD_RIGHT;
        let mut width = 80;
        apply_pty_resize(5, &mut width);
        assert_eq!(width, min, "tiny pane should clamp up to wrap-math floor");
        apply_pty_resize(0, &mut width);
        assert_eq!(width, min, "zero-column resize should also clamp");
    }

    #[test]
    fn consumed_line_echo_uses_crlf_for_multiline_history() {
        let bytes = consumed_line_echo_bytes("a\nb\nc\n");
        let rendered = String::from_utf8(bytes).unwrap();
        assert_eq!(
            rendered,
            "\r\n\r\n\x1b[90m❯ \x1b[0ma\r\n\x1b[90m  \x1b[0mb\r\n\x1b[90m  \x1b[0mc\r\n\x1b[90m  \x1b[0m\r\n"
        );
    }

    fn submit_line(ed: &mut LineEditor, bytes: &[u8]) -> Option<String> {
        let (_, evs) = ed.feed_bytes(bytes);
        for ev in evs {
            if let LineEvent::Submit(s) = ev {
                return Some(s);
            }
        }
        None
    }

    #[test]
    fn simple_typing_and_enter() {
        let mut ed = editor();
        assert_eq!(submit_line(&mut ed, b"hello\r"), Some("hello".into()));
    }

    #[test]
    fn line_feed_inserts_newline_without_submit() {
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(b"hello\nworld");
        assert!(evs.is_empty());
        assert_eq!(ed.buf, "hello\nworld");
        assert_eq!(ed.cursor, 11);
    }

    #[test]
    fn carriage_return_submits_multiline_prompt() {
        let mut ed = editor();
        assert_eq!(
            submit_line(&mut ed, b"hello\nworld\r"),
            Some("hello\nworld".into())
        );
    }

    #[test]
    fn left_arrow_then_insert() {
        // type "ab", left-arrow, insert "X" → "aXb"
        let mut ed = editor();
        ed.feed_bytes(b"ab\x1b[D");
        assert_eq!(ed.cursor, 1);
        ed.feed_bytes(b"X");
        assert_eq!(ed.buf, "aXb");
    }

    #[test]
    fn ctrl_a_then_e() {
        let mut ed = editor();
        ed.feed_bytes(b"hello");
        assert_eq!(ed.cursor, 5);
        ed.feed_bytes(&[0x01]); // C-a
        assert_eq!(ed.cursor, 0);
        ed.feed_bytes(&[0x05]); // C-e
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn ctrl_b_and_f() {
        let mut ed = editor();
        ed.feed_bytes(b"hi");
        ed.feed_bytes(&[0x02]); // C-b
        assert_eq!(ed.cursor, 1);
        ed.feed_bytes(&[0x02]);
        assert_eq!(ed.cursor, 0);
        ed.feed_bytes(&[0x06]); // C-f
        assert_eq!(ed.cursor, 1);
    }

    #[test]
    fn backspace_and_delete() {
        let mut ed = editor();
        ed.feed_bytes(b"hello");
        ed.feed_bytes(&[0x7f]); // DEL → backspace
        assert_eq!(ed.buf, "hell");
        ed.feed_bytes(&[0x01]); // C-a
        ed.feed_bytes(b"\x1b[3~"); // forward Delete
        assert_eq!(ed.buf, "ell");
    }

    #[test]
    fn kill_to_end_and_home() {
        let mut ed = editor();
        ed.feed_bytes(b"hello world");
        ed.feed_bytes(&[0x01]); // C-a
        ed.feed_bytes(&[0x06, 0x06, 0x06, 0x06, 0x06]); // forward 5 chars → cursor at " "
        ed.feed_bytes(&[0x0b]); // C-k
        assert_eq!(ed.buf, "hello");
        ed.feed_bytes(&[0x15]); // C-u (kill to home)
        assert_eq!(ed.buf, "");
    }

    #[test]
    fn ctrl_w_kills_last_word() {
        let mut ed = editor();
        ed.feed_bytes(b"hello world");
        ed.feed_bytes(&[0x17]); // C-w
        assert_eq!(ed.buf, "hello ");
    }

    #[test]
    fn history_with_arrows() {
        let mut ed = editor();
        submit_line(&mut ed, b"one\r");
        submit_line(&mut ed, b"two\r");
        ed.feed_bytes(b"\x1b[A"); // up → "two"
        assert_eq!(ed.buf, "two");
        ed.feed_bytes(b"\x1b[A"); // up → "one"
        assert_eq!(ed.buf, "one");
        ed.feed_bytes(b"\x1b[B"); // down → "two"
        assert_eq!(ed.buf, "two");
        ed.feed_bytes(b"\x1b[B"); // down → saved (empty)
        assert_eq!(ed.buf, "");
    }

    #[test]
    fn history_with_ctrl_p_n() {
        let mut ed = editor();
        submit_line(&mut ed, b"alpha\r");
        submit_line(&mut ed, b"beta\r");
        ed.feed_bytes(&[0x10]); // C-p
        assert_eq!(ed.buf, "beta");
        ed.feed_bytes(&[0x10]);
        assert_eq!(ed.buf, "alpha");
        ed.feed_bytes(&[0x0e]); // C-n
        assert_eq!(ed.buf, "beta");
    }

    #[test]
    fn arrows_move_within_multiline_before_history() {
        let mut ed = editor();
        submit_line(&mut ed, b"history\r");
        ed.feed_bytes(b"abc\nde");
        assert_eq!(ed.cursor, 6);

        ed.feed_bytes(b"\x1b[A");
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 2);

        ed.feed_bytes(b"\x1b[B");
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 6);

        ed.feed_bytes(b"\x1b[A");
        assert_eq!(ed.cursor, 2);
        ed.feed_bytes(b"\x1b[A");
        assert_eq!(ed.buf, "history");
        ed.feed_bytes(b"\x1b[B");
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn ctrl_p_n_move_within_multiline_before_history() {
        let mut ed = editor();
        submit_line(&mut ed, b"history\r");
        ed.feed_bytes(b"abc\nde");

        ed.feed_bytes(&[0x10]); // C-p
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 2);

        ed.feed_bytes(&[0x0e]); // C-n
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 6);

        ed.feed_bytes(&[0x10]);
        assert_eq!(ed.cursor, 2);
        ed.feed_bytes(&[0x10]);
        assert_eq!(ed.buf, "history");
        ed.feed_bytes(&[0x0e]);
        assert_eq!(ed.buf, "abc\nde");
        assert_eq!(ed.cursor, 6);
    }

    #[test]
    fn arrows_move_across_soft_wrapped_rows() {
        let mut ed = editor();
        ed.set_width(5); // 2 cells for prompt, 3 cells for text.
        ed.feed_bytes(b"abcdef");

        ed.feed_bytes(b"\x1b[A");
        assert_eq!(ed.cursor, 3);

        ed.feed_bytes(&[0x01]); // C-a
        ed.feed_bytes(&[0x06, 0x06]); // C-f twice
        ed.feed_bytes(b"\x1b[B");
        assert_eq!(ed.cursor, 5);
    }

    #[test]
    fn ctrl_c_interrupts_and_ctrl_d_eofs() {
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(&[0x03]);
        assert!(matches!(evs.as_slice(), [LineEvent::Interrupt]));
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(&[0x04]);
        assert!(matches!(evs.as_slice(), [LineEvent::Eof]));
    }

    #[test]
    fn pty_interrupt_detection_preserves_escape_sequences() {
        assert!(pty_input_requests_interrupt(&[0x03]));
        assert!(pty_input_requests_interrupt(&[0x1b]));
        assert!(!pty_input_requests_interrupt(b"\x1b[A"));
        assert!(!pty_input_requests_interrupt(b"\x1b[1;5D"));
        assert!(!pty_input_requests_interrupt(b"abc"));
    }

    #[tokio::test]
    async fn kill_supervisor_task_aborts_running_tool() {
        struct DropNotify(Option<tokio::sync::oneshot::Sender<()>>);

        impl Drop for DropNotify {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }

        let tasks = crate::tasks::Tasks::new();
        let (completion_tx, _completion_rx) = tokio::sync::mpsc::unbounded_channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();

        let supervisor = tokio::spawn(crate::tasks::supervise(
            "c1".into(),
            "slow".into(),
            "".into(),
            tasks.clone(),
            completion_tx,
            std::time::Duration::from_secs(60),
            async move {
                let _guard = DropNotify(Some(dropped_tx));
                let _ = started_tx.send(());
                std::future::pending::<std::result::Result<ToolOutcome, String>>().await
            },
        ));

        started_rx.await.expect("tool runner should start");
        let outcome = kill_supervisor_task(&tasks, "c1", supervisor).await;
        assert!(matches!(outcome, crate::tasks::SupervisorOutcome::Killed));
        dropped_rx.await.expect("tool runner should be aborted");
        assert!(tasks.running.lock().await.is_empty());
    }

    #[test]
    fn redraw_renders_popup_below() {
        let mut ed = editor();
        let (out, _) = ed.feed_bytes(b"/");
        let s = String::from_utf8_lossy(&out);
        // Popup uses gray SGR for each entry and lists at least `/model`.
        assert!(s.contains("\x1b[90m"), "missing popup color SGR: {:?}", s);
        assert!(s.contains("/model"), "missing /model entry: {:?}", s);
        // Cursor is repositioned with an absolute-column escape.
        assert!(s.contains("\x1b[") && s.contains("G"), "missing column move: {:?}", s);
    }

    #[test]
    fn slash_matches_narrow_as_typed() {
        let mut ed = editor();
        ed.feed_bytes(b"/");
        let all = ed.slash_matches();
        assert!(all.contains(&"/model"));
        assert!(all.contains(&"/quit"));
        assert!(all.contains(&"/exit"));
        assert!(all.contains(&"/reset"));
        ed.feed_bytes(b"q");
        assert_eq!(ed.slash_matches(), vec!["/quit"]);
        ed.feed_bytes(b"x");
        assert!(ed.slash_matches().is_empty());
    }

    #[test]
    fn tab_completes_common_prefix() {
        let mut ed = editor();
        ed.feed_bytes(b"/m");
        ed.feed_bytes(&[0x09]); // Tab — only /model matches
        assert_eq!(ed.buf, "/model ");
        let mut ed = editor();
        // `/` has 3 matches with no shared chars beyond `/` — Tab is a no-op.
        ed.feed_bytes(b"/");
        let buf_before = ed.buf.clone();
        ed.feed_bytes(&[0x09]);
        assert_eq!(ed.buf, buf_before);
    }

    #[test]
    fn tab_no_match_is_noop() {
        let mut ed = editor();
        ed.feed_bytes(b"hello");
        let buf_before = ed.buf.clone();
        ed.feed_bytes(&[0x09]);
        assert_eq!(ed.buf, buf_before);
    }

    #[test]
    fn common_prefix_unit() {
        assert_eq!(common_prefix(&["/model", "/quit", "/exit"]), "/");
        assert_eq!(common_prefix(&["/quit", "/exit"]), "/");
        assert_eq!(common_prefix(&["/model"]), "/model");
        assert_eq!(common_prefix(&[]), "");
    }

    /// Submit events fired while the agent is mid-turn still come out
    /// of `feed_bytes` — the caller (drive_with_input) is responsible
    /// for pushing them onto the queue. Verify that the editor produces
    /// the Submit for each Enter in a multi-line chunk, so a fast-typing
    /// user gets every queued message captured.
    #[test]
    fn multiple_submits_in_one_chunk() {
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(b"hello\rworld\r");
        let submits: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                LineEvent::Submit(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(submits, vec!["hello", "world"]);
    }
}

/// Tri-state interrupt signal used during in-flight turns.
#[derive(Default)]
struct Interrupted {
    flag: std::sync::atomic::AtomicBool,
}
impl Interrupted {
    fn set(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    fn take(&self) -> bool {
        self.flag.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

pub async fn run(
    params: SessionStartParams,
    ctx: AdapterContext,
    spec: ResolvedModel,
) -> Result<()> {
    let AdapterContext { session_id, emit, mut inbox } = ctx;
    let mut provider_name = spec.provider_name();
    let mut model = spec.model.clone();
    let mut provider = spec.provider;
    let cwd = PathBuf::from(&params.cwd);
    let registry = std::sync::Arc::new(ToolRegistry::with_defaults());
    let specs = registry.specs();
    let mut automode = std::env::var("AGENTD_ZARVIS_AUTOMODE").as_deref() == Ok("1");

    // Per-session task registry: tracks every spawned tool's
    // supervisor handle so manual `[kill]` / `[bg]` clicks can find
    // the right call_id, and so auto-bg can hand off cleanly. The
    // background-completion channel feeds completions back into the
    // agent loop as `OBSERVATION:` synthetic messages.
    let tasks = crate::tasks::Tasks::new();
    let (bg_completion_tx, mut bg_completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tasks::BackgroundCompletion>();
    let bg_after = crate::tasks::bg_after_duration();

    // Project-guide injection: assemble the full system prompt once
    // here, then re-use across every provider.complete call. Read
    // happens at session start (and on resume since we re-enter
    // this function), not per turn — so edits to AGENTS.md mid-
    // session aren't picked up until the session is reopened.
    let system_prompt: String = {
        let base = crate::agent::system_prompt_for_env();
        match crate::project_guide::format_section(&cwd) {
            Some(section) => format!("{base}\n\n{section}"),
            None => base.to_string(),
        }
    };

    let term = Terminal::new(&emit);
    let resuming = persist::is_resume();
    // On resume we emit nothing — banner, note, and prompt all stay off
    // so the PTY looks exactly as it did before the daemon restart. The
    // line editor's redraw on the first keystroke will paint the prompt
    // cleanly. (Pressing Enter on an empty line is also a no-op + fresh
    // prompt, so the user has an explicit "wake me up" escape hatch.)
    if !resuming {
        term.banner(provider_name, &model, automode);
    }
    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: Some(format!("{}:{}  [interactive]", provider_name, model)),
    });
    // The active `❯` lives in the TUI's fixed editor pane, fed by
    // `SessionEvent::EditorState`; no inline prompt write needed.
    let _ = resuming;

    // Clone the id before moving it into ToolCtx — the
    // observation task and slash handlers (e.g. `/loop`) need it.
    let self_id_for_obs = session_id.clone();
    let session_id_for_slash = session_id.clone();
    let tool_ctx = ToolCtx {
        cwd,
        session_id,
        client: tokio::sync::OnceCell::new(),
    };

    // Prompt bytes the line editor will re-emit on every redraw. Must
    // match Terminal::prompt's payload sans the leading `\r\n` (we keep
    // those local to Terminal::prompt because the editor uses bare `\r`
    // for redraw and assumes the prompt sits on a fresh row).
    // The prompt's SGR escapes are invisible; `❯ ` is 2 cells.
    let mut editor = LineEditor::new(b"\x1b[1;36m\xe2\x9d\xaf \x1b[0m", 2);
    let mut pty_width: usize = params
        .pty_size
        .map(|s| s.cols as usize)
        .unwrap_or(80)
        .max(MIN_USABLE_WIDTH + LEFT_MARGIN_CELLS + PAD_RIGHT);
    editor.set_width(pty_width);
    let data_dir = persist::session_data_dir_from_env();
    let mut persist = Persist::open(data_dir.as_deref());
    let mut messages: Vec<Message> = if persist::is_resume() {
        if let Some(p) = persist.as_ref().map(|p| p.path().to_path_buf()) {
            Persist::load(&p).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    // Intentionally silent on resume — the prior PTY state is still
    // visible to the user; no need for a meta-narration line.
    let _ = resuming; // referenced above
    let mut pending: VecDeque<String> = VecDeque::new();
    if !persist::is_resume() {
        if let Some(p) = params.prompt.clone() {
            if !p.trim().is_empty() {
                pending.push_back(p);
            }
        }
    }
    // Submissions captured while the agent was mid-turn. Drained at
    // the top of every outer-loop iteration so the user can keep
    // composing thoughts while the agent works. Independent of
    // `pending` (which is the one-shot startup prompt).
    let mut queue: VecDeque<String> = VecDeque::new();
    // Vestigial — the new model uses `SessionEvent::EditorState` for
    // the bottom input pane, no PTY-side row tracking required.
    let mut queued_rows: usize = 0;
    // Initial editor state so the TUI's bottom pane has something to
    // paint before the first drive call (idle waiting for user input).
    emit_editor_state(&emit, &editor, &queue);

    // Orchestrator-only: subscribe to other sessions' events so the
    // agent can react to fleet activity (sessions finishing, errors,
    // approval requests) without the user polling. Non-orchestrator
    // sessions get `None` here and skip the obs branch in the inner
    // select. Rate-limited so a burst of events can't fire a turn
    // per event.
    let is_orchestrator =
        std::env::var("AGENTD_SESSION_KIND").as_deref() == Ok("orchestrator");
    let mut obs_rx = if is_orchestrator {
        Some(crate::observe::spawn(self_id_for_obs))
    } else {
        None
    };
    let mut obs_limiter =
        crate::observe::RateLimiter::new(5, std::time::Duration::from_secs(60));

    'outer: loop {
        // Wait for a user message — drain order: startup prompt
        // (`pending`), then anything queued during the previous turn,
        // then live typing.
        let user_text = if let Some(t) = pending.pop_front() {
            // Echo the pre-supplied prompt as if the user just sent it.
            term.echo_consumed_line(&t);
            emit_editor_state(&emit, &editor, &queue);
            t
        } else if let Some(t) = queue.pop_front() {
            // Echo the combined consumed text into the chat as a gray
            // `❯` history line so the user has a record of what the
            // agent is now answering. The editor pane updates to
            // empty queued + empty buf via `emit_editor_state`.
            editor.set_queued_recall(None);
            term.echo_consumed_line(&t);
            queued_rows = 0;
            emit_editor_state(&emit, &editor, &queue);
            t
        } else {
            emit.emit(SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            });
            match read_one_line(
                &mut inbox,
                &mut editor,
                &term,
                &mut automode,
                &mut pty_width,
                obs_rx.as_mut(),
                &mut bg_completion_rx,
                &tasks,
            )
            .await
            {
                ReadOutcome::Line(t) => t,
                ReadOutcome::Observation(obs) => {
                    if !obs_limiter.try_consume() {
                        tracing::info!(
                            session = %obs.session_id,
                            "orchestrator observation rate-limited; dropping"
                        );
                        continue 'outer;
                    }
                    let text = obs.as_synthetic_user_message();
                    term.note(&text);
                    text
                }
                ReadOutcome::BackgroundCompletion(bc) => {
                    // Emit the real ToolResult so the transcript +
                    // any MCP/CLI subscribers see the actual output
                    // — replaces the synthetic "(running in
                    // background)" result the LLM was given earlier.
                    let (ok, output_text) = match &bc.outcome {
                        Ok(o) => (o.ok, o.output.clone()),
                        Err(e) => (false, format!("({e})")),
                    };
                    emit.emit(SessionEvent::ToolResult {
                        tool: bc.call_id.clone(),
                        ok,
                        output: output_text.clone(),
                    });
                    // TaskEnd for the daemon's registry — closes
                    // out the entry that's been in `Backgrounded`
                    // state since the auto-bg fired.
                    let preview: String = output_text.chars().take(200).collect();
                    emit.emit(SessionEvent::TaskEnd {
                        call_id: bc.call_id.clone(),
                        ok,
                        output_preview: preview,
                    });
                    // No inline PTY writes — the items-model
                    // renderer already has the original block (from
                    // the earlier TaskStart) and will fill it from
                    // the ToolResult event we emitted above.
                    // Synthesize an OBSERVATION: message so the
                    // agent's next turn knows about the completion.
                    let short_call: String = bc.call_id.chars().take(10).collect();
                    let preview: String = output_text.chars().take(160).collect();
                    let label = if ok { "ok" } else { "failed" };
                    let text = format!(
                        "OBSERVATION: background tool {} ({}) finished {} after {:.1}s. Output: {}",
                        short_call,
                        bc.tool_name,
                        label,
                        bc.duration.as_secs_f64(),
                        preview
                    );
                    term.note(&text);
                    text
                }
                ReadOutcome::Stop => break 'outer,
                ReadOutcome::Eof => {
                    term.note("(end of session)");
                    break 'outer;
                }
            }
        };

        // Slash-command meta inputs: never sent to the model. `/model`
        // is adapter-internal (it switches zarvis state); everything
        // else is delegated to the client via SessionEvent::ClientCommand
        // so the TUI can dispatch its own slash table without an LLM
        // roundtrip.
        let trimmed = user_text.trim();
        if let Some(rest) = trimmed.strip_prefix("/model") {
            let arg = rest.trim();
            if arg.is_empty() {
                term.note(&format!("(model: {}:{})", provider_name, model));
            } else {
                match crate::agent::resolve_model_from_spec(arg) {
                    Ok(new) => {
                        let new_name = new.provider_name();
                        let new_model = new.model.clone();
                        provider = new.provider;
                        provider_name = new_name;
                        model = new_model;
                        term.note(&format!("(model → {}:{})", provider_name, model));
                        emit.emit(SessionEvent::Status {
                            state: SessionState::Running,
                            detail: Some(format!("{}:{}  [interactive]", provider_name, model)),
                        });
                    }
                    Err(e) => {
                        term.note(&format!("(model switch failed: {e})"));
                    }
                }
            }
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }
        // `/loop` is special-cased before the generic tui-dispatch
        // path: it parses the spec inline (asking the LLM for an
        // interval when the user didn't supply one), then calls
        // the agentd_loop_create tool synthetically — same path
        // the LLM would take, just from the adapter side.
        if let Some(rest) = trimmed.strip_prefix("/loop ").or_else(|| {
            (trimmed == "/loop").then_some("")
        }) {
            handle_slash_loop(
                rest,
                &session_id_for_slash,
                &emit,
                &term,
                provider.as_ref(),
                &model,
                &tool_ctx,
            )
            .await;
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }
        if trimmed == "/reset" {
            messages.clear();
            if let Some(p) = persist.as_mut() {
                p.reset();
            }
            pending.clear();
            queue.clear();
            editor.reset_history();
            queued_rows = 0;
            emit.emit(SessionEvent::Reset);
            term.banner(provider_name, &model, automode);
            term.note("(session reset)");
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }
        if let Some((name, args)) = parse_slash_command(trimmed) {
            // Encode as a `tui` tool call so it lives in the same
            // transcript surface as agent tool calls — the client TUI
            // subscribes to ToolUse, recognizes the conventional
            // tool name, and dispatches. We follow up with a
            // synthetic ToolResult immediately so the trace looks
            // like any other completed tool call.
            let tool_args = match &args {
                Some(a) => serde_json::json!({ "command": name, "args": a }),
                None => serde_json::json!({ "command": name }),
            };
            let pretty = match &args {
                Some(a) => format!("/{name} {a}"),
                None => format!("/{name}"),
            };
            // Use a deterministic-enough id so a future repeat shows
            // up as a separate call; the LLM never sees these so
            // collision tolerance is fine.
            let call_id = format!(
                "tui-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            );
            emit.emit(SessionEvent::ToolUse {
                tool: agentd_protocol::TUI_DISPATCH_TOOL.to_string(),
                args: tool_args,
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call_id,
                ok: true,
                output: pretty,
            });
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }
        if trimmed.is_empty() {
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }

        push_msg!(messages, persist, Message {
            role: Role::User,
            content: Content::Text { text: user_text.clone() },
        });
        emit.emit(SessionEvent::Message {
            role: agentd_protocol::MessageRole::User,
            text: user_text,
        });
        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });
        let turn_started_at_ms = now_ms();
        let mut final_status = "Worked";
        emit_agent_status(&emit, turn_started_at_ms, "Working");

        // Inner step loop — feed tool results back until end-of-turn.
        loop {
            let _pruned = context::prune(&mut messages, provider_name, &model);
            let mut sink = PtySink::new(&emit, pty_width, turn_started_at_ms);
            // Wrap the provider call so user typing during the
            // stream is fed to the editor and pressed-Enter lines
            // join the pending-input queue instead of vanishing
            // until the turn ends. Silent variant — the agent is
            // writing to the same PTY, so we can't echo keystrokes
            // without garbling the stream.
            let drive = drive_with_input_silent(
                &mut inbox,
                &mut editor,
                &mut queue,
                &mut automode,
                &emit,
                tasks.clone(),
                async {
                    provider
                        .complete(
                            &model,
                            &system_prompt,
                            &messages,
                            &specs,
                            &mut sink,
                        )
                        .await
                },
            )
            .await;
            let turn = match drive {
                DriveExit::Done(Ok(t)) => {
                    sink.finalize();
                    t
                }
                DriveExit::Done(Err(e)) => {
                    sink.finalize();
                    final_status = "Errored";
                    term.note(&format!("(provider error: {e})"));
                    emit.emit(SessionEvent::Error { message: format!("{e}") });
                    break;
                }
                DriveExit::Stop | DriveExit::Channel => {
                    sink.finalize();
                    finish_agent_status(&emit, turn_started_at_ms, "Stopped");
                    break 'outer;
                }
                DriveExit::Interrupt => {
                    sink.finalize();
                    final_status = "Interrupted";
                    term.note("(interrupted)");
                    break;
                }
            };
            emit.emit(SessionEvent::Cost {
                usd: turn.usage.usd,
                tokens_in: turn.usage.input_tokens,
                tokens_out: turn.usage.output_tokens,
            });

            if turn.tool_calls.is_empty() {
                if let Some(text) = turn.text {
                    push_msg!(messages, persist, Message {
                        role: Role::Assistant,
                        content: Content::Text { text },
                    });
                }
                break;
            }

            push_msg!(messages, persist, Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: turn.text.clone(),
                    calls: turn.tool_calls.clone(),
                },
            });
            // Partition by risk: Safe in parallel, Risky serial through
            // the approval gate. See agent::run for the matching logic.
            let mut safe_idx: Vec<usize> = Vec::new();
            let mut risky_idx: Vec<usize> = Vec::new();
            for (i, c) in turn.tool_calls.iter().enumerate() {
                let r = registry.get(&c.name).map(|t| t.risk()).unwrap_or(ToolRisk::Risky);
                if matches!(r, ToolRisk::Safe) {
                    safe_idx.push(i);
                } else {
                    risky_idx.push(i);
                }
            }

            let mut outcomes: std::collections::BTreeMap<usize, std::result::Result<ToolOutcome, String>> =
                std::collections::BTreeMap::new();
            let mut early_stop = false;

            if !safe_idx.is_empty() {
                emit_agent_status(&emit, turn_started_at_ms, "Working");
                // Precompute display metadata in the main task so the
                // parallel children don't re-derive args summaries.
                let safe_meta: Vec<(usize, provider::ToolCall, String)> = safe_idx
                    .iter()
                    .map(|&i| {
                        let call = turn.tool_calls[i].clone();
                        let summary = registry
                            .get(&call.name)
                            .map(|t| t.args_summary(&call.input))
                            .unwrap_or_else(|| {
                                serde_json::to_string(&call.input)
                                    .unwrap_or_default()
                            });
                        (i, call, summary)
                    })
                    .collect();

                // Spawn each supervisor outside the input-driver
                // future. If the user interrupts, dropping the driver
                // must not drop the supervisor and detach the tool.
                let (safe_tx, mut safe_rx) = tokio::sync::mpsc::unbounded_channel();
                let mut safe_supervisors = Vec::new();
                for (i, call, summary) in &safe_meta {
                    let safe_index = *i;
                    let call_clone = call.clone();
                    let summary_clone = summary.clone();
                    let reg = registry.clone();
                    let emit_c = emit.clone();
                    let ctx_c = crate::agent::clone_tool_ctx(&tool_ctx);
                    let tasks_c = tasks.clone();
                    let bg_tx_c = bg_completion_tx.clone();
                    let bg_after_c = bg_after;
                    let call_id = call.id.clone();
                    let tool_name = call.name.clone();
                    let call_id_for_handle = call_id.clone();
                    let tx = safe_tx.clone();
                    let handle = tokio::spawn(async move {
                        // Each safe call gets its own supervisor —
                        // auto-bg works in parallel just like the
                        // risky serial path. SupervisorOutcome maps
                        // to Result<ToolOutcome, String> for the
                        // existing render pipeline.
                        let tool_runner = async move {
                            run_safe_call_silent(call_clone, &reg, &ctx_c, &emit_c).await
                        };
                        let outcome = crate::tasks::supervise(
                            call_id,
                            tool_name,
                            summary_clone,
                            tasks_c,
                            bg_tx_c,
                            bg_after_c,
                            tool_runner,
                        )
                        .await;
                        let _ = tx.send((safe_index, outcome));
                    });
                    safe_supervisors.push((safe_index, call_id_for_handle, handle));
                }
                drop(safe_tx);

                let render_fut = async {
                    let mut outcomes_map: HashMap<
                        usize,
                        std::result::Result<ToolOutcome, String>,
                    > = HashMap::new();
                    while outcomes_map.len() < safe_meta.len() {
                        let (i, outcome) = match safe_rx.recv().await {
                            Some(item) => item,
                            None => break,
                        };
                        // No inline PTY writes for tool blocks — the
                        // items-model renderer synthesizes them from
                        // the structured ToolUse / ToolResult /
                        // TaskStart / TaskEnd events that the
                        // supervisor already emitted. Leaving the
                        // PTY stream as pure chat content keeps the
                        // user's live prompt + typing visible during
                        // tool waits (those bytes used to be stripped
                        // by the OSC fence).
                        outcomes_map.insert(i, outcome_from_supervisor(outcome));
                    }
                    outcomes_map
                };

                let drive = drive_with_input(
                    &mut inbox,
                    &mut editor,
                    &term,
                    &mut queue,
                    &mut queued_rows,
                    &mut automode,
                    tasks.clone(),
                    render_fut,
                )
                .await;
                match drive {
                    DriveExit::Done(map) => {
                        for (i, outcome) in map {
                            outcomes.insert(i, outcome);
                        }
                    }
                    DriveExit::Stop | DriveExit::Channel => {
                        early_stop = true;
                        for (i, call_id, handle) in safe_supervisors {
                            kill_joined_task(&tasks, &call_id, handle).await;
                            outcomes.entry(i).or_insert_with(|| Err("stop".to_string()));
                        }
                    }
                    DriveExit::Interrupt => {
                        for (i, call_id, handle) in safe_supervisors {
                            kill_joined_task(&tasks, &call_id, handle).await;
                            outcomes
                                .entry(i)
                                .or_insert_with(|| Err("interrupt".to_string()));
                        }
                    }
                }
            }

            if !early_stop {
                for &i in &risky_idx {
                    let call = &turn.tool_calls[i];
                    emit_agent_status(&emit, turn_started_at_ms, "Working");
                    let outcome = run_one_tool(
                        call,
                        &registry,
                        &tool_ctx,
                        &emit,
                        &term,
                        &mut inbox,
                        &mut automode,
                        &mut editor,
                        &mut queue,
                        &mut queued_rows,
                        tasks.clone(),
                        bg_completion_tx.clone(),
                        bg_after,
                    )
                    .await;
                    let stop_now = matches!(outcome.as_ref(), Err(r) if r == "stop");
                    outcomes.insert(i, outcome);
                    if stop_now {
                        early_stop = true;
                        break;
                    }
                }
            }

            // Append messages in model-expected order.
            for i in 0..turn.tool_calls.len() {
                let call = &turn.tool_calls[i];
                let outcome = outcomes
                    .remove(&i)
                    .unwrap_or_else(|| Err("turn aborted before this tool ran".to_string()));
                match outcome {
                    Ok(o) => {
                        let truncated = truncate_for_model(&o.output, TOOL_OUTPUT_BUDGET);
                        push_msg!(messages, persist, Message {
                            role: Role::Tool,
                            content: Content::ToolResult {
                                call_id: call.id.clone(),
                                output: truncated,
                                is_error: !o.ok,
                            },
                        });
                    }
                    Err(reason) => {
                        push_msg!(messages, persist, Message {
                            role: Role::Tool,
                            content: Content::ToolResult {
                                call_id: call.id.clone(),
                                output: format!("(turn aborted: {reason})"),
                                is_error: true,
                            },
                        });
                    }
                }
            }
            if early_stop {
                finish_agent_status(&emit, turn_started_at_ms, "Stopped");
                return Ok(());
            }
            if matches!(turn.stop_reason, StopReason::MaxTokens) {
                break;
            }
        }

        finish_agent_status(&emit, turn_started_at_ms, final_status);
        // Reset the editor pane to empty after the turn ends.
        emit_editor_state(&emit, &editor, &queue);
    }
    Ok(())
}

enum ReadOutcome {
    Line(String),
    /// Orchestrator-only: an observation from another session arrived
    /// while we were waiting for user input. The outer loop turns
    /// this into a pseudo-user message so the agent can react.
    Observation(crate::observe::Observation),
    /// A backgrounded tool just finished. The outer loop emits the
    /// real `ToolResult` event (so the transcript catches up) and
    /// synthesizes an `OBSERVATION:` user message so the agent's
    /// next turn knows about the completion.
    BackgroundCompletion(crate::tasks::BackgroundCompletion),
    Stop,
    Eof,
}

#[allow(clippy::too_many_arguments)]
async fn read_one_line(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    automode: &mut bool,
    pty_width: &mut usize,
    mut obs_rx: Option<&mut tokio::sync::mpsc::UnboundedReceiver<crate::observe::Observation>>,
    bg_completion_rx: &mut tokio::sync::mpsc::UnboundedReceiver<
        crate::tasks::BackgroundCompletion,
    >,
    tasks: &std::sync::Arc<crate::tasks::Tasks>,
) -> ReadOutcome {
    loop {
        let inbox_recv = inbox.recv();
        let obs_recv = async {
            match obs_rx.as_mut() {
                Some(rx) => rx.recv().await,
                None => std::future::pending().await,
            }
        };
        let bg_recv = bg_completion_rx.recv();
        let msg = tokio::select! {
            biased;
            obs = obs_recv => match obs {
                Some(o) => return ReadOutcome::Observation(o),
                None => {
                    obs_rx = None;
                    continue;
                }
            },
            done = bg_recv => match done {
                Some(c) => return ReadOutcome::BackgroundCompletion(c),
                None => continue, // channel closed; ignore
            },
            m = inbox_recv => m,
        };
        match msg {
            None => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => {
                editor.buf.clear();
                editor.cursor = 0;
                term.note("(C-c)");
                emit_editor_state(term.emit, editor, &VecDeque::new());
            }
            Some(AdapterInboxMsg::Input(t)) => {
                term.echo_consumed_line(&t);
                return ReadOutcome::Line(t);
            }
            Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                // PTY bytes only update editor state; nothing is
                // painted inline in chat. The TUI's bottom pane shows
                // the live editor via `EditorState` events.
                let (_discarded, events) = editor.feed_bytes(&bytes);
                for ev in events {
                    match ev {
                        LineEvent::Submit(line) => {
                            term.echo_consumed_line(&line);
                            emit_editor_state(term.emit, editor, &VecDeque::new());
                            return ReadOutcome::Line(line);
                        }
                        LineEvent::Interrupt => {
                            editor.buf.clear();
                            editor.cursor = 0;
                            term.note("(C-c)");
                        }
                        LineEvent::Eof => return ReadOutcome::Eof,
                        LineEvent::DequeueRecall => {
                            // No queue to drain while idle — the
                            // editor just pulled saved content into
                            // `buf` and we'll see the Submit next.
                        }
                    }
                }
                emit_editor_state(term.emit, editor, &VecDeque::new());
            }
            Some(AdapterInboxMsg::PtyResize { cols, .. }) => {
                apply_pty_resize(cols, pty_width);
                editor.set_width(*pty_width);
            }
            Some(AdapterInboxMsg::ToolDecision { .. }) => {}
            Some(AdapterInboxMsg::ToolAction { .. }) => {
                // Tool actions only matter while a tool is running.
                // We're waiting for input, so nothing to do.
            }
        }
    }
}

/// Clear the PTY and re-emit the entire conversation at the new width.
/// Run one tool with approval gating + interrupt support. Mirrors the
/// headless version but renders into the PTY and reads y/n/a from
/// PtyInput when prompting.
#[allow(clippy::too_many_arguments)]
async fn run_one_tool(
    call: &ToolCall,
    registry: &std::sync::Arc<ToolRegistry>,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    term: &Terminal<'_>,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    automode: &mut bool,
    editor: &mut LineEditor,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    bg_completion_tx: crate::tasks::BgCompletionTx,
    bg_after: std::time::Duration,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            // Items model synthesizes the block from these events.
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
        }
    };

    let args_summary = tool.args_summary(&call.input);
    // No inline PTY writes for the tool's header/body/result —
    // the items-model renderer synthesizes the whole block from
    // the structured events emitted below. Leaving the PTY
    // stream as pure chat content means the user's live prompt
    // + typing remain visible during the tool's wait (the
    // previous fence implementation stripped them).
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
    });
    // Task-lifecycle event for the daemon's per-session task
    // registry — surfaces in `/tasks` + MCP `agentd_get_tasks`.
    emit.emit(SessionEvent::TaskStart {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        args_summary: args_summary.clone(),
    });

    let needs_approval = !*automode && matches!(tool.risk(), ToolRisk::Risky);
    if needs_approval {
        term.approval(&call.name, &args_summary, tool.risk());
        emit.emit(SessionEvent::ToolApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_summary: args_summary.clone(),
            risk: tool.risk(),
        });
        let decision = wait_for_approval(inbox, &call.id, automode).await;
        match decision {
            ApprovalOutcome::Stop => return Err("stop".into()),
            ApprovalOutcome::Interrupt => return Err("interrupt".into()),
            ApprovalOutcome::Deny => {
                term.print("n\r\n");
                let msg = "user denied this action".to_string();
                emit.emit(SessionEvent::ToolResult {
                    tool: call.id.clone(),
                    ok: false,
                    output: msg.clone(),
                });
                return Ok(ToolOutcome { ok: false, output: msg });
            }
            ApprovalOutcome::Approve => term.print("y\r\n"),
            ApprovalOutcome::Automode => {
                term.print("a\r\n");
                *automode = true;
            }
        }
    }

    let supervisor_outcome = run_with_supervisor(
        call.id.clone(),
        call.name.clone(),
        args_summary.clone(),
        registry.clone(),
        call.input.clone(),
        tool_ctx,
        inbox,
        editor,
        term,
        queue,
        queued_rows,
        automode,
        tasks,
        bg_completion_tx,
        bg_after,
    )
    .await;
    let outcome: Result<ToolOutcome, String> = match supervisor_outcome {
        crate::tasks::SupervisorOutcome::Done(res) => res,
        crate::tasks::SupervisorOutcome::Killed => Err("interrupt".into()),
        crate::tasks::SupervisorOutcome::Backgrounded => {
            // Synthesize a placeholder result so the LLM's
            // conversation slot is closed; the real result will
            // land as an OBSERVATION when the background watcher
            // reports completion.
            emit.emit(SessionEvent::TaskBackgrounded {
                call_id: call.id.clone(),
            });
            Ok(ToolOutcome {
                ok: true,
                output: crate::tasks::BG_PLACEHOLDER_OUTPUT.to_string(),
            })
        }
    };
    match &outcome {
        Ok(o) => {
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: o.ok,
                output: o.output.clone(),
            });
            // Foreground-completed (non-bg) paths fire TaskEnd
            // here; the backgrounded path fires it later when the
            // BackgroundCompletion arrives via the agent loop.
            if o.output != crate::tasks::BG_PLACEHOLDER_OUTPUT {
                let preview: String = o.output.chars().take(200).collect();
                emit.emit(SessionEvent::TaskEnd {
                    call_id: call.id.clone(),
                    ok: o.ok,
                    output_preview: preview,
                });
            }
        }
        Err(reason) => {
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("({reason})"),
            });
            emit.emit(SessionEvent::TaskEnd {
                call_id: call.id.clone(),
                ok: false,
                output_preview: format!("({reason})"),
            });
        }
    }
    outcome
}

enum ApprovalOutcome {
    Approve,
    Deny,
    Automode,
    Stop,
    Interrupt,
}

async fn wait_for_approval(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    call_id: &str,
    automode: &mut bool,
) -> ApprovalOutcome {
    loop {
        match inbox.recv().await {
            None => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => return ApprovalOutcome::Interrupt,
            Some(AdapterInboxMsg::SetAutoMode(on)) => {
                *automode = on;
                if on {
                    return ApprovalOutcome::Automode;
                }
            }
            Some(AdapterInboxMsg::ToolDecision { call_id: cid, decision })
                if cid == call_id =>
            {
                return match decision.as_str() {
                    "approve" => ApprovalOutcome::Approve,
                    "automode" => {
                        *automode = true;
                        ApprovalOutcome::Automode
                    }
                    _ => ApprovalOutcome::Deny,
                };
            }
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                // Single-key approval from the PTY.
                for b in bytes {
                    match b {
                        b'y' | b'Y' | b'\r' | b'\n' => return ApprovalOutcome::Approve,
                        b'n' | b'N' | 0x1b | 0x07 => return ApprovalOutcome::Deny,
                        b'a' | b'A' => {
                            *automode = true;
                            return ApprovalOutcome::Automode;
                        }
                        0x03 => return ApprovalOutcome::Deny,
                        _ => {}
                    }
                }
            }
            Some(_) => {}
        }
    }
}

/// PTY-rendering counterpart of `agent::run_safe_call`: emits the same
/// `ToolUse` / `ToolResult` events AND the colored inline blocks that
/// interactive mode draws into the PTY.
/// Run a Safe tool call and emit only the structured `ToolUse` /
/// `ToolResult` events — no PTY writes. Used by the parallel-safe
/// block so each task's PTY rendering can be deferred to a
/// sequential pass in original call order. The user sees
/// `→ name` immediately followed by the `✓ result` block for the
/// same call, never interleaved with another in-flight tool.
async fn run_safe_call_silent(
    call: provider::ToolCall,
    registry: &ToolRegistry,
    ctx: &ToolCtx,
    emit: &EventEmitter,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
            });
            let msg = format!("unknown tool: {}", call.name);
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: msg.clone(),
            });
            return Ok(ToolOutcome { ok: false, output: msg });
        }
    };
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
    });
    let args_summary_for_event = tool.args_summary(&call.input);
    emit.emit(SessionEvent::TaskStart {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        args_summary: args_summary_for_event.clone(),
    });
    let outcome = tool
        .run(call.input.clone(), ctx)
        .await
        .map_err(|e| format!("tool error: {e}"));
    match &outcome {
        Ok(o) => {
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: o.ok,
                output: o.output.clone(),
            });
            // The supervisor may auto-bg this call later — only fire
            // TaskEnd for genuinely-foreground completions. The
            // outer agent loop handles TaskEnd for the bg path via
            // BackgroundCompletion.
            let preview: String = o.output.chars().take(200).collect();
            emit.emit(SessionEvent::TaskEnd {
                call_id: call.id.clone(),
                ok: o.ok,
                output_preview: preview,
            });
        }
        Err(reason) => {
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("({reason})"),
            });
            emit.emit(SessionEvent::TaskEnd {
                call_id: call.id.clone(),
                ok: false,
                output_preview: format!("({reason})"),
            });
        }
    }
    outcome
}


/// Run a tool through the per-session supervisor. The supervisor
/// spawns the actual `tool.run` on its own task, races the join
/// handle against the auto-bg timer + control-channel signals, and
/// returns a [`SupervisorOutcome`] this caller maps back to a
/// `Result<ToolOutcome, String>` for the agent loop.
///
/// While the tool runs, this caller still drains the inbox via
/// `drive_with_input` so user typing queues + Ctrl-C / explicit
/// `ToolAction` messages flow through. The supervisor's control
/// channel is what actually drives kill/background; this function
/// just relays inbox events into it.
#[allow(clippy::too_many_arguments)]
async fn run_with_supervisor(
    call_id: String,
    tool_name: String,
    args_summary: String,
    registry: std::sync::Arc<ToolRegistry>,
    input: serde_json::Value,
    ctx: &ToolCtx,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    automode: &mut bool,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    bg_completion_tx: crate::tasks::BgCompletionTx,
    bg_after: std::time::Duration,
) -> crate::tasks::SupervisorOutcome {
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let client_seed = ctx.client.get().cloned();
    let tool_name_for_runner = tool_name.clone();
    let tool_runner = async move {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
        };
        if let Some(c) = client_seed {
            let _ = local_ctx.client.set(c);
        }
        let tool = match registry.get(&tool_name_for_runner) {
            Some(t) => t,
            None => {
                return Err(format!("tool went away: {tool_name_for_runner}"));
            }
        };
        tool.run(input, &local_ctx)
            .await
            .map_err(|e| format!("tool error: {e}"))
    };

    // Race the supervisor (which owns the spawned tool task) against
    // the inbox drain so user typing during a long tool run still
    // queues. Inbox `ToolAction` events get forwarded to the
    // supervisor's control channel; everything else flows through
    // the editor / queue / automode handlers like before.
    let tasks_for_drive = tasks.clone();
    let mut supervisor = tokio::spawn(crate::tasks::supervise(
        call_id.clone(),
        tool_name.clone(),
        args_summary.clone(),
        tasks.clone(),
        bg_completion_tx,
        bg_after,
        tool_runner,
    ));
    let drive = drive_with_input_relaying(
        inbox,
        editor,
        term,
        queue,
        queued_rows,
        automode,
        tasks_for_drive,
        &call_id,
        async { (&mut supervisor).await },
    )
    .await;
    match drive {
        DriveExit::Done(outcome) => join_supervisor_outcome(outcome),
        DriveExit::Stop | DriveExit::Channel => {
            kill_supervisor_task(&tasks, &call_id, supervisor).await
        }
        DriveExit::Interrupt => kill_supervisor_task(&tasks, &call_id, supervisor).await,
    }
}

fn outcome_from_supervisor(
    outcome: crate::tasks::SupervisorOutcome,
) -> std::result::Result<ToolOutcome, String> {
    match outcome {
        crate::tasks::SupervisorOutcome::Done(r) => r,
        crate::tasks::SupervisorOutcome::Killed => Err("interrupt".into()),
        crate::tasks::SupervisorOutcome::Backgrounded => Ok(ToolOutcome {
            ok: true,
            output: crate::tasks::BG_PLACEHOLDER_OUTPUT.to_string(),
        }),
    }
}

fn join_supervisor_outcome(
    outcome: std::result::Result<
        crate::tasks::SupervisorOutcome,
        tokio::task::JoinError,
    >,
) -> crate::tasks::SupervisorOutcome {
    match outcome {
        Ok(outcome) => outcome,
        Err(e) if e.is_cancelled() => crate::tasks::SupervisorOutcome::Killed,
        Err(e) => crate::tasks::SupervisorOutcome::Done(Err(format!("join error: {e}"))),
    }
}

async fn kill_supervisor_task(
    tasks: &crate::tasks::Tasks,
    call_id: &str,
    handle: tokio::task::JoinHandle<crate::tasks::SupervisorOutcome>,
) -> crate::tasks::SupervisorOutcome {
    let signaled =
        crate::tasks::forward_control(tasks, call_id, crate::tasks::ToolControl::Kill).await;
    if !signaled && !handle.is_finished() {
        handle.abort();
    }
    join_supervisor_outcome(handle.await)
}

async fn kill_joined_task<T>(
    tasks: &crate::tasks::Tasks,
    call_id: &str,
    handle: tokio::task::JoinHandle<T>,
) {
    let signaled =
        crate::tasks::forward_control(tasks, call_id, crate::tasks::ToolControl::Kill).await;
    if !signaled && !handle.is_finished() {
        handle.abort();
    }
    let _ = handle.await;
}

/// Variant of `drive_with_input` that *also* relays
/// `AdapterInboxMsg::ToolAction` events to a specific call_id's
/// supervisor via its control channel. Used by
/// `run_with_supervisor` so kill/bg buttons on the live tool block
/// actually take effect. Other inbox handling is identical to the
/// regular `drive_with_input`.
#[allow(clippy::too_many_arguments)]
async fn drive_with_input_relaying<F>(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    automode: &mut bool,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    this_call_id: &str,
    fut: F,
) -> DriveExit<F::Output>
where
    F: std::future::Future,
{
    // Editor state lives in the TUI's fixed bottom pane now, fed by
    // `EditorState` events. No PTY-side prompt painting here — the
    // PTY scrollback is reserved for chat + tool blocks.
    let _ = queued_rows; // retained for source-compat with callers
    emit_editor_state(term.emit, editor, queue);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            res = &mut fut => return DriveExit::Done(res),
            msg = inbox.recv() => {
                match msg {
                    None => return DriveExit::Channel,
                    Some(AdapterInboxMsg::Stop) => return DriveExit::Stop,
                    Some(AdapterInboxMsg::Interrupt) => return DriveExit::Interrupt,
                    Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
                    Some(AdapterInboxMsg::Input(t)) => {
                        enqueue_line(queue, editor, t);
                        emit_editor_state(term.emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyInput(bytes)) => {
                        if pty_input_requests_interrupt(&bytes) {
                            return DriveExit::Interrupt;
                        }
                        let (_discarded, events) = editor.feed_bytes(&bytes);
                        for ev in events {
                            match ev {
                                LineEvent::Submit(line) => {
                                    enqueue_line(queue, editor, line);
                                }
                                LineEvent::Interrupt => return DriveExit::Interrupt,
                                LineEvent::Eof => {}
                                LineEvent::DequeueRecall => {
                                    // Editor's buf has the recalled
                                    // combined text; drop the queue
                                    // so it doesn't double-run.
                                    queue.clear();
                                }
                            }
                        }
                        emit_editor_state(term.emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyResize { .. }) => {}
                    Some(AdapterInboxMsg::ToolDecision { .. }) => {}
                    Some(AdapterInboxMsg::ToolAction { call_id, action }) => {
                        // Forward to whichever supervisor matches —
                        // not strictly limited to this_call_id, since
                        // parallel-safe tools each have their own.
                        let control = match action.as_str() {
                            "kill" => Some(crate::tasks::ToolControl::Kill),
                            "background" => Some(crate::tasks::ToolControl::Background),
                            _ => None,
                        };
                        if let Some(c) = control {
                            let _ = crate::tasks::forward_control(&tasks, &call_id, c)
                                .await;
                        }
                        let _ = this_call_id; // suppress unused-var lint
                    }
                }
            }
        }
    }
}


/// Parse a `^/word( args)?$` line into `(name, args)`. Returns `None`
/// for anything that doesn't look like a client slash command — empty
/// input, lines that don't start with `/`, or `/` followed by a
/// non-word character (so things like `/2` or `/!` flow through to the
/// LLM as natural-language). The name comes out lowercased so the TUI
/// dispatch table doesn't have to be case-aware.
/// Inline parser for the `/loop` slash spec — same shape as the
/// daemon-side `loops::parse_slash_spec` (duplicated to avoid a
/// daemon ↔ adapter coupling for this small helper). Recognized
/// tokens: `every? <num><unit>`, optional `for <num><unit>` tail.
fn parse_slash_loop_spec(
    input: &str,
    now_ms: i64,
) -> Option<(agentd_protocol::LoopSpec, Option<i64>, String)> {
    let mut tokens = input.split_whitespace().peekable();
    if matches!(tokens.peek().copied(), Some("every")) {
        tokens.next();
    }
    let first = tokens.peek().copied()?;
    let secs = parse_duration_secs(first)?;
    tokens.next();
    let mut expires_at_ms: Option<i64> = None;
    if matches!(tokens.peek().copied(), Some("for")) {
        tokens.next();
        if let Some(t) = tokens.peek().copied() {
            if let Some(d) = parse_duration_secs(t) {
                tokens.next();
                expires_at_ms = Some(now_ms + (d as i64) * 1000);
            }
        }
    }
    let rest: Vec<&str> = tokens.collect();
    let prompt = rest.join(" ");
    Some((
        agentd_protocol::LoopSpec::Interval { seconds: secs },
        expires_at_ms,
        prompt,
    ))
}

fn parse_duration_secs(tok: &str) -> Option<u64> {
    let split_at = tok.find(|c: char| !c.is_ascii_digit())?;
    if split_at == 0 {
        return None;
    }
    let (num_s, unit_s) = tok.split_at(split_at);
    let num: u64 = num_s.parse().ok()?;
    let mult: u64 = match unit_s.to_ascii_lowercase().as_str() {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86400,
        _ => return None,
    };
    num.checked_mul(mult)
}

/// Handle a `/loop ...` slash command. Parses any explicit
/// interval / `for <duration>` tokens; for prompts with no
/// interval, asks the model to suggest one. Calls the
/// `agentd_loop_create` tool path via the IPC client so the loop
/// is persisted by the daemon's scheduler.
async fn handle_slash_loop(
    rest: &str,
    session_id: &str,
    emit: &EventEmitter,
    term: &Terminal<'_>,
    provider: &dyn crate::provider::LlmProvider,
    model: &str,
    tool_ctx: &ToolCtx,
) {
    use agentd_protocol::{LoopCreateParams, LoopSpec};
    let rest = rest.trim();
    if rest.is_empty() {
        term.note("(usage: /loop [interval] [for <duration>] <prompt>)");
        return;
    }
    let now_ms = chrono::Utc::now().timestamp_millis();

    // Try the inline parser first. On miss, ask the LLM for an
    // interval seeded by the user's prompt.
    let (spec, expires_at_ms, prompt, suggested) = match parse_slash_loop_spec(rest, now_ms) {
        Some((s, e, p)) => (s, e, p, false),
        None => {
            term.note("(no interval given — asking the model to pick one…)");
            let secs = match crate::interval_suggest::suggest(provider, model, rest).await {
                Ok(s) => s,
                Err(e) => {
                    term.note(&format!("(interval suggest failed: {e})"));
                    return;
                }
            };
            (
                LoopSpec::Interval { seconds: secs },
                None,
                rest.to_string(),
                true,
            )
        }
    };

    // Clamp + persist via the daemon. The daemon enforces the
    // same bounds, but clamping here lets us include "(clamped)"
    // in the user-visible note.
    let LoopSpec::Interval { seconds } = spec;
    let (clamped, was_clamped) = clamp_interval_for_slash(seconds);
    let spec = LoopSpec::Interval { seconds: clamped };

    if prompt.trim().is_empty() {
        term.note("(no prompt — usage: /loop [interval] <prompt>)");
        return;
    }

    let client = match crate::tools::agentd::client(tool_ctx).await {
        Ok(c) => c,
        Err(e) => {
            term.note(&format!("(daemon connect failed: {e})"));
            return;
        }
    };
    let loop_obj = match client
        .loop_create(LoopCreateParams {
            session_id: session_id.to_string(),
            spec,
            prompt: prompt.clone(),
            expires_at_ms,
        })
        .await
    {
        Ok(l) => l,
        Err(e) => {
            term.note(&format!("(loop create failed: {e})"));
            return;
        }
    };

    // Emit the equivalent ToolUse + ToolResult so the transcript
    // shows the action — matches the path the LLM would take if
    // it called `agentd_loop_create` directly.
    let call_id = format!("slash-loop-{}", loop_obj.id);
    let args = serde_json::json!({
        "session_id": loop_obj.session_id,
        "interval_seconds": clamped,
        "prompt": loop_obj.prompt,
    });
    emit.emit(SessionEvent::ToolUse {
        tool: "agentd_loop_create".to_string(),
        args,
    });
    emit.emit(SessionEvent::ToolResult {
        tool: call_id,
        ok: true,
        output: serde_json::to_string(&loop_obj).unwrap_or_default(),
    });

    let note = format!(
        "(loop {} every {}s — \"{}\"{}{})",
        loop_obj.id.chars().take(10).collect::<String>(),
        clamped,
        prompt.chars().take(60).collect::<String>(),
        if suggested { " — interval suggested" } else { "" },
        if was_clamped { " — clamped to bounds" } else { "" },
    );
    term.note(&note);
}

/// Read the bounds the daemon's loop module uses for clamping.
/// Duplicated from `daemon::loops::clamp_interval` because the
/// adapter doesn't share that module — but the env-var keys
/// match so a deployment-time override applies to both sides.
fn clamp_interval_for_slash(secs: u64) -> (u64, bool) {
    let min = std::env::var("AGENTD_LOOP_MIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);
    let max = std::env::var("AGENTD_LOOP_MAX_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(24 * 3600u64);
    if secs < min {
        (min, true)
    } else if secs > max {
        (max, true)
    } else {
        (secs, false)
    }
}

fn parse_slash_command(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.strip_prefix('/')?;
    let (name, args) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    let args = if args.is_empty() { None } else { Some(args.to_string()) };
    Some((name.to_lowercase(), args))
}

/// Push a user-typed (or programmatically-sent) line onto the
/// pending-input queue. Trimmed-empty lines are dropped silently —
/// they'd just produce a blank turn at the next AwaitingInput.
///
/// Successive enqueues coalesce into a single queue entry joined by
/// newlines so that the LLM receives one well-structured user message
/// with the original line breaks intact, and up-arrow can pull the
/// whole batch back into the editor as a single multi-line prompt.
fn enqueue_line(
    queue: &mut VecDeque<String>,
    editor: &mut LineEditor,
    line: String,
) {
    if line.trim().is_empty() {
        return;
    }
    if let Some(existing) = queue.back_mut() {
        existing.push('\n');
        existing.push_str(&line);
        let combined = existing.clone();
        editor.set_queued_recall(Some(combined));
    } else {
        queue.push_back(line.clone());
        editor.set_queued_recall(Some(line));
    }
}

/// Outcome of [`drive_with_input`]: either the wrapped future completed,
/// or the inbox produced a control event that should unwind the agent's
/// current turn.
enum DriveExit<T> {
    Done(T),
    Stop,
    Channel,
    Interrupt,
}

/// Run `fut` concurrently with an inbox-drain task so the user can keep
/// typing — and queue submissions — while the agent is mid-turn.
///
/// Returns [`DriveExit::Done`] when `fut` completes first, or one of the
/// control variants when an inbox message demands the turn unwind
/// (Stop / channel closed / Ctrl-C / `Interrupt` inbox message).
///
/// PTY bytes are fed to `editor` *silently* — the editor's redraw output
/// is discarded so it can't clobber whatever the streaming agent is
/// writing to the same PTY. Submit events (and any programmatic
/// `AdapterInboxMsg::Input` arriving during the turn) are pushed onto
/// `queue` with an inline `↳ queued: ...` marker for visual feedback.
#[allow(clippy::too_many_arguments)]
async fn drive_with_input<F>(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    automode: &mut bool,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    fut: F,
) -> DriveExit<F::Output>
where
    F: std::future::Future,
{
    // Editor lives in the TUI's fixed bottom pane; emit state changes
    // as `EditorState` events instead of painting the PTY scrollback.
    let _ = queued_rows; // retained for source-compat with callers
    emit_editor_state(term.emit, editor, queue);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            res = &mut fut => return DriveExit::Done(res),
            msg = inbox.recv() => {
                match msg {
                    None => return DriveExit::Channel,
                    Some(AdapterInboxMsg::Stop) => return DriveExit::Stop,
                    Some(AdapterInboxMsg::Interrupt) => return DriveExit::Interrupt,
                    Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
                    Some(AdapterInboxMsg::Input(t)) => {
                        enqueue_line(queue, editor, t);
                        emit_editor_state(term.emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyInput(bytes)) => {
                        if pty_input_requests_interrupt(&bytes) {
                            return DriveExit::Interrupt;
                        }
                        let (_discarded, events) = editor.feed_bytes(&bytes);
                        for ev in events {
                            match ev {
                                LineEvent::Submit(line) => {
                                    enqueue_line(queue, editor, line);
                                }
                                LineEvent::Interrupt => {
                                    return DriveExit::Interrupt;
                                }
                                LineEvent::Eof => {}
                                LineEvent::DequeueRecall => {
                                    queue.clear();
                                }
                            }
                        }
                        emit_editor_state(term.emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyResize { .. }) => {}
                    Some(AdapterInboxMsg::ToolDecision { .. }) => {}
                    Some(AdapterInboxMsg::ToolAction { call_id, action }) => {
                        // Forward to whichever supervisor matches.
                        // No-op if call_id doesn't exist (already
                        // completed, never existed, parallel race).
                        let control = match action.as_str() {
                            "kill" => Some(crate::tasks::ToolControl::Kill),
                            "background" => Some(crate::tasks::ToolControl::Background),
                            _ => None,
                        };
                        if let Some(c) = control {
                            let _ = crate::tasks::forward_control(&tasks, &call_id, c)
                                .await;
                        }
                    }
                }
            }
        }
    }
}

/// Variant of [`drive_with_input`] that keeps the user's typing
/// invisible to the PTY. Used while the agent is streaming its
/// response — relaying editor bytes to the same PTY would interleave
/// with the agent's writes and clobber both. Submissions still flow
/// into `queue` (and the editor's `queued_recall`), so the work the
/// user composes during streaming is preserved for the next turn or
/// for an up-arrow recall once a tool drive takes over.
#[allow(clippy::too_many_arguments)]
async fn drive_with_input_silent<F>(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    queue: &mut VecDeque<String>,
    automode: &mut bool,
    emit: &EventEmitter,
    _tasks: std::sync::Arc<crate::tasks::Tasks>,
    fut: F,
) -> DriveExit<F::Output>
where
    F: std::future::Future,
{
    // The agent owns the PTY for the duration of this future; the
    // TUI's fixed editor pane is what users see, fed by
    // `EditorState` events.
    emit_editor_state(emit, editor, queue);
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            res = &mut fut => return DriveExit::Done(res),
            msg = inbox.recv() => {
                match msg {
                    None => return DriveExit::Channel,
                    Some(AdapterInboxMsg::Stop) => return DriveExit::Stop,
                    Some(AdapterInboxMsg::Interrupt) => return DriveExit::Interrupt,
                    Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
                    Some(AdapterInboxMsg::Input(t)) => {
                        enqueue_line(queue, editor, t);
                        emit_editor_state(emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyInput(bytes)) => {
                        if pty_input_requests_interrupt(&bytes) {
                            return DriveExit::Interrupt;
                        }
                        let (_discarded, events) = editor.feed_bytes(&bytes);
                        for ev in events {
                            match ev {
                                LineEvent::Submit(line) => {
                                    enqueue_line(queue, editor, line);
                                }
                                LineEvent::Interrupt => {
                                    return DriveExit::Interrupt;
                                }
                                LineEvent::Eof => {}
                                LineEvent::DequeueRecall => {
                                    queue.clear();
                                }
                            }
                        }
                        emit_editor_state(emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyResize { .. }) => {}
                    Some(AdapterInboxMsg::ToolDecision { .. }) => {}
                    Some(AdapterInboxMsg::ToolAction { .. }) => {
                        // No active tool while the agent is streaming.
                    }
                }
            }
        }
    }
}
