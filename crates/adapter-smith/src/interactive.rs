//! Interactive (PTY) mode for smith.
//!
//! Smith doesn't spawn a child — there's no CLI to attach a real PTY
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
use agentd_protocol::{ApprovalMode, SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    fn banner(&self, provider: &str, model: &str, mode: ApprovalMode) {
        let mode_badge = match mode.badge() {
            Some(b) => format!("  [{b}]"),
            None => String::new(),
        };
        let banner = format!(
            "\r\n\x1b[1;35msmith\x1b[0m  \x1b[2m{provider}:{model}\x1b[0m{mode_badge}\r\n",
        );
        self.write(banner.as_bytes());
    }
    fn tool_use(&self, name: &str, args_summary: &str) {
        let line = format!("\r\n\x1b[1;32m→ {name}\x1b[0m\x1b[2m({args_summary})\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
    fn tool_result(&self, ok: bool, output: &str) {
        let glyph = if ok {
            "\x1b[1;32m✓\x1b[0m"
        } else {
            "\x1b[1;31m✗\x1b[0m"
        };
        // Print a short single-line preview of the result; full content
        // is in the transcript (we also emit ToolResult).
        let one_line: String = output
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect();
        let line = format!("  {glyph}  \x1b[2m{one_line}\x1b[0m\r\n");
        self.write(line.as_bytes());
    }
    /// Open a tool-block region in the PTY stream with a custom OSC
    /// marker. Ratatui clients use this as a fence: the bytes between
    /// the open and matching close are smith's truncated rendering,
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
                let payload = format!("  {glyph}  \x1b[2m{trimmed}\x1b[0m\r\n");
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
            let footer =
                format!("     \x1b[2;36m[+{remaining} lines — click to expand]\x1b[0m\r\n");
            self.write(footer.as_bytes());
        }
    }
    fn approval(&self, tool: &str, args_summary: &str, risk: ToolRisk, allow_auto_review: bool) {
        let risk_label = match risk {
            ToolRisk::Safe => "safe",
            ToolRisk::Risky => "risky",
        };
        let auto_review_option = if allow_auto_review {
            " / \x1b[1m[a]\x1b[0mauto-review"
        } else {
            ""
        };
        let line = format!(
            "\r\n\x1b[1;33m? approve [{risk_label}]\x1b[0m {tool}\x1b[2m({args_summary})\x1b[0m\
             — \x1b[1m[y]\x1b[0mapprove / \x1b[1m[n]\x1b[0mdeny{auto_review_option} / \x1b[1m[f]\x1b[0munsafe-auto: "
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

fn emit_agent_status(emit: &EventEmitter, started_at_ms: i64, status: &str) {
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
fn emit_editor_state(emit: &EventEmitter, editor: &LineEditor, queue: &VecDeque<String>) {
    emit.emit(SessionEvent::EditorState {
        queued: queue.iter().cloned().collect(),
        buf: editor.buf.clone(),
        cursor: editor.cursor,
        completions: editor.slash_matches(),
    });
}

/// Lines whose start matches one of these labels are dimmed in the PTY
/// (the structured Message event still carries the raw text — this is
/// purely a rendering tweak). Cheap to extend; keep the entries short
/// so the at-start-of-line buffer stays tiny.
const DIM_LINE_PREFIXES: &[&str] = &["Summary:"];

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
/// First-line marker for streaming reasoning / "thinking" text from
/// providers that emit it (Anthropic extended thinking, Codex
/// Responses reasoning summaries). `\xc2\xb7` = `·`. Visually
/// smaller and dimmer than the response bullet so it reads as
/// secondary context, not the agent's actual answer.
const REASONING_BULLET: &[u8] = b"\xc2\xb7 ";
/// SGR escape that opens the dim+italic styling we paint reasoning
/// text in. Matches `REASONING_RESET` below — every newline inside
/// the reasoning block has to bracket the line break with reset
/// then re-enter, otherwise some terminals leak the attribute onto
/// the next blank line.
const REASONING_OPEN_SGR: &[u8] = b"\x1b[2;3m";
/// Matching SGR escape that closes the reasoning styling so the
/// regular response block isn't accidentally dim-italic.
const REASONING_RESET_SGR: &[u8] = b"\x1b[0m";
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
    /// When true, deltas render into the session PTY. Off for quiet
    /// ambient Operator ticks where the model should update widgets or
    /// say `noted` without adding visible minibuffer chatter.
    emit_pty: bool,
    /// When true, every delta also fires a `SessionEvent::Message` so
    /// the daemon's transcript view sees the streaming text. Off for
    /// replay paths where the message is already in the transcript.
    emit_messages: bool,
    /// Start time of the current live turn, used for `AgentStatus`.
    status_started_at_ms: i64,
    /// True while a reasoning sub-block is currently open. Streaming
    /// reasoning deltas keep appending to it; the first regular text
    /// delta (or `finalize`) closes it before the response block
    /// opens.
    in_reasoning: bool,
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
            emit_pty: true,
            emit_messages: true,
            status_started_at_ms,
            in_reasoning: false,
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

    /// Open a reasoning sub-block: top padding (only if nothing has
    /// rendered yet), bullet, dim+italic SGR. Bracketed by
    /// `close_reasoning_block` before any regular response text or
    /// finalize so the styling doesn't leak.
    fn open_reasoning_block(&mut self, out: &mut Vec<u8>) {
        if !self.emitted {
            for _ in 0..PAD_TOP {
                out.extend_from_slice(b"\r\n");
            }
            self.emitted = true;
        }
        out.push(b'\r');
        out.extend_from_slice(REASONING_OPEN_SGR);
        out.extend_from_slice(REASONING_BULLET);
        self.col = 0;
        self.at_line_start = true;
        self.in_reasoning = true;
    }

    fn reasoning_newline(&mut self, out: &mut Vec<u8>) {
        // Reset SGR over the line break to keep the next blank line
        // from inheriting the dim+italic on some terminals, then
        // re-enter the styling on the continuation line.
        out.extend_from_slice(REASONING_RESET_SGR);
        out.extend_from_slice(b"\r\n");
        out.extend_from_slice(REASONING_OPEN_SGR);
        out.extend_from_slice(REASONING_BULLET);
        self.col = 0;
        self.at_line_start = true;
    }

    fn close_reasoning_block(&mut self, out: &mut Vec<u8>) {
        if !self.in_reasoning {
            return;
        }
        out.extend_from_slice(REASONING_RESET_SGR);
        out.extend_from_slice(b"\r\n");
        self.in_reasoning = false;
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
        if self.in_reasoning {
            self.close_reasoning_block(&mut out);
        }
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
        if self.emit_pty && !out.is_empty() {
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
        // Reasoning ran into the answer — drop the SGR + newline
        // separator before the response bullet opens.
        if self.in_reasoning {
            self.close_reasoning_block(&mut out);
        }
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
        if self.emit_pty && !out.is_empty() {
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

    /// Streaming reasoning text — open a dim+italic sub-block on
    /// first call, emit each char with simple soft-wrap. The dim
    /// styling and bullet visually separate the reasoning trace
    /// from the actual response, which lands later via `delta` (and
    /// `delta` automatically closes the reasoning block first).
    fn reasoning_delta(&mut self, text: &str) {
        if self.emit_messages {
            emit_agent_status(self.emit, self.status_started_at_ms, "Thinking");
        }
        let mut out: Vec<u8> = Vec::with_capacity(text.len() + 32);
        if !self.in_reasoning {
            self.open_reasoning_block(&mut out);
        }
        for c in text.chars() {
            if c == '\n' {
                self.reasoning_newline(&mut out);
                continue;
            }
            if !self.at_line_start && self.col >= self.usable_width() {
                // Soft-wrap inside the reasoning block — same gutter
                // as `delta` so right-margin alignment matches.
                self.reasoning_newline(&mut out);
            }
            self.at_line_start = false;
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            out.extend_from_slice(s.as_bytes());
            self.col += 1;
        }
        if self.emit_pty && !out.is_empty() {
            self.emit.emit(SessionEvent::pty(&out));
        }
        if self.emit_messages {
            self.emit.emit(SessionEvent::Reasoning {
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
    Csi { params: String },
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
    /// Empty when the buf doesn't start with `/`. Candidates come from the
    /// shared registry's popup set, sorted for a deterministic ghost order.
    fn slash_matches(&self) -> Vec<String> {
        if !self.buf.starts_with('/') {
            return Vec::new();
        }
        let mut matches = agentd_protocol::slash::model_completion_matches(&self.buf);
        if matches.is_empty() {
            matches = agentd_protocol::slash::popup_names()
                .filter(|c| c.starts_with(self.buf.as_str()))
                .map(str::to_string)
                .collect();
        }
        matches.sort_unstable();
        matches
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
        let match_refs: Vec<&str> = matches.iter().map(String::as_str).collect();
        let prefix = common_prefix(&match_refs);
        if prefix.chars().count() > self.buf.chars().count() {
            self.buf = prefix;
            if matches.len() == 1 && self.buf == matches[0].as_str() {
                self.buf.push(' ');
            }
            self.cursor = self.buf.chars().count();
            return true;
        }
        if matches.len() == 1 && self.buf == matches[0].as_str() {
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
        if !line.is_empty() && self.history.last().map(|s| s.as_str()) != Some(line.as_str()) {
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
        self.width.saturating_sub(self.prompt_visible_width).max(1)
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
                        self.esc = EscState::Csi {
                            params: String::new(),
                        };
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

    fn handle_ss3_final(&mut self, final_byte: u8, out: &mut Vec<u8>, events: &mut Vec<LineEvent>) {
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
            rows.push(VisualEditorRow { start, end: idx });
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

fn pty_input_requests_background(bytes: &[u8]) -> bool {
    bytes == [0x02]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> LineEditor {
        LineEditor::new(b"> ", 2)
    }

    #[test]
    fn default_monitor_spec_picks_cheap_same_provider_tier() {
        // Frontier operator models → a cheaper same-provider model.
        assert_eq!(
            default_monitor_spec("codex-oauth", "gpt-5.5").as_deref(),
            Some("codex-oauth:gpt-5.4-mini")
        );
        assert_eq!(
            default_monitor_spec("openai", "gpt-5.5").as_deref(),
            Some("openai:gpt-5-mini")
        );
        assert_eq!(
            default_monitor_spec("anthropic", "claude-opus-4-8").as_deref(),
            Some("anthropic:claude-sonnet-4-5")
        );
        // Already-small operator models → keep them (no downgrade).
        assert_eq!(default_monitor_spec("openai", "gpt-5-mini"), None);
        assert_eq!(default_monitor_spec("anthropic", "claude-haiku-4-5"), None);
        assert_eq!(default_monitor_spec("anthropic", "claude-sonnet-4-5"), None);
        // No confident cheap default → keep the operator's model.
        assert_eq!(default_monitor_spec("ollama", "llama3"), None);
        assert_eq!(default_monitor_spec("gemini", "gemini-pro"), None);
    }

    #[test]
    fn observation_panel_echo_shows_trigger_without_boilerplate() {
        // Real user input echoes itself — no synthetic echo.
        assert!(observation_panel_echo("hello operator").is_none());
        // Fleet event: shown (dim), with the internal marker stripped.
        let s = String::from_utf8(
            observation_panel_echo("OBSERVATION: session ab12 entered errored (boom)").unwrap(),
        )
        .unwrap();
        assert!(s.contains("session ab12 entered errored (boom)"), "{s}");
        assert!(!s.contains("OBSERVATION:"), "marker not stripped:\n{s}");
        // Ambient monitor: boilerplate stripped, findings kept under a label.
        let obs = "OBSERVATION: ambient fleet monitor flagged the following. Decide whether to \
                   surface it to the user or update an Operator widget; reply exactly `noted` if \
                   it's not worth it.\ns277 \"x\": blocked at prompt";
        let s = String::from_utf8(observation_panel_echo(obs).unwrap()).unwrap();
        assert!(s.contains("ambient monitor flagged:"), "{s}");
        assert!(s.contains("s277 \"x\": blocked at prompt"), "{s}");
        assert!(
            !s.contains("Decide whether to surface"),
            "boilerplate kept:\n{s}"
        );
    }

    #[test]
    fn parse_triage_finding_filters_nothing() {
        assert!(parse_triage_finding("nothing").is_none());
        assert!(parse_triage_finding("  Nothing.  ").is_none());
        assert!(parse_triage_finding("").is_none());
        let f = parse_triage_finding("s123 \"dogfood\": stuck on a trust prompt").unwrap();
        assert!(f.contains("ambient fleet monitor flagged"), "{f}");
        assert!(f.contains("dogfood") && f.contains("trust prompt"), "{f}");
    }

    #[test]
    fn truncate_keep_tail_keeps_recent_drops_older() {
        assert_eq!(truncate_keep_tail("short", 100), "short");
        let long = "0123456789".repeat(20); // 200 bytes
        let out = truncate_keep_tail(&long, 50);
        assert!(out.starts_with("…(older truncated) "), "{out}");
        assert!(out.ends_with("0123456789"), "{out}"); // kept the most-recent tail
        assert!(out.len() < 90, "len {}", out.len());
    }

    #[test]
    fn ambient_snapshot_counts_idle_deltas_and_preview_priority() {
        let now = chrono::Utc::now();
        let stale = now.timestamp_millis() - 15 * 60_000; // quiet 15m → idle
        let fresh = now.timestamp_millis();
        fn summary(
            id: &str,
            state: &str,
            last_pty_ms: Option<i64>,
        ) -> agentd_protocol::SessionSummary {
            let mut v = serde_json::json!({
                "id": id, "harness": "claude", "cwd": "/x",
                "state": state, "created_at": "2026-06-06T00:00:00Z"
            });
            if let Some(ms) = last_pty_ms {
                v["last_pty_at_ms"] = ms.into();
            }
            serde_json::from_value(v).unwrap()
        }
        let mut prev = HashMap::new();

        // First tick: own session excluded; a running session quiet ≥10m is
        // flagged idle while a freshly-active one is not; baseline (no delta).
        let sessions = vec![
            summary("self0000", "running", Some(fresh)),
            summary("aaaa1111", "running", Some(stale)), // idle
            summary("bbbb2222", "running", Some(fresh)), // busy
            summary("cccc3333", "errored", None),
        ];
        let snap = compute_ambient_snapshot(&sessions, "self0000", &mut prev, now);
        assert!(
            snap.summary.contains("2 running (1 idle"),
            "{}",
            snap.summary
        );
        assert!(snap.summary.contains("1 errored"), "{}", snap.summary);
        assert!(
            snap.summary.contains("aaaa1111") && snap.summary.contains("quiet"),
            "{}",
            snap.summary
        );
        assert!(snap.summary.contains("baseline only"), "{}", snap.summary);
        // Preview priority: errored first, then idle; recently-active sessions
        // (bbbb2222) are also previewable so the Operator sees live activity.
        assert_eq!(snap.preview_targets[0].0, "cccc3333");
        assert!(snap.preview_targets.iter().any(|(id, _)| id == "aaaa1111"));
        assert!(snap.preview_targets.iter().any(|(id, _)| id == "bbbb2222"));

        // Second tick: aaaa1111 → done surfaces as a delta.
        let sessions2 = vec![
            summary("self0000", "running", Some(fresh)),
            summary("aaaa1111", "done", None),
            summary("bbbb2222", "running", Some(fresh)),
            summary("cccc3333", "errored", None),
        ];
        let snap2 = compute_ambient_snapshot(&sessions2, "self0000", &mut prev, now);
        assert!(
            snap2.summary.contains("aaaa1111") && snap2.summary.contains("→ done"),
            "{}",
            snap2.summary
        );
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

    #[test]
    fn drain_steering_is_none_when_no_input_queued() {
        let mut q: VecDeque<String> = VecDeque::new();
        assert_eq!(drain_steering(&mut q), None);
    }

    #[test]
    fn drain_steering_joins_entries_and_empties_the_queue() {
        let mut q: VecDeque<String> = VecDeque::new();
        q.push_back("first".to_string());
        q.push_back("second".to_string());
        assert_eq!(drain_steering(&mut q).as_deref(), Some("first\nsecond"));
        assert!(q.is_empty(), "queue must be emptied after draining");
        assert_eq!(drain_steering(&mut q), None, "second drain finds nothing");
    }

    #[test]
    fn mid_turn_submissions_drain_as_a_single_steering_message() {
        // `enqueue_line` coalesces consecutive submits into one entry, so a
        // step-boundary drain yields exactly the combined text the user typed
        // while the agent was working.
        let mut q: VecDeque<String> = VecDeque::new();
        let mut ed = editor();
        enqueue_line(&mut q, &mut ed, "focus on the parser".to_string());
        enqueue_line(&mut q, &mut ed, "skip the docs for now".to_string());
        enqueue_line(&mut q, &mut ed, "   ".to_string()); // whitespace ignored
        assert_eq!(
            drain_steering(&mut q).as_deref(),
            Some("focus on the parser\nskip the docs for now")
        );
        assert!(q.is_empty());
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

    #[tokio::test]
    async fn wait_for_approval_maps_prompt_keys_to_new_modes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(agentd_protocol::adapter::AdapterInboxMsg::PtyInput(vec![
            b'a',
        ]))
        .await
        .unwrap();
        let mut mode = ApprovalMode::Manual;
        assert_eq!(
            wait_for_approval(&mut rx, "call-1", &mut mode).await,
            ApprovalOutcome::AutoReview
        );
        // `a` switches the session into auto-review mode, which persists.
        assert_eq!(mode, ApprovalMode::AutoReview);

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(agentd_protocol::adapter::AdapterInboxMsg::PtyInput(vec![
            b'f',
        ]))
        .await
        .unwrap();
        let mut mode = ApprovalMode::Manual;
        assert_eq!(
            wait_for_approval(&mut rx, "call-1", &mut mode).await,
            ApprovalOutcome::UnsafeAuto
        );
        assert_eq!(mode, ApprovalMode::UnsafeAuto);
    }

    #[tokio::test]
    async fn wait_for_approval_maps_decisions_to_new_modes() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(agentd_protocol::adapter::AdapterInboxMsg::ToolDecision {
            call_id: "call-1".into(),
            decision: "auto_review".into(),
        })
        .await
        .unwrap();
        let mut mode = ApprovalMode::Manual;
        assert_eq!(
            wait_for_approval(&mut rx, "call-1", &mut mode).await,
            ApprovalOutcome::AutoReview
        );
        // `auto_review` decision persists the auto-review mode too.
        assert_eq!(mode, ApprovalMode::AutoReview);

        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        tx.send(agentd_protocol::adapter::AdapterInboxMsg::ToolDecision {
            call_id: "call-1".into(),
            decision: "unsafe_auto".into(),
        })
        .await
        .unwrap();
        let mut mode = ApprovalMode::Manual;
        assert_eq!(
            wait_for_approval(&mut rx, "call-1", &mut mode).await,
            ApprovalOutcome::UnsafeAuto
        );
        assert_eq!(mode, ApprovalMode::UnsafeAuto);
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

    #[test]
    fn pty_background_detection_only_matches_ctrl_b() {
        assert!(pty_input_requests_background(&[0x02]));
        assert!(!pty_input_requests_background(&[0x02, b'x']));
        assert!(!pty_input_requests_background(&[0x03]));
        assert!(!pty_input_requests_background(b"b"));
    }

    #[test]
    fn background_completion_observation_is_model_only_text() {
        let text = background_completion_observation_text(
            "call_0123456789abcdef",
            "shell",
            true,
            std::time::Duration::from_millis(94_500),
            "stdout:\nBuild & test pass\nexit_code: 0\n",
        );

        assert_eq!(
            text,
            "OBSERVATION: background tool call_01234 (shell) finished ok after 94.5s. Output: stdout:\nBuild & test pass\nexit_code: 0\n"
        );
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
        assert!(
            s.contains("\x1b[") && s.contains("G"),
            "missing column move: {:?}",
            s
        );
    }

    #[test]
    fn slash_matches_narrow_as_typed() {
        let mut ed = editor();
        ed.feed_bytes(b"/");
        let all = ed.slash_matches();
        assert!(all.iter().any(|s| s == "/model"));
        assert!(all.iter().any(|s| s == "/quit"));
        assert!(all.iter().any(|s| s == "/reset"));
        assert!(all.iter().any(|s| s == "/border"));
        assert!(all.iter().any(|s| s == "/compact"));
        // The popup lists canonical names only; `/exit` is an alias of `/quit`
        // (it still resolves when typed) so it isn't a separate ghost entry.
        assert!(!all.iter().any(|s| s == "/exit"));
        ed.feed_bytes(b"bor");
        assert_eq!(ed.slash_matches(), vec!["/border".to_string()]);
        let mut ed = editor();
        ed.feed_bytes(b"/");
        ed.feed_bytes(b"q");
        assert_eq!(ed.slash_matches(), vec!["/quit".to_string()]);
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
        ed.feed_bytes(b"/model codex-oauth:gpt-5.");
        ed.feed_bytes(&[0x09]);
        // gpt-5.5 and gpt-5.4-mini share the prefix gpt-5. — already at common prefix, no-op
        assert_eq!(ed.buf, "/model codex-oauth:gpt-5.");
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

    // --- monitor_model_usable timeout regression ----------------------------
    //
    // Regression: C-x x froze the TUI for ~60 s when the operator model had
    // a broken OAuth token.  The freeze originated in monitor_model_usable,
    // which had no upper bound on the health-check call — a hanging provider
    // could keep the adapter unresponsive long enough for the daemon's
    // adapter.request timeout (60 s) to fire, blocking the render loop.
    //
    // Fix: monitor_model_usable wraps the check in tokio::time::timeout(5 s).
    // This test verifies that a provider that never resolves causes the check
    // to return `false` well within the 5 s bound (we allow 8 s to avoid
    // flakiness on slow CI, but in practice it resolves in ~5 s).

    struct HangingProvider;

    #[async_trait::async_trait]
    impl crate::provider::LlmProvider for HangingProvider {
        fn name(&self) -> &str {
            "hanging-stub"
        }

        async fn complete(
            &self,
            _model: &str,
            _system: &str,
            _messages: &[crate::provider::Message],
            _tools: &[crate::provider::ToolSpec],
            _sink: &mut dyn crate::provider::TextSink,
        ) -> anyhow::Result<crate::provider::ProviderTurn> {
            std::future::pending::<anyhow::Result<crate::provider::ProviderTurn>>().await
        }
    }

    #[tokio::test]
    async fn monitor_model_usable_times_out_on_hanging_provider() {
        let m = crate::agent::ResolvedModel {
            model: "stub".to_string(),
            provider: Box::new(HangingProvider),
            kind: crate::provider::routing::Provider::OpenAI,
            profile: None,
        };
        let start = std::time::Instant::now();
        let usable = monitor_model_usable(&m).await;
        let elapsed = start.elapsed();
        assert!(!usable, "hanging provider should be reported as unusable");
        assert!(
            elapsed < std::time::Duration::from_secs(8),
            "monitor_model_usable took {elapsed:?}, expected < 8 s (5 s timeout + headroom)"
        );
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
    let AdapterContext {
        session_id,
        emit,
        mut inbox,
    } = ctx;
    let mut provider_name = spec.provider_name();
    // User-facing label (`@profile` when from config, else the wire name).
    // `provider_name` stays the wire name for limit/context/monitor keying.
    let mut display_name = spec.display_name();
    let mut model = spec.model.clone();
    let mut provider = spec.provider;
    let cwd = PathBuf::from(&params.cwd);
    let hooks = crate::hooks::Hooks::load(&cwd, &emit);
    let base_hook_payload = crate::hooks::base_payload(&session_id, &cwd, "interactive");
    let registry = std::sync::Arc::new(ToolRegistry::with_defaults());
    let specs = registry.specs();
    let mut approval_mode = if std::env::var("CONSTRUCT_SMITH_AUTOMODE").as_deref() == Ok("1") {
        ApprovalMode::UnsafeAuto
    } else {
        ApprovalMode::Manual
    };
    // Per-model learned input-token limits. Shared with `agent.rs`
    // via `state_dir/smith-model-limits.json`, so a context-overflow
    // learned in one session benefits every later session on the same
    // machine. We hold one instance for the lifetime of this run and
    // mutate through `record_overflow` / `record_call`.
    let mut limits = crate::model_limits::ModelLimits::load();

    // Per-session task registry: tracks every spawned tool's
    // supervisor handle so manual kill/background controls can find
    // the right call_id, and so auto-bg can hand off cleanly. The
    // background-completion channel feeds completions back into the
    // agent loop as `OBSERVATION:` synthetic messages.
    let tasks = crate::tasks::Tasks::new();
    let (bg_completion_tx, mut bg_completion_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::tasks::BackgroundCompletion>();
    let bg_after = crate::tasks::bg_after_duration();

    // Session-local prompt sections are built once here, then reused
    // across every provider.complete call. Resume re-enters this
    // function and refreshes them.
    let system_prompt: String = {
        let mut prompt = crate::agent::system_prompt_for_env().to_string();
        if let Some(section) = crate::project_guide::format_section(&cwd) {
            prompt.push_str("\n\n");
            prompt.push_str(&section);
        }
        if let Some(section) = crate::skills::format_section(&cwd) {
            prompt.push_str("\n\n");
            prompt.push_str(&section);
        }
        prompt
    };

    let term = Terminal::new(&emit);
    let resuming = persist::is_resume();
    // On resume we emit nothing — banner, note, and prompt all stay off
    // so the PTY looks exactly as it did before the daemon restart. The
    // line editor's redraw on the first keystroke will paint the prompt
    // cleanly. (Pressing Enter on an empty line is also a no-op + fresh
    // prompt, so the user has an explicit "wake me up" escape hatch.)
    if !resuming {
        term.banner(&display_name, &model, approval_mode);
    }
    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: Some(format!("{}:{}  [interactive]", display_name, model)),
    });
    hooks
        .run(
            "session_start",
            &cwd,
            &emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "provider": provider_name,
                    "model": model,
                }),
            ),
        )
        .await;
    // The active `❯` lives in the TUI's fixed editor pane, fed by
    // `SessionEvent::EditorState`; no inline prompt write needed.
    let _ = resuming;

    // Clone the id before moving it into ToolCtx — the
    // observation task and slash handlers (e.g. `/loop`) need it.
    let self_id_for_obs = session_id.clone();
    let session_id_for_slash = session_id.clone();
    let tool_ctx = ToolCtx {
        cwd: cwd.clone(),
        session_id: session_id.clone(),
        client: tokio::sync::OnceCell::new(),
        emit: Some(emit.clone()),
        procs: std::sync::Arc::new(crate::tools::proc::ProcRegistry::default()),
        sandbox: crate::agent::announce_sandbox(&emit),
        sandbox_policy: crate::sandbox::SandboxPolicy::workspace_default(&cwd),
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
    // Heal any orphaned tool call / stray tool result in the loaded
    // history before the first provider request. A torn write (crash,
    // turn-timeout SIGKILL, or two adapters briefly sharing one
    // smith.jsonl) can persist an assistant tool-call line without its
    // result; replayed verbatim that one record 400s every codex/openai
    // request and wedges the session permanently. Rewrite the on-disk
    // copy too so the repair sticks across future resumes.
    {
        let repaired = context::sanitize_tool_pairing(&mut messages);
        if repaired > 0 {
            tracing::warn!(
                repaired,
                "smith resume: repaired orphaned tool-call pairing in loaded history"
            );
            if let Some(p) = persist.as_mut() {
                if let Err(e) = p.rewrite(&messages) {
                    tracing::warn!(error = ?e, "smith resume: persist rewrite after repair failed");
                }
            }
        }
    }
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
    let is_orchestrator = std::env::var("CONSTRUCT_SESSION_KIND").as_deref() == Ok("orchestrator");
    let mut obs_rx = if is_orchestrator {
        Some(crate::observe::spawn(self_id_for_obs))
    } else {
        None
    };
    let mut obs_limiter = crate::observe::RateLimiter::new(5, std::time::Duration::from_secs(60));
    let ambient_loop = is_orchestrator.then(|| OperatorAmbientLoop {
        interval: operator_ambient_loop_interval(),
        self_id: session_id.clone(),
    });
    // Runtime toggle: `/operator enable|disable` flips this without restart.
    // Seeded from the env var the daemon re-injects on respawn so the choice
    // survives daemon restarts.
    let mut ambient_loop_enabled = is_orchestrator
        && std::env::var("CONSTRUCT_OPERATOR_LOOP_DISABLED").as_deref() != Ok("1");
    // Operator's own id (to exclude from the ambient fleet snapshot) and the
    // prior-tick session states (for the per-tick delta).
    let self_id_for_ambient = session_id.clone();
    let mut prev_fleet: HashMap<String, agentd_protocol::SessionState> = HashMap::new();
    // Ambient monitor model: the fleet scan + triage runs as a one-shot
    // completion off the operator's own conversation, so the bulky snapshot /
    // previews never accumulate in the operator's context and only escalations
    // reach it. Configure a cheaper model via CONSTRUCT_OPERATOR_MONITOR_MODEL;
    // otherwise it falls back to the operator's own model.
    // Resolve the monitor model (orchestrator-only — that's the only session
    // that ambient-ticks). The explicit override wins; otherwise default to a
    // cheaper same-provider tier (e.g. mini / sonnet) so the per-tick triage
    // doesn't run on the operator's frontier model. A startup health-check
    // falls back to the operator's own model when the chosen model can't be
    // resolved or doesn't actually answer.
    let monitor_model = if is_orchestrator {
        let candidate = std::env::var("CONSTRUCT_OPERATOR_MONITOR_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            // Pass the display label: for a `@profile` it won't match any
            // known wire name, so we keep the operator's own model rather
            // than swap in a cheap default that lives on a different
            // endpoint/billing path than the profile's.
            .or_else(|| default_monitor_spec(&display_name, &model))
            .and_then(|spec| crate::agent::resolve_model_from_spec(&spec).ok());
        match candidate {
            Some(m) if monitor_model_usable(&m).await => {
                tracing::info!(model = %m.model, "operator ambient monitor model");
                Some(m)
            }
            Some(m) => {
                tracing::warn!(
                    model = %m.model,
                    "ambient monitor model unusable; falling back to operator model"
                );
                None
            }
            None => None,
        }
    } else {
        None
    };

    'outer: loop {
        // Wait for a user message — drain order: startup prompt
        // (`pending`), then anything queued during the previous turn,
        // then live typing.
        let mut user_text = if let Some(t) = pending.pop_front() {
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
                &mut approval_mode,
                &mut pty_width,
                obs_rx.as_mut(),
                &mut bg_completion_rx,
                &tasks,
                ambient_loop_enabled.then(|| ambient_loop.clone()).flatten(),
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
                ReadOutcome::AmbientTick => {
                    // Offload the fleet scan + triage to a cheap one-shot
                    // monitor. Only a real finding becomes an operator turn;
                    // quiet ticks never touch the operator's context.
                    let scan =
                        build_ambient_observation(&self_id_for_ambient, &mut prev_fleet).await;
                    let (mp, mm): (&dyn provider::LlmProvider, &str) = match &monitor_model {
                        Some(r) => (r.provider.as_ref(), r.model.as_str()),
                        None => (provider.as_ref(), model.as_str()),
                    };
                    match run_ambient_triage(&scan, mp, mm).await {
                        Some(finding) => finding,
                        None => continue 'outer,
                    }
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
                        tool: bc.tool_name.clone(),
                        ok,
                        output: output_text.clone(),
                        call_id: Some(bc.call_id.clone()),
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
                    let text = background_completion_observation_text(
                        &bc.call_id,
                        &bc.tool_name,
                        ok,
                        bc.duration,
                        &output_text,
                    );
                    // Keep the synthetic observation in the model's
                    // input stream, but don't echo it to the visible
                    // terminal. The real completion was already
                    // emitted as ToolResult above, and printing the
                    // raw OBSERVATION leaks model-control text into
                    // web/TUI output.
                    text
                }
                ReadOutcome::Stop => {
                    hooks
                        .run("session_stop", &cwd, &emit, base_hook_payload.clone())
                        .await;
                    break 'outer;
                }
                ReadOutcome::Eof => {
                    term.note("(end of session)");
                    // Tell the daemon the session ended on its own so
                    // state moves to `Done` cleanly. Without this the
                    // last-broadcast state stays `AwaitingInput`, and
                    // even after the adapter library's run loop exits
                    // (which broadcasts `Closed`), a daemon restart
                    // would resurrect the session via
                    // `resume_running_sessions`. Emitting `Done`
                    // explicitly captures user intent — Ctrl-D meant
                    // "I'm done."
                    term.emit.emit(SessionEvent::Done { exit_code: 0 });
                    hooks
                        .run("session_stop", &cwd, &emit, base_hook_payload.clone())
                        .await;
                    break 'outer;
                }
            }
        };

        // Slash commands never reach the model. Resolve the verb once via the
        // shared registry (crates/protocol/src/slash.rs), then branch on the
        // typed CommandId / routing — never on the raw string:
        //   * Adapter  → mutate smith state here (model / reset / compact)
        //   * ToolCall → synthesize a real tool call (/loop → loop_create)
        //   * Client   → hand off to the attached client as a ClientCommand
        let trimmed = user_text.trim();
        if let Some((verb, args)) = parse_slash_command(trimmed) {
            use agentd_protocol::slash::{CommandId, Routing, SlashCommand};
            match SlashCommand::resolve(&verb) {
                Some(cmd) => match cmd.routing {
                    Routing::Client => {
                        // Typed client action: travels with its CommandId so the
                        // daemon persists it for forensics, `agentd_get_transcript`
                        // filters it (registry visibility), and the client
                        // dispatches by id — no fake `tui` tool call, no model leak.
                        emit.emit(SessionEvent::ClientCommand { id: cmd.id, args });
                    }
                    Routing::ToolCall => {
                        // `/loop`: parse the spec inline (asking the LLM for an
                        // interval when omitted), then synthesize the same
                        // agentd_loop_create call the model would make.
                        handle_slash_loop(
                            args.as_deref().unwrap_or(""),
                            &session_id_for_slash,
                            &emit,
                            &term,
                            provider.as_ref(),
                            &model,
                            &tool_ctx,
                        )
                        .await;
                    }
                    Routing::Adapter => match cmd.id {
                        CommandId::Model => {
                            let arg = args.as_deref().unwrap_or("").trim();
                            if arg.is_empty() {
                                term.note(&format!("(model: {}:{})", display_name, model));
                                // Surface any configured `@profile` endpoints so
                                // the user can discover what `/model @<name>`
                                // accepts without leaving the session.
                                if let Ok(profiles) = crate::provider::config::load_all() {
                                    if !profiles.is_empty() {
                                        let names: Vec<String> =
                                            profiles.keys().map(|n| format!("@{n}")).collect();
                                        term.note(&format!("(profiles: {})", names.join(", ")));
                                    }
                                }
                            } else {
                                match crate::agent::resolve_model_from_spec(arg) {
                                    Ok(new) => {
                                        let new_name = new.provider_name();
                                        let new_display = new.display_name();
                                        let new_model = new.model.clone();
                                        // Tell the daemon the active model
                                        // changed so it records the new spec on
                                        // the session (survives restart) and the
                                        // UI label tracks the switch. A profile
                                        // keeps its `@name` form — re-resolving
                                        // as `provider:model` would drop the
                                        // profile's endpoint/key.
                                        let spec = match &new.profile {
                                            Some(p) => format!("{p}:{new_model}"),
                                            None => format!("{new_name}:{new_model}"),
                                        };
                                        emit.emit(SessionEvent::ModelChanged {
                                            model: spec,
                                        });
                                        provider = new.provider;
                                        provider_name = new_name;
                                        display_name = new_display;
                                        model = new_model;
                                        term.note(&format!(
                                            "(model → {}:{})",
                                            display_name, model
                                        ));
                                        emit.emit(SessionEvent::Status {
                                            state: SessionState::Running,
                                            detail: Some(format!(
                                                "{}:{}  [interactive]",
                                                display_name, model
                                            )),
                                        });
                                    }
                                    Err(e) => {
                                        term.note(&format!("(model switch failed: {e})"));
                                    }
                                }
                            }
                        }
                        CommandId::Reset => {
                            messages.clear();
                            if let Some(p) = persist.as_mut() {
                                p.reset();
                            }
                            pending.clear();
                            queue.clear();
                            editor.reset_history();
                            queued_rows = 0;
                            emit.emit(SessionEvent::Reset);
                            term.banner(&display_name, &model, approval_mode);
                            term.note("(session reset)");
                        }
                        CommandId::Compact => {
                            // Optional N — recent turn pairs to keep verbatim.
                            let rest = args.as_deref().unwrap_or("").trim();
                            let keep_pairs = if rest.is_empty() {
                                Some(crate::compact::DEFAULT_KEEP_PAIRS)
                            } else {
                                match rest.parse::<usize>() {
                                    Ok(n) if n >= 1 => Some(n),
                                    _ => None,
                                }
                            };
                            match keep_pairs {
                                None => term.note(
                                    "(usage: /compact [N]  — N is recent turn pairs to keep)",
                                ),
                                Some(keep_pairs) => {
                                    term.note(&format!(
                                        "(compacting — keeping last {keep_pairs} turn pairs…)"
                                    ));
                                    match crate::compact::compact(
                                        &mut messages,
                                        keep_pairs,
                                        provider.as_ref(),
                                        &model,
                                    )
                                    .await
                                    {
                                        Ok(Some(outcome)) => {
                                            if let Some(p) = persist.as_mut() {
                                                if let Err(e) = p.rewrite(&messages) {
                                                    tracing::warn!(error = ?e, "compact: persist rewrite failed");
                                                }
                                            }
                                            emit.emit(SessionEvent::ContextCompacted {
                                                kept_turns: outcome.kept_turn_pairs,
                                                dropped_turns: outcome.dropped_turn_pairs,
                                                tokens_before: outcome.tokens_before,
                                                tokens_after: outcome.tokens_after,
                                                summary_preview: outcome.summary_preview.clone(),
                                            });
                                            term.note(&format!(
                                                "(compacted {} turns; ~{}→{} tokens)",
                                                outcome.dropped_turn_pairs,
                                                outcome.tokens_before,
                                                outcome.tokens_after,
                                            ));
                                        }
                                        Ok(None) => {
                                            term.note("(nothing to compact — not enough history)");
                                        }
                                        Err(e) => {
                                            term.note(&format!("(compact failed: {e})"));
                                        }
                                    }
                                }
                            }
                        }
                        CommandId::Operator => {
                            if ambient_loop.is_none() {
                                term.note("(/operator is only available in the operator session)");
                            } else {
                                match args.as_deref().map(str::trim).unwrap_or("") {
                                    "enable" => {
                                        ambient_loop_enabled = true;
                                        emit.emit(SessionEvent::OperatorLoopChanged {
                                            enabled: true,
                                        });
                                        term.note("(operator loop enabled)");
                                    }
                                    "disable" => {
                                        ambient_loop_enabled = false;
                                        emit.emit(SessionEvent::OperatorLoopChanged {
                                            enabled: false,
                                        });
                                        term.note("(operator loop disabled)");
                                    }
                                    _ => {
                                        term.note("(usage: /operator enable|disable)");
                                    }
                                }
                            }
                        }
                        // Routing::Adapter is only model/reset/compact/operator today;
                        // any other id here is a registry/handler mismatch.
                        other => {
                            tracing::warn!(?other, "adapter-routed slash command has no handler");
                        }
                    },
                },
                None => {
                    // Unknown verb: legacy `tui` dispatch fallback so a stray
                    // `/foo` still reaches the client's table instead of the
                    // model. Known commands never take this path.
                    let tool_args = match &args {
                        Some(a) => serde_json::json!({ "command": verb, "args": a }),
                        None => serde_json::json!({ "command": verb }),
                    };
                    let pretty = match &args {
                        Some(a) => format!("/{verb} {a}"),
                        None => format!("/{verb}"),
                    };
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
                        call_id: Some(call_id.clone()),
                    });
                    emit.emit(SessionEvent::ToolResult {
                        tool: agentd_protocol::TUI_DISPATCH_TOOL.to_string(),
                        ok: true,
                        output: pretty,
                        call_id: Some(call_id),
                    });
                }
            }
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }
        if trimmed.is_empty() {
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }

        let prompt_payload = hooks
            .mutate(
                "user_prompt_mutate",
                &cwd,
                &emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({ "prompt": user_text }),
                ),
            )
            .await;
        if let Some(prompt) = prompt_payload.get("prompt").and_then(|v| v.as_str()) {
            user_text = prompt.to_string();
        }
        if user_text.trim().is_empty() {
            emit_editor_state(&emit, &editor, &queue);
            continue;
        }

        hooks
            .run(
                "user_prompt_submit",
                &cwd,
                &emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({ "prompt": user_text }),
                ),
            )
            .await;

        push_msg!(
            messages,
            persist,
            Message {
                role: Role::User,
                content: Content::Text {
                    text: user_text.clone()
                },
            }
        );
        // Echo an OBSERVATION trigger into the panel (dim) before the response
        // streams, so the user sees what a reply like `noted` is reacting to —
        // an ambient monitor finding or a fleet event — instead of a bare reply
        // with no context. Real user input echoes itself, so it's skipped.
        if let Some(echo) = observation_panel_echo(&user_text) {
            emit.emit(SessionEvent::pty(&echo));
        }
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
            // Budget mirrors `agent::run` (headless): use the
            // *learned* per-model cap when we have one, else fall back
            // to the hardcoded table. Probe occasionally (criteria in
            // `model_limits::should_probe`) to detect a model quietly
            // raising its limit. Without this, interactive sessions
            // were pruning against the hardcoded cap and hitting the
            // real (often lower) cap with no recovery.
            let now_ms = chrono::Utc::now().timestamp_millis();
            let hardcoded_cap = context::context_window_tokens(provider_name, &model);
            let learned = limits.get(provider_name, &model);
            let est = context::estimate_tokens(&messages) as u64;
            let is_probe =
                learned.is_some() && limits.should_probe(provider_name, &model, est, now_ms);
            let effective_cap = match learned {
                Some(lim) => lim,
                None => hardcoded_cap as u64,
            };
            let budget = if is_probe {
                ((effective_cap as f64) * crate::model_limits::PROBE_OVERFLOW_RATIO) as usize
            } else {
                ((effective_cap as f64) * context::UTILIZATION) as usize
            };
            // Auto-compact pass before the destructive rolling prune.
            // We try this first so historical context survives as a
            // summary instead of vanishing. On any failure (provider
            // error, no cut point), we fall straight through to the
            // existing prune-by-removal path — never block a turn on
            // compaction.
            if crate::compact::auto_compact_enabled() {
                match crate::compact::maybe_auto_compact(
                    &mut messages,
                    effective_cap,
                    provider.as_ref(),
                    &model,
                )
                .await
                {
                    Ok(Some(outcome)) => {
                        if let Some(p) = persist.as_mut() {
                            if let Err(e) = p.rewrite(&messages) {
                                tracing::warn!(error = ?e, "auto-compact: persist rewrite failed");
                            }
                        }
                        emit.emit(SessionEvent::ContextCompacted {
                            kept_turns: outcome.kept_turn_pairs,
                            dropped_turns: outcome.dropped_turn_pairs,
                            tokens_before: outcome.tokens_before,
                            tokens_after: outcome.tokens_after,
                            summary_preview: outcome.summary_preview.clone(),
                        });
                        term.note(&format!(
                            "(auto-compacted {} turns; ~{}→{} tokens)",
                            outcome.dropped_turn_pairs, outcome.tokens_before, outcome.tokens_after,
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = ?e, "auto-compact failed; falling back to prune");
                    }
                }
            }
            let _pruned = context::prune_to_budget(&mut messages, budget);
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
                &mut approval_mode,
                &emit,
                tasks.clone(),
                async {
                    crate::provider_watchdog::complete(
                        provider.as_ref(),
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
                    // Context overflow → learn the real cap and retry
                    // once. Mirrors `agent.rs`. We downcast the
                    // typed sentinel out of the wrapped `anyhow`;
                    // `parse_overflow` in `provider/mod.rs` is what
                    // makes providers emit it.
                    if let Some(ov) = e.downcast_ref::<crate::provider::ContextOverflow>() {
                        let new_limit = limits.record_overflow(
                            provider_name,
                            &model,
                            ov.extracted,
                            effective_cap,
                            now_ms,
                        );
                        let retry_budget = ((new_limit as f64) * context::UTILIZATION) as usize;
                        let _pruned = context::prune_to_budget(&mut messages, retry_budget);
                        term.note(&format!(
                            "(context overflow — relearned cap as {} tokens, retrying)",
                            new_limit
                        ));
                        emit.emit(SessionEvent::Status {
                            state: SessionState::Running,
                            detail: Some(format!(
                                "context overflow — relearning ({} tokens) and retrying",
                                new_limit
                            )),
                        });
                        let mut sink2 = PtySink::new(&emit, pty_width, turn_started_at_ms);
                        let drive2 = drive_with_input_silent(
                            &mut inbox,
                            &mut editor,
                            &mut queue,
                            &mut approval_mode,
                            &emit,
                            tasks.clone(),
                            async {
                                crate::provider_watchdog::complete(
                                    provider.as_ref(),
                                    &model,
                                    &system_prompt,
                                    &messages,
                                    &specs,
                                    &mut sink2,
                                )
                                .await
                            },
                        )
                        .await;
                        match drive2 {
                            DriveExit::Done(Ok(t)) => {
                                sink2.finalize();
                                t
                            }
                            DriveExit::Done(Err(e2)) => {
                                sink2.finalize();
                                final_status = "Errored";
                                // `{:#}` renders the full anyhow cause chain
                                // (context: source: root) so transport-level
                                // failures are actually diagnosable, not just
                                // the outermost label.
                                term.note(&format!("(still over budget after retry: {e2:#})"));
                                emit.emit(SessionEvent::Error {
                                    message: format!("still over budget after retry: {e2:#}"),
                                });
                                break;
                            }
                            DriveExit::Stop | DriveExit::Channel => {
                                sink2.finalize();
                                finish_agent_status(&emit, turn_started_at_ms, "Stopped");
                                break 'outer;
                            }
                            DriveExit::Interrupt => {
                                sink2.finalize();
                                final_status = "Interrupted";
                                term.note("(interrupted)");
                                break;
                            }
                        }
                    } else {
                        final_status = "Errored";
                        // `{:#}` renders the full anyhow cause chain
                        // (e.g. "codex-oauth SSE stream: error reading a body
                        // from connection: connection reset") instead of just
                        // the outermost context, so provider/transport
                        // failures are diagnosable.
                        term.note(&format!("(provider error: {e:#})"));
                        emit.emit(SessionEvent::Error {
                            message: format!("{e:#}"),
                        });
                        break;
                    }
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
            // Record the call so probe state advances (and the learned
            // limit grows on a probe that pushed past the prior cap).
            limits.record_call(
                provider_name,
                &model,
                turn.usage.input_tokens,
                is_probe,
                hardcoded_cap as u64,
                now_ms,
            );
            emit.emit(SessionEvent::Cost {
                usd: turn.usage.usd,
                tokens_in: turn.usage.input_tokens,
                tokens_out: turn.usage.output_tokens,
                tokens_cached: turn.usage.cached_tokens,
            });

            if turn.is_empty() {
                final_status = "Errored";
                let msg = format!(
                    "{} returned an empty response for model {}",
                    display_name, model
                );
                term.note(&format!("(provider error: {msg})"));
                emit.emit(SessionEvent::Error { message: msg });
                break;
            }

            if turn.tool_calls.is_empty() {
                if let Some(text) = turn.text {
                    push_msg!(
                        messages,
                        persist,
                        Message {
                            role: Role::Assistant,
                            content: Content::Text { text },
                        }
                    );
                }
                break;
            }

            push_msg!(
                messages,
                persist,
                Message {
                    role: Role::Assistant,
                    content: Content::AssistantToolCalls {
                        text: turn.text.clone(),
                        calls: turn.tool_calls.clone(),
                    },
                }
            );
            // Partition by risk: Safe in parallel, Risky serial through
            // the approval gate. See agent::run for the matching logic. We
            // use `effective_risk` so auto-approved file writes (e.g. into
            // the widgets dir) batch with the Safe parallel group instead of
            // serializing through a gate they'll skip anyway.
            let mut safe_idx: Vec<usize> = Vec::new();
            let mut risky_idx: Vec<usize> = Vec::new();
            for (i, c) in turn.tool_calls.iter().enumerate() {
                let r = registry
                    .get(&c.name)
                    .map(|t| crate::tools::effective_risk(t, &c.input, &tool_ctx.cwd))
                    .unwrap_or(ToolRisk::Risky);
                if matches!(r, ToolRisk::Safe) {
                    safe_idx.push(i);
                } else {
                    risky_idx.push(i);
                }
            }

            let mut outcomes: std::collections::BTreeMap<
                usize,
                std::result::Result<ToolOutcome, String>,
            > = std::collections::BTreeMap::new();
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
                                serde_json::to_string(&call.input).unwrap_or_default()
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
                    let hooks_c = hooks.clone();
                    let hook_base = base_hook_payload.clone();
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
                            run_safe_call_silent(
                                call_clone, &reg, &ctx_c, &emit_c, &hooks_c, &hook_base,
                            )
                            .await
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
                    let mut outcomes_map: HashMap<usize, std::result::Result<ToolOutcome, String>> =
                        HashMap::new();
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
                    &mut approval_mode,
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
                        &mut approval_mode,
                        &mut editor,
                        &mut queue,
                        &mut queued_rows,
                        provider.as_ref(),
                        &model,
                        &crate::agent::AutoReviewContext {
                            cwd: tool_ctx.cwd.display().to_string(),
                            current_task: messages.iter().rev().find_map(|m| match &m.content {
                                Content::Text { text } if matches!(m.role, Role::User) => {
                                    Some(text.chars().take(2_000).collect())
                                }
                                _ => None,
                            }),
                            recent_approvals: Vec::new(),
                        },
                        tasks.clone(),
                        bg_completion_tx.clone(),
                        bg_after,
                        &hooks,
                        &base_hook_payload,
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
                        push_msg!(
                            messages,
                            persist,
                            Message {
                                role: Role::Tool,
                                content: Content::ToolResult {
                                    call_id: call.id.clone(),
                                    output: truncated,
                                    is_error: !o.ok,
                                },
                            }
                        );
                    }
                    Err(reason) => {
                        push_msg!(
                            messages,
                            persist,
                            Message {
                                role: Role::Tool,
                                content: Content::ToolResult {
                                    call_id: call.id.clone(),
                                    output: format!("(turn aborted: {reason})"),
                                    is_error: true,
                                },
                            }
                        );
                    }
                }
            }
            if early_stop {
                finish_agent_status(&emit, turn_started_at_ms, "Stopped");
                hooks
                    .run("session_stop", &cwd, &emit, base_hook_payload.clone())
                    .await;
                return Ok(());
            }
            if matches!(turn.stop_reason, StopReason::MaxTokens) {
                break;
            }

            // Mid-turn steering: if the user typed while the model streamed
            // or tools ran, those lines were enqueued (not abandoned). Fold
            // them into the conversation NOW — as a user message right after
            // this step's tool results — so the next model step incorporates
            // the new guidance instead of holding it until the whole turn
            // ends. This is additive steering, not an interrupt: in-flight
            // work isn't aborted (Stop/Interrupt remain the way to do that).
            // `drive_with_input_silent` runs silently, so the typed lines
            // were never echoed live — back-fill them into the chat history
            // exactly like the outer loop's queued-input path.
            if let Some(steer) = drain_steering(&mut queue) {
                editor.set_queued_recall(None);
                term.echo_consumed_line(&steer);
                queued_rows = 0;
                emit_editor_state(&emit, &editor, &queue);
                push_msg!(
                    messages,
                    persist,
                    Message {
                        role: Role::User,
                        content: Content::Text {
                            text: steer.clone()
                        },
                    }
                );
                emit.emit(SessionEvent::Message {
                    role: agentd_protocol::MessageRole::User,
                    text: steer,
                });
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
    /// Ambient operator loop tick while the orchestrator is idle.
    AmbientTick,
    /// A backgrounded tool just finished. The outer loop emits the
    /// real `ToolResult` event (so the transcript catches up) and
    /// synthesizes an `OBSERVATION:` user message so the agent's
    /// next turn knows about the completion.
    BackgroundCompletion(crate::tasks::BackgroundCompletion),
    Stop,
    Eof,
}

#[derive(Clone)]
struct OperatorAmbientLoop {
    interval: Duration,
    self_id: String,
}

fn operator_ambient_loop_interval() -> Duration {
    let secs = std::env::var("CONSTRUCT_OPERATOR_AMBIENT_LOOP_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(60)
        .clamp(10, 86_400);
    Duration::from_secs(secs)
}

/// Bare ambient-tick prompt used when the daemon snapshot is unavailable.
const AMBIENT_TICK_FALLBACK: &str = "OBSERVATION: ambient operator loop tick. Quietly inspect only if useful; update Operator widgets for helpful ambient status; reply exactly `noted` if nothing needs surfacing.";

/// Dim PTY echo of an `OBSERVATION:` trigger so the operator panel shows what a
/// response (e.g. a bare `noted`) is reacting to — an ambient monitor finding
/// or a fleet event — instead of an answer with no visible question. Returns
/// `None` for non-observation turns (real user input echoes itself). The
/// monitor's instruction boilerplate is stripped so only the substance shows.
fn observation_panel_echo(user_text: &str) -> Option<Vec<u8>> {
    let body = user_text.strip_prefix("OBSERVATION: ")?;
    let display = match body.split_once('\n') {
        Some((first, rest)) if first.starts_with("ambient fleet monitor flagged") => {
            format!("ambient monitor flagged:\n{}", rest.trim_end())
        }
        _ => body.trim_end().to_string(),
    };
    if display.trim().is_empty() {
        return None;
    }
    let mut out = String::from("\r\n");
    for line in display.lines() {
        out.push_str("\x1b[2m\u{2502} ");
        out.push_str(line);
        out.push_str("\x1b[0m\r\n");
    }
    Some(out.into_bytes())
}

/// Minutes a `Running` PTY session may go without output before the snapshot
/// treats it as idle (likely waiting for input, or stuck). Interactive
/// claude/codex/shell sessions never emit `AwaitingInput`, so PTY quiescence
/// (`last_pty_at_ms`) is the only "is it waiting?" signal available for them.
const IDLE_RUNNING_MINS: i64 = 10;

fn ambient_active_window() -> Duration {
    let secs = std::env::var("CONSTRUCT_OPERATOR_ACTIVE_WINDOW_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(IDLE_RUNNING_MINS as u64 * 60)
        .clamp(60, 86_400);
    Duration::from_secs(secs)
}

/// Max sessions previewed per tick. Override with `CONSTRUCT_OPERATOR_PREVIEW_SESSIONS`.
fn preview_session_cap() -> usize {
    std::env::var("CONSTRUCT_OPERATOR_PREVIEW_SESSIONS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(10)
        .min(50)
}
/// Per-session preview byte budget. Override with `CONSTRUCT_OPERATOR_PREVIEW_BYTES`.
/// When the recent messages exceed it, the older part is truncated.
fn preview_byte_cap() -> usize {
    std::env::var("CONSTRUCT_OPERATOR_PREVIEW_BYTES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(800)
        .clamp(120, 8000)
}

/// Keep the tail (most recent) of `s` within `max_bytes`, truncating the older
/// front and marking it. Char-boundary safe.
fn truncate_keep_tail(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…(older truncated) {}", &s[start..])
}

struct AmbientSnapshot {
    summary: String,
    /// `(session_id, label)` for the notable sessions worth an inline preview,
    /// most important first. The caller previews the top [`PREVIEW_SESSIONS`].
    preview_targets: Vec<(String, String)>,
}

async fn operator_fleet_has_active_session(self_id: &str) -> bool {
    let socket = agentd_protocol::paths::Paths::discover().socket();
    let Ok(client) = agentd_client::Client::connect(&socket).await else {
        return false;
    };
    let Ok(sessions) = client.list().await else {
        return false;
    };
    let now = chrono::Utc::now();
    let window = ambient_active_window();
    sessions
        .iter()
        .any(|session| ambient_session_is_active(session, self_id, now, window))
}

fn ambient_session_is_active(
    session: &agentd_protocol::SessionSummary,
    self_id: &str,
    now: chrono::DateTime<chrono::Utc>,
    window: Duration,
) -> bool {
    if session.id == self_id || session.state.is_terminal() {
        return false;
    }
    let window_ms = window.as_millis() as i64;
    let now_ms = now.timestamp_millis();
    let recent_pty = session
        .last_pty_at_ms
        .is_some_and(|last| now_ms.saturating_sub(last).max(0) <= window_ms);
    let recent_event = session
        .last_event_at
        .is_some_and(|last| (now - last).num_milliseconds().max(0) <= window_ms);
    recent_pty
        || recent_event
        || session.state == agentd_protocol::SessionState::Running
            && session.last_pty_at_ms.is_none()
            && session.last_event_at.is_none()
}

/// Build the ambient-tick observation text. Pulls a live fleet snapshot from
/// the daemon, folds in what changed since the previous tick, and attaches a
/// short recent-transcript preview for the few most notable sessions — so the
/// Operator has concrete signal instead of a blank "go look".
/// Falls back to [`AMBIENT_TICK_FALLBACK`] if the daemon can't be reached.
async fn build_ambient_observation(
    self_id: &str,
    prev: &mut HashMap<String, agentd_protocol::SessionState>,
) -> String {
    let socket = agentd_protocol::paths::Paths::discover().socket();
    let Ok(client) = agentd_client::Client::connect(&socket).await else {
        return AMBIENT_TICK_FALLBACK.to_string();
    };
    let Ok(sessions) = client.list().await else {
        return AMBIENT_TICK_FALLBACK.to_string();
    };
    let snap = compute_ambient_snapshot(&sessions, self_id, prev, chrono::Utc::now());
    let mut out = snap.summary;
    // Attach a byte-bounded preview for the most relevant sessions (needs-
    // attention first, then recently-active). Bounded count + per-session byte
    // budget keep input context in check; the full session id in each heading
    // lets the Operator drill in with agentd_get_transcript/output/diff.
    let bytes = preview_byte_cap();
    for (sid, label) in snap.preview_targets.iter().take(preview_session_cap()) {
        if let Some(preview) = fetch_session_preview(&client, sid, bytes).await {
            out.push_str(&format!("\n\n[{sid}] {label} — recent:\n{preview}"));
        }
    }
    out
}

/// System prompt for the one-shot ambient fleet-monitor triage. It judges the
/// snapshot + previews and returns only what's worth the operator's attention.
const AMBIENT_MONITOR_SYSTEM: &str = "You are an ambient fleet monitor for an operator agent. \
You are given a current fleet snapshot plus recent-activity previews of notable sessions. \
Identify ONLY things genuinely worth the operator's attention: a session that looks stuck or \
blocked (e.g. waiting on a prompt like 'trust this folder?'), errored, finished with notable \
output, or a clear opportunity to reduce user effort. Be conservative — routine activity and \
normal idleness are NOT notable, and you have no broader context about what the user is doing. \
If anything qualifies, reply with at most 3 one-line findings, each: '<session id> \"<title>\": \
<what + why, with a short evidence snippet>'. If nothing qualifies, reply with exactly the single \
word: nothing";

/// Default monitor model when `CONSTRUCT_OPERATOR_MONITOR_MODEL` is unset: a
/// cheaper tier on the **same provider** as the operator (so auth/keys are
/// already present), using model names the codebase/provider is known to
/// accept. Returns `None` — keep the operator's own model — when the operator
/// is already on a small model, or the provider has no obvious cheap default
/// (gemini/ollama/unknown), so we never silently pick a non-existent model.
fn default_monitor_spec(provider_name: &str, operator_model: &str) -> Option<String> {
    let m = operator_model.to_ascii_lowercase();
    if m.contains("mini") || m.contains("nano") || m.contains("haiku") {
        return None; // already a small/cheap model
    }
    match provider_name {
        // ChatGPT-account Codex exposes a restricted model set; `gpt-5.4-mini`
        // is the small tier (the `*-codex-mini` names are rejected). The
        // startup health-check falls back if an account lacks it.
        "codex-oauth" => Some("codex-oauth:gpt-5.4-mini".to_string()),
        "openai" => Some("openai:gpt-5-mini".to_string()),
        "anthropic" if !m.contains("sonnet") => Some("anthropic:claude-sonnet-4-5".to_string()),
        // anthropic already on sonnet, or gemini/ollama/unknown → keep operator's.
        _ => None,
    }
}

/// One-shot liveness check for the chosen monitor model. A wrong model name
/// resolves fine but 400s at call time, which would silently blind the monitor
/// (every triage returns "nothing"). A tiny completion at startup lets us fall
/// back to the operator's own model — which we know works — instead.
///
/// Bounded to 5 seconds: the check is best-effort and must not hold up the
/// adapter startup sequence (which blocks the TUI pty_resize path and freezes
/// the render loop for the full 60 s adapter.request timeout when it hangs).
async fn monitor_model_usable(m: &crate::agent::ResolvedModel) -> bool {
    let messages = vec![Message {
        role: Role::User,
        content: Content::Text {
            text: "Reply with the single word: ok".to_string(),
        },
    }];
    let mut sink = DiscardSink;
    let fut = crate::provider_watchdog::complete(
        m.provider.as_ref(),
        &m.model,
        "Health check.",
        &messages,
        &[],
        &mut sink,
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), fut)
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Null sink for one-shot completions where we only want the final text.
struct DiscardSink;
impl TextSink for DiscardSink {
    fn delta(&mut self, _text: &str) {}
}

/// Run the ambient monitor: a one-shot completion (cheap model when configured)
/// that triages the scan off the operator's context. Returns a compact
/// operator-facing observation when there's a finding, or `None` to skip the
/// operator turn entirely.
async fn run_ambient_triage(
    scan: &str,
    provider: &dyn provider::LlmProvider,
    model: &str,
) -> Option<String> {
    let messages = vec![Message {
        role: Role::User,
        content: Content::Text {
            text: scan.to_string(),
        },
    }];
    let mut sink = DiscardSink;
    let turn = crate::provider_watchdog::complete(
        provider,
        model,
        AMBIENT_MONITOR_SYSTEM,
        &messages,
        &[],
        &mut sink,
    )
    .await
    .ok()?;
    parse_triage_finding(turn.text.as_deref().unwrap_or(""))
}

/// Turn the monitor's raw reply into an operator-facing observation, or `None`
/// when it found nothing worth surfacing.
fn parse_triage_finding(reply: &str) -> Option<String> {
    let t = reply.trim().trim_end_matches('.');
    if t.is_empty() || t.eq_ignore_ascii_case("nothing") {
        return None;
    }
    Some(format!(
        "OBSERVATION: ambient fleet monitor flagged the following. Decide whether to surface it to \
         the user or update an Operator widget; reply exactly `noted` if it's not worth it.\n{}",
        reply.trim()
    ))
}

/// Compact, ANSI-free preview of a session's recent activity, byte-bounded
/// (older part truncated past `max_bytes`). Prefers the rendered PTY screen —
/// it works for PTY-heavy claude/codex sessions whose transcript tail is all
/// `Pty` markers — and falls back to recent transcript messages for headless /
/// no-PTY sessions.
async fn fetch_session_preview(
    client: &agentd_client::Client,
    sid: &str,
    max_bytes: usize,
) -> Option<String> {
    if let Some(screen) = pty_screen_preview(client, sid, max_bytes).await {
        return Some(screen);
    }
    let tr = client.transcript_tail(sid, 40).await.ok()?;
    let mut msgs: Vec<String> = Vec::new();
    for ev in tr.events.iter().rev() {
        if let agentd_protocol::SessionEvent::Message { role, text } = &ev.event {
            let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
            if collapsed.is_empty() {
                continue;
            }
            msgs.push(format!("[{role:?}] {collapsed}"));
            if msgs.len() >= 8 {
                break;
            }
        }
    }
    if msgs.is_empty() {
        return None;
    }
    msgs.reverse();
    Some(truncate_keep_tail(
        &format!("  {}", msgs.join("\n  ")),
        max_bytes,
    ))
}

/// Render the recent PTY-log tail through a `vt100` parser and return the
/// screen's non-blank lines (byte-bounded). `None` for sessions with no PTY.
async fn pty_screen_preview(
    client: &agentd_client::Client,
    sid: &str,
    max_bytes: usize,
) -> Option<String> {
    use base64::Engine as _;
    let replay = client.pty_replay_tail(sid, 64 * 1024).await.ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(replay.data.as_bytes())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let (rows, cols) = replay
        .size
        .map(|s| (s.rows.max(1), s.cols.max(1)))
        .unwrap_or((30, 100));
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(&bytes);
    let contents = parser.screen().contents();
    // Keep lines with real content; drop blanks and pure box-drawing/separator
    // rows that just eat the byte budget.
    let lines: Vec<&str> = contents
        .lines()
        .filter(|l| l.chars().any(|c| c.is_alphanumeric()))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(truncate_keep_tail(
        &format!("  {}", lines.join("\n  ")),
        max_bytes,
    ))
}

/// Pure snapshot builder: counts active sessions by state (including idle
/// running, detected via PTY quiescence), lists what changed since the previous
/// tick, flags what needs attention, and returns the notable sessions to
/// preview. Split from the daemon round-trip so it's testable. Updates `prev`.
fn compute_ambient_snapshot(
    sessions: &[agentd_protocol::SessionSummary],
    self_id: &str,
    prev: &mut HashMap<String, agentd_protocol::SessionState>,
    now: chrono::DateTime<chrono::Utc>,
) -> AmbientSnapshot {
    use agentd_protocol::SessionState;
    let now_ms = now.timestamp_millis();
    let first_tick = prev.is_empty();
    let label = |s: &agentd_protocol::SessionSummary| -> String {
        let title = s.title.clone().unwrap_or_else(|| s.harness.clone());
        let short: String = s.id.chars().take(8).collect();
        format!("{short} \"{}\"", title.chars().take(32).collect::<String>())
    };
    let event_idle_min = |s: &agentd_protocol::SessionSummary| -> i64 {
        s.last_event_at
            .map(|t| (now - t).num_minutes().max(0))
            .unwrap_or(0)
    };
    // Minutes since last PTY byte — the "actually quiet?" signal. `None` (no
    // PTY, e.g. headless) yields a negative value so it never counts as idle.
    let pty_idle_min = |s: &agentd_protocol::SessionSummary| -> i64 {
        match s.last_pty_at_ms {
            Some(ms) => (now_ms - ms).max(0) / 60_000,
            None => -1,
        }
    };

    let (mut running, mut awaiting, mut errored, mut idle) = (0usize, 0usize, 0usize, 0usize);
    // (session_id, line) per category, in priority order for needs-attention.
    let mut errored_list: Vec<(String, String)> = Vec::new();
    let mut idle_list: Vec<(String, String)> = Vec::new();
    let mut awaiting_list: Vec<(String, String)> = Vec::new();
    // Running sessions with recent PTY output — previewed (not "needs
    // attention") so the Operator can see what's actively happening.
    // (pty_idle_min, session_id, line); sorted most-recent-first below.
    let mut active_list: Vec<(i64, String, String)> = Vec::new();
    let mut changes: Vec<String> = Vec::new();
    let mut cur: HashMap<String, SessionState> = HashMap::new();

    for s in sessions {
        if s.id == self_id {
            continue;
        }
        cur.insert(s.id.clone(), s.state);
        if !first_tick && prev.get(&s.id) != Some(&s.state) {
            match s.state {
                SessionState::Errored => changes.push(format!("{} → errored", label(s))),
                SessionState::AwaitingInput => {
                    changes.push(format!("{} → awaiting_input", label(s)))
                }
                SessionState::Done => changes.push(format!("{} → done", label(s))),
                _ => {}
            }
        }
        match s.state {
            SessionState::Running => {
                running += 1;
                let pm = pty_idle_min(s);
                if pm >= IDLE_RUNNING_MINS {
                    idle += 1;
                    idle_list.push((
                        s.id.clone(),
                        format!("{} running but quiet {pm}m (idle/waiting?)", label(s)),
                    ));
                } else {
                    let ago = if pm < 0 {
                        "?".to_string()
                    } else {
                        format!("{pm}m")
                    };
                    active_list.push((
                        pm.max(0),
                        s.id.clone(),
                        format!("{} running, last output {ago} ago", label(s)),
                    ));
                }
            }
            SessionState::AwaitingInput => {
                awaiting += 1;
                let m = event_idle_min(s);
                if m >= 30 {
                    awaiting_list.push((s.id.clone(), format!("{} awaiting_input {m}m", label(s))));
                }
            }
            SessionState::Errored => {
                errored += 1;
                errored_list.push((
                    s.id.clone(),
                    format!("{} errored {}m ago", label(s), event_idle_min(s)),
                ));
            }
            _ => {}
        }
    }
    *prev = cur;

    // "Needs attention" = the concerning sessions only.
    let mut attention: Vec<(String, String)> = Vec::new();
    attention.extend(errored_list.iter().cloned());
    attention.extend(idle_list.iter().cloned());
    attention.extend(awaiting_list.iter().cloned());

    // Preview targets = needs-attention sessions, then recently-active ones
    // (most recent output first). Capped by the caller (preview_session_cap).
    active_list.sort_by_key(|(pm, _, _)| *pm);
    let mut preview_targets: Vec<(String, String)> = Vec::new();
    preview_targets.extend(errored_list);
    preview_targets.extend(idle_list);
    preview_targets.extend(awaiting_list);
    preview_targets.extend(active_list.into_iter().map(|(_, id, line)| (id, line)));

    let cap_lines = |v: &[(String, String)], n: usize| -> String {
        if v.is_empty() {
            return "none".to_string();
        }
        let extra = v.len().saturating_sub(n);
        let mut shown: Vec<String> = v.iter().take(n).map(|(_, l)| l.clone()).collect();
        if extra > 0 {
            shown.push(format!("(+{extra} more)"));
        }
        shown.join("; ")
    };
    let changes_line = if first_tick {
        "(first tick — baseline only)".to_string()
    } else if changes.is_empty() {
        "none".to_string()
    } else {
        let extra = changes.len().saturating_sub(6);
        let mut c: Vec<String> = changes.into_iter().take(6).collect();
        if extra > 0 {
            c.push(format!("(+{extra} more)"));
        }
        c.join("; ")
    };

    // Data-only fleet snapshot — this feeds the one-shot monitor triage, which
    // owns the judgment instructions (see AMBIENT_MONITOR_SYSTEM).
    let summary = format!(
        "Fleet snapshot.\n\
         Fleet now: {running} running ({idle} idle ≥{IDLE_RUNNING_MINS}m), {awaiting} awaiting_input, {errored} errored.\n\
         Changes since last tick: {changes_line}.\n\
         Needs attention: {}.\n\
         Note: a 'running but quiet' session is likely waiting for the user or stuck — interactive \
         claude/codex sessions don't signal awaiting_input, so judge them by the previews below.\n\
         Recent-activity previews of notable sessions follow, each headed by its full session id.",
        cap_lines(&attention, 6),
    );

    AmbientSnapshot {
        summary,
        preview_targets,
    }
}

fn background_completion_observation_text(
    call_id: &str,
    tool_name: &str,
    ok: bool,
    duration: std::time::Duration,
    output_text: &str,
) -> String {
    let short_call: String = call_id.chars().take(10).collect();
    let preview: String = output_text.chars().take(160).collect();
    let label = if ok { "ok" } else { "failed" };
    format!(
        "OBSERVATION: background tool {} ({}) finished {} after {:.1}s. Output: {}",
        short_call,
        tool_name,
        label,
        duration.as_secs_f64(),
        preview
    )
}

#[allow(clippy::too_many_arguments)]
async fn read_one_line(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    approval_mode: &mut ApprovalMode,
    pty_width: &mut usize,
    mut obs_rx: Option<&mut tokio::sync::mpsc::UnboundedReceiver<crate::observe::Observation>>,
    bg_completion_rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::tasks::BackgroundCompletion>,
    tasks: &std::sync::Arc<crate::tasks::Tasks>,
    ambient_loop: Option<OperatorAmbientLoop>,
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
        let ambient_tick = async {
            let Some(config) = ambient_loop.clone() else {
                std::future::pending::<()>().await;
                return;
            };
            loop {
                tokio::time::sleep(config.interval).await;
                if operator_fleet_has_active_session(&config.self_id).await {
                    return;
                }
            }
        };
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
            _ = ambient_tick => return ReadOutcome::AmbientTick,
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
            Some(AdapterInboxMsg::SetApprovalMode(mode)) => {
                *approval_mode = mode;
            }
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
    approval_mode: &mut ApprovalMode,
    editor: &mut LineEditor,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    provider: &dyn crate::provider::LlmProvider,
    model: &str,
    review_ctx: &crate::agent::AutoReviewContext,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    bg_completion_tx: crate::tasks::BgCompletionTx,
    bg_after: std::time::Duration,
    hooks: &crate::hooks::Hooks,
    base_hook_payload: &serde_json::Value,
) -> std::result::Result<ToolOutcome, String> {
    let mut call = call.clone();
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            // Items model synthesizes the block from these events.
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
                call_id: Some(call.id.clone()),
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: format!("unknown tool: {}", call.name),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
        }
    };

    let mutation = hooks
        .mutate(
            "pre_tool_use_mutate",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": tool.args_summary(&call.input),
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    if let Some(args) = mutation.get("args") {
        call.input = args.clone();
    }
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
        call_id: Some(call.id.clone()),
    });
    hooks
        .run(
            "pre_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": args_summary,
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    // Task-lifecycle event for the daemon's per-session task
    // registry — surfaces in `/tasks` + MCP `agentd_get_tasks`.
    emit.emit(SessionEvent::TaskStart {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        args_summary: args_summary.clone(),
    });

    let is_risky = matches!(
        crate::tools::effective_risk(tool, &call.input, &tool_ctx.cwd),
        ToolRisk::Risky
    );
    // Decide whether this risky call needs a human prompt. In auto-review
    // mode the reviewer vets it first and may defer to the user; it can
    // only approve or ask — it never denies on its own, so the human
    // always makes the final reject call.
    let mut ask_user = is_risky && matches!(*approval_mode, ApprovalMode::Manual);
    let mut allow_auto_review = true;
    if is_risky && matches!(*approval_mode, ApprovalMode::AutoReview) {
        match crate::agent::auto_review_for_adapter(
            provider,
            model,
            call.name.as_str(),
            &args_summary,
            &call.input,
            review_ctx,
        )
        .await
        {
            crate::agent::AutoReviewResult::Approve => {}
            crate::agent::AutoReviewResult::Deny | crate::agent::AutoReviewResult::AskUser => {
                ask_user = true;
                allow_auto_review = false;
            }
        }
    }
    if ask_user {
        emit.emit(SessionEvent::ToolApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_summary: args_summary.clone(),
            risk: tool.risk(),
            allow_auto_review,
        });
        hooks
            .run(
                "tool_approval_request",
                &tool_ctx.cwd,
                emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({
                        "call_id": call.id,
                        "tool": call.name,
                        "args": call.input,
                        "args_summary": args_summary,
                        "risk": tool.risk(),
                    }),
                ),
            )
            .await;
        // Prompt until we reach a terminal decision. Pressing `a`
        // switches the session into auto-review mode (which persists)
        // and vets this call; if the reviewer still wants a human we
        // loop and ask again.
        let mut denied = false;
        loop {
            term.approval(&call.name, &args_summary, tool.risk(), allow_auto_review);
            let mode_before_approval = *approval_mode;
            let approval_outcome = wait_for_approval(inbox, &call.id, approval_mode).await;
            if *approval_mode != mode_before_approval {
                emit.emit(SessionEvent::ApprovalModeChanged {
                    mode: *approval_mode,
                });
            }
            match approval_outcome {
                ApprovalOutcome::Stop => {
                    emit.emit(SessionEvent::ToolApprovalResolved {
                        call_id: call.id.clone(),
                    });
                    return Err("stop".into());
                }
                ApprovalOutcome::Interrupt => {
                    emit.emit(SessionEvent::ToolApprovalResolved {
                        call_id: call.id.clone(),
                    });
                    return Err("interrupt".into());
                }
                ApprovalOutcome::Deny => {
                    term.print("n\r\n");
                    denied = true;
                    break;
                }
                ApprovalOutcome::Approve => {
                    term.print("y\r\n");
                    break;
                }
                ApprovalOutcome::UnsafeAuto => {
                    term.print("f\r\n");
                    break;
                }
                ApprovalOutcome::AutoReview => {
                    term.print("a\r\n");
                    match crate::agent::auto_review_for_adapter(
                        provider,
                        model,
                        call.name.as_str(),
                        &args_summary,
                        &call.input,
                        review_ctx,
                    )
                    .await
                    {
                        crate::agent::AutoReviewResult::Approve => break,
                        // Never deny outright — bounce back to the user.
                        crate::agent::AutoReviewResult::Deny
                        | crate::agent::AutoReviewResult::AskUser => {
                            allow_auto_review = false;
                            continue;
                        }
                    }
                }
            }
        }
        // The pending approval is resolved (answered here or from another
        // client) — tell passive viewers (web dialog, TUI minibuffer) to
        // dismiss their prompt.
        emit.emit(SessionEvent::ToolApprovalResolved {
            call_id: call.id.clone(),
        });
        if denied {
            let msg = "user denied this action".to_string();
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: msg.clone(),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: msg,
            });
        }
    }

    // Sandbox escalation (spec 0029): a Risky (effective) call that reaches
    // this point has been *permitted* — user-approved, auto-review-approved,
    // or an auto mode — so it may legitimately cross the confined boundary;
    // run it with the policy relaxed. Safe calls keep the confined floor.
    let escalated_ctx;
    let run_ctx = if is_risky {
        escalated_ctx = tool_ctx.escalated();
        &escalated_ctx
    } else {
        tool_ctx
    };
    let supervisor_outcome = run_with_supervisor(
        call.id.clone(),
        call.name.clone(),
        args_summary.clone(),
        registry.clone(),
        call.input.clone(),
        run_ctx,
        inbox,
        editor,
        term,
        queue,
        queued_rows,
        approval_mode,
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
                tool: call.name.clone(),
                ok: o.ok,
                output: o.output.clone(),
                call_id: Some(call.id.clone()),
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
                tool: call.name.clone(),
                ok: false,
                output: format!("({reason})"),
                call_id: Some(call.id.clone()),
            });
            emit.emit(SessionEvent::TaskEnd {
                call_id: call.id.clone(),
                ok: false,
                output_preview: format!("({reason})"),
            });
        }
    }
    let (ok, output) = match &outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    };
    hooks
        .run(
            "post_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "ok": ok,
                    "output": truncate_for_model(&output, TOOL_OUTPUT_BUDGET),
                }),
            ),
        )
        .await;
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalOutcome {
    Approve,
    Deny,
    AutoReview,
    UnsafeAuto,
    Stop,
    Interrupt,
}

async fn wait_for_approval(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    call_id: &str,
    approval_mode: &mut ApprovalMode,
) -> ApprovalOutcome {
    loop {
        match inbox.recv().await {
            None => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ApprovalOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => return ApprovalOutcome::Interrupt,
            Some(AdapterInboxMsg::SetApprovalMode(mode)) => {
                *approval_mode = mode;
                match mode {
                    ApprovalMode::UnsafeAuto => return ApprovalOutcome::UnsafeAuto,
                    ApprovalMode::AutoReview => return ApprovalOutcome::AutoReview,
                    ApprovalMode::Manual => {}
                }
            }
            Some(AdapterInboxMsg::ToolDecision {
                call_id: cid,
                decision,
            }) if cid == call_id => {
                return match decision.as_str() {
                    "approve" => ApprovalOutcome::Approve,
                    "auto_review" => {
                        *approval_mode = ApprovalMode::AutoReview;
                        ApprovalOutcome::AutoReview
                    }
                    "unsafe_auto" => {
                        *approval_mode = ApprovalMode::UnsafeAuto;
                        ApprovalOutcome::UnsafeAuto
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
                            *approval_mode = ApprovalMode::AutoReview;
                            return ApprovalOutcome::AutoReview;
                        }
                        b'f' | b'F' => {
                            *approval_mode = ApprovalMode::UnsafeAuto;
                            return ApprovalOutcome::UnsafeAuto;
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
    mut call: provider::ToolCall,
    registry: &ToolRegistry,
    ctx: &ToolCtx,
    emit: &EventEmitter,
    hooks: &crate::hooks::Hooks,
    base_hook_payload: &serde_json::Value,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
                call_id: Some(call.id.clone()),
            });
            let msg = format!("unknown tool: {}", call.name);
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: msg.clone(),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: msg,
            });
        }
    };
    let mutation = hooks
        .mutate(
            "pre_tool_use_mutate",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": tool.args_summary(&call.input),
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    if let Some(args) = mutation.get("args") {
        call.input = args.clone();
    }
    let args_summary_for_event = tool.args_summary(&call.input);
    hooks
        .run(
            "pre_tool_use",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": args_summary_for_event,
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
        call_id: Some(call.id.clone()),
    });
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
                tool: call.name.clone(),
                ok: o.ok,
                output: o.output.clone(),
                call_id: Some(call.id.clone()),
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
                tool: call.name.clone(),
                ok: false,
                output: format!("({reason})"),
                call_id: Some(call.id.clone()),
            });
            emit.emit(SessionEvent::TaskEnd {
                call_id: call.id.clone(),
                ok: false,
                output_preview: format!("({reason})"),
            });
        }
    }
    let (ok, output) = match &outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    };
    hooks
        .run(
            "post_tool_use",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "ok": ok,
                    "output": truncate_for_model(&output, TOOL_OUTPUT_BUDGET),
                }),
            ),
        )
        .await;
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
    approval_mode: &mut ApprovalMode,
    tasks: std::sync::Arc<crate::tasks::Tasks>,
    bg_completion_tx: crate::tasks::BgCompletionTx,
    bg_after: std::time::Duration,
) -> crate::tasks::SupervisorOutcome {
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let emit = ctx.emit.clone();
    let procs = ctx.procs.clone();
    let sandbox = ctx.sandbox.clone();
    let sandbox_policy = ctx.sandbox_policy.clone();
    let client_seed = ctx.client.get().cloned();
    let tool_name_for_runner = tool_name.clone();
    let tool_runner = async move {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
            emit,
            procs,
            sandbox,
            sandbox_policy,
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
    // the editor / queue / approval_mode handlers like before.
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
        approval_mode,
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
    outcome: std::result::Result<crate::tasks::SupervisorOutcome, tokio::task::JoinError>,
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
/// `run_with_supervisor` so kill/background controls on the live tool block
/// actually take effect. Other inbox handling is identical to the
/// regular `drive_with_input`.
#[allow(clippy::too_many_arguments)]
async fn drive_with_input_relaying<F>(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    queue: &mut VecDeque<String>,
    queued_rows: &mut usize,
    approval_mode: &mut ApprovalMode,
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
                    Some(AdapterInboxMsg::SetApprovalMode(mode)) => *approval_mode = mode,
                    Some(AdapterInboxMsg::Input(t)) => {
                        enqueue_line(queue, editor, t);
                        emit_editor_state(term.emit, editor, queue);
                    }
                    Some(AdapterInboxMsg::PtyInput(bytes)) => {
                        if pty_input_requests_interrupt(&bytes) {
                            return DriveExit::Interrupt;
                        }
                        if pty_input_requests_background(&bytes) {
                            let _ = crate::tasks::forward_control(
                                &tasks,
                                this_call_id,
                                crate::tasks::ToolControl::Background,
                            )
                            .await;
                            continue;
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
        call_id: Some(call_id.clone()),
    });
    emit.emit(SessionEvent::ToolResult {
        tool: "agentd_loop_create".to_string(),
        ok: true,
        output: serde_json::to_string(&loop_obj).unwrap_or_default(),
        call_id: Some(call_id),
    });

    let note = format!(
        "(loop {} every {}s — \"{}\"{}{})",
        loop_obj.id.chars().take(10).collect::<String>(),
        clamped,
        prompt.chars().take(60).collect::<String>(),
        if suggested {
            " — interval suggested"
        } else {
            ""
        },
        if was_clamped {
            " — clamped to bounds"
        } else {
            ""
        },
    );
    term.note(&note);
}

/// Read the bounds the daemon's loop module uses for clamping.
/// Duplicated from `daemon::loops::clamp_interval` because the
/// adapter doesn't share that module — but the env-var keys
/// match so a deployment-time override applies to both sides.
fn clamp_interval_for_slash(secs: u64) -> (u64, bool) {
    let min = std::env::var("CONSTRUCT_LOOP_MIN_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30u64);
    let max = std::env::var("CONSTRUCT_LOOP_MAX_SECS")
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
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let args = if args.is_empty() {
        None
    } else {
        Some(args.to_string())
    };
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
fn enqueue_line(queue: &mut VecDeque<String>, editor: &mut LineEditor, line: String) {
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

/// Drain everything the user enqueued during the current turn into a single
/// newline-joined steering message, or `None` if the queue is empty. Used at
/// a turn's step boundary to fold live user input into the conversation so
/// the agent can be steered mid-task without waiting for the turn to finish.
fn drain_steering(queue: &mut VecDeque<String>) -> Option<String> {
    if queue.is_empty() {
        return None;
    }
    Some(queue.drain(..).collect::<Vec<_>>().join("\n"))
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
    approval_mode: &mut ApprovalMode,
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
                    Some(AdapterInboxMsg::SetApprovalMode(mode)) => *approval_mode = mode,
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
    approval_mode: &mut ApprovalMode,
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
                    Some(AdapterInboxMsg::SetApprovalMode(mode)) => *approval_mode = mode,
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
