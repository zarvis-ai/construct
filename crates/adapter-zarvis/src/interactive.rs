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

use crate::agent::{push_msg, ResolvedModel, SYSTEM_PROMPT};
use crate::context;
use crate::persist::{self, Persist};
use crate::provider::{Content, Message, Role, StopReason, TextSink, ToolCall};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;

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
             \x1b[2mtype your prompt and press Enter. C-c interrupts a turn. \
             `/quit` or C-d to end the session.\x1b[0m\r\n",
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
}

/// Lines whose start matches one of these labels are dimmed in the PTY
/// (the structured Message event still carries the raw text — this is
/// purely a rendering tweak). Cheap to extend; keep the entries short
/// so the at-start-of-line buffer stays tiny.
const DIM_LINE_PREFIXES: &[&str] = &["Summary:"];

/// Sink for the interactive mode: deltas go directly to the PTY (with
/// optional dim-line styling) and as Message events so the transcript
/// still has the raw text.
struct PtySink<'a> {
    emit: &'a EventEmitter,
    at_line_start: bool,
    in_dim_line: bool,
    /// Buffered chars seen at the start of the current line while we
    /// decide whether they match a `DIM_LINE_PREFIXES` entry. Bounded
    /// by the longest prefix length, so streaming UX stays snappy.
    prefix_buf: String,
}
impl<'a> PtySink<'a> {
    fn new(emit: &'a EventEmitter) -> Self {
        Self {
            emit,
            at_line_start: true,
            in_dim_line: false,
            prefix_buf: String::new(),
        }
    }
}
impl<'a> TextSink for PtySink<'a> {
    fn delta(&mut self, text: &str) {
        let mut out = String::with_capacity(text.len() + 16);
        for c in text.chars() {
            if c == '\n' {
                // End of line: flush any buffered prefix, close dim, CRLF.
                if !self.prefix_buf.is_empty() {
                    out.push_str(&self.prefix_buf);
                    self.prefix_buf.clear();
                }
                if self.in_dim_line {
                    out.push_str("\x1b[0m");
                    self.in_dim_line = false;
                }
                out.push_str("\r\n");
                self.at_line_start = true;
                continue;
            }
            if self.at_line_start && !self.in_dim_line {
                self.prefix_buf.push(c);
                // Did we just complete one of the dim labels?
                if let Some(matched) = DIM_LINE_PREFIXES
                    .iter()
                    .find(|p| **p == self.prefix_buf.as_str())
                {
                    out.push_str("\x1b[2m");
                    out.push_str(matched);
                    self.prefix_buf.clear();
                    self.in_dim_line = true;
                    self.at_line_start = false;
                    continue;
                }
                // Still a prefix of some candidate? keep buffering.
                let still_candidate = DIM_LINE_PREFIXES
                    .iter()
                    .any(|p| p.starts_with(self.prefix_buf.as_str()));
                if !still_candidate {
                    out.push_str(&self.prefix_buf);
                    self.prefix_buf.clear();
                    self.at_line_start = false;
                }
                continue;
            }
            out.push(c);
        }
        if !out.is_empty() {
            self.emit.emit(SessionEvent::pty(out.as_bytes()));
        }
        // Transcript copy stays raw.
        self.emit.emit(SessionEvent::Message {
            role: agentd_protocol::MessageRole::Assistant,
            text: text.to_string(),
        });
    }
}

/// Readline-ish line editor — handles printable chars, cursor
/// navigation (arrows + C-a/C-e/C-b/C-f), history (↑/↓ + C-p/C-n),
/// killing (C-k/C-u/C-w/Backspace/Delete), Enter, C-c, C-d.
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
    /// What we re-emit to redraw the line; produced fresh each frame
    /// from `(prompt_seq, buf, cursor)`.
    prompt_seq: &'static [u8],
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
}

impl LineEditor {
    fn new(prompt_seq: &'static [u8]) -> Self {
        Self {
            buf: String::new(),
            cursor: 0,
            history: Vec::new(),
            hist_pos: None,
            saved: String::new(),
            esc: EscState::Idle,
            prompt_seq,
        }
    }

    /// Bytes the terminal needs to repaint the current line. Caller is
    /// responsible for having the cursor sitting on the prompt's row
    /// before calling (we always start with `\r` + erase-to-end).
    fn redraw(&self) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::with_capacity(self.buf.len() + 32);
        out.extend_from_slice(b"\r\x1b[K");
        out.extend_from_slice(self.prompt_seq);
        out.extend_from_slice(self.buf.as_bytes());
        // Move cursor back by (chars after cursor) cells, if any.
        let tail = self.buf.chars().count().saturating_sub(self.cursor);
        if tail > 0 {
            let mv = format!("\x1b[{tail}D");
            out.extend_from_slice(mv.as_bytes());
        }
        out
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

    fn history_prev(&mut self) {
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
            b'\r' | b'\n' => {
                events.push(self.submit());
                // Drop the prompt's tail; caller draws its own newline +
                // re-prompt after handling the Submit event.
                out.extend_from_slice(b"\r\n");
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
            // Ctrl-P (history prev)
            0x10 => {
                self.history_prev();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-N (history next)
            0x0e => {
                self.history_next();
                out.extend_from_slice(&self.redraw());
            }
            // Ctrl-L — clear screen + redraw.
            0x0c => {
                out.extend_from_slice(b"\x1b[2J\x1b[H");
                out.extend_from_slice(&self.redraw());
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
                // If cursor is at end, optimize: just echo the char.
                // Otherwise full redraw to shift the tail.
                if self.cursor == self.buf.chars().count() {
                    out.push(b);
                } else {
                    out.extend_from_slice(&self.redraw());
                }
            }
        }
    }

    fn handle_csi_final(
        &mut self,
        final_byte: u8,
        params: &str,
        out: &mut Vec<u8>,
        _events: &mut Vec<LineEvent>,
    ) {
        match final_byte {
            b'A' => self.history_prev(),
            b'B' => self.history_next(),
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
        _events: &mut Vec<LineEvent>,
    ) {
        match final_byte {
            b'A' => self.history_prev(),
            b'B' => self.history_next(),
            b'C' => self.move_right(),
            b'D' => self.move_left(),
            b'H' => self.move_home(),
            b'F' => self.move_end(),
            _ => return,
        }
        out.extend_from_slice(&self.redraw());
    }
}

fn char_index_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> LineEditor {
        LineEditor::new(b"> ")
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
    fn ctrl_c_interrupts_and_ctrl_d_eofs() {
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(&[0x03]);
        assert!(matches!(evs.as_slice(), [LineEvent::Interrupt]));
        let mut ed = editor();
        let (_, evs) = ed.feed_bytes(&[0x04]);
        assert!(matches!(evs.as_slice(), [LineEvent::Eof]));
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
    let provider_name = spec.provider_name();
    let model = spec.model.clone();
    let provider = spec.provider;
    let cwd = PathBuf::from(&params.cwd);
    let registry = ToolRegistry::with_defaults();
    let specs = registry.specs();
    let mut automode = std::env::var("AGENTD_ZARVIS_AUTOMODE").as_deref() == Ok("1");

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
    if !resuming {
        term.prompt();
    }

    let tool_ctx = ToolCtx {
        cwd,
        session_id,
        client: tokio::sync::OnceCell::new(),
    };

    // Prompt bytes the line editor will re-emit on every redraw. Must
    // match Terminal::prompt's payload sans the leading `\r\n` (we keep
    // those local to Terminal::prompt because the editor uses bare `\r`
    // for redraw and assumes the prompt sits on a fresh row).
    let mut editor = LineEditor::new(b"\x1b[1;36m\xe2\x9d\xaf \x1b[0m");
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

    'outer: loop {
        // Wait for a user message, either from pending or by typing.
        let user_text = if let Some(t) = pending.pop_front() {
            // Echo the pre-supplied prompt as if the user typed it, so
            // the transcript is faithful.
            term.print(&t);
            term.newline();
            t
        } else {
            emit.emit(SessionEvent::Status {
                state: SessionState::AwaitingInput,
                detail: None,
            });
            match read_one_line(&mut inbox, &mut editor, &term, &mut automode).await {
                ReadOutcome::Line(t) => t,
                ReadOutcome::Stop => break 'outer,
                ReadOutcome::Eof => {
                    term.note("(end of session)");
                    break 'outer;
                }
            }
        };

        // Slash-quit shortcut.
        let trimmed = user_text.trim();
        if trimmed == "/quit" || trimmed == "/exit" {
            term.note("(bye)");
            break;
        }
        if trimmed.is_empty() {
            term.prompt();
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

        // Inner step loop — feed tool results back until end-of-turn.
        loop {
            let _pruned = context::prune(&mut messages, provider_name, &model);
            let mut sink = PtySink::new(&emit);
            let turn = match provider
                .complete(&model, SYSTEM_PROMPT, &messages, &specs, &mut sink)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    term.note(&format!("(provider error: {e})"));
                    emit.emit(SessionEvent::Error { message: format!("{e}") });
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
            for call in turn.tool_calls.iter() {
                let outcome = run_one_tool(
                    call,
                    &registry,
                    &tool_ctx,
                    &emit,
                    &term,
                    &mut inbox,
                    &mut automode,
                )
                .await;
                let outcome = match outcome {
                    Ok(o) => o,
                    Err(reason) => {
                        push_msg!(messages, persist, Message {
                            role: Role::Tool,
                            content: Content::ToolResult {
                                call_id: call.id.clone(),
                                output: format!("(turn aborted: {reason})"),
                                is_error: true,
                            },
                        });
                        if reason == "stop" {
                            return Ok(());
                        }
                        break;
                    }
                };
                let truncated = truncate_for_model(&outcome.output, TOOL_OUTPUT_BUDGET);
                push_msg!(messages, persist, Message {
                    role: Role::Tool,
                    content: Content::ToolResult {
                        call_id: call.id.clone(),
                        output: truncated,
                        is_error: !outcome.ok,
                    },
                });
            }
            if matches!(turn.stop_reason, StopReason::MaxTokens) {
                break;
            }
        }

        term.prompt();
    }
    Ok(())
}

enum ReadOutcome {
    Line(String),
    Stop,
    Eof,
}

async fn read_one_line(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    editor: &mut LineEditor,
    term: &Terminal<'_>,
    automode: &mut bool,
) -> ReadOutcome {
    loop {
        match inbox.recv().await {
            None => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Stop) => return ReadOutcome::Stop,
            Some(AdapterInboxMsg::Interrupt) => {
                editor.buf.clear();
                editor.cursor = 0;
                term.note("(C-c)");
                term.prompt();
            }
            Some(AdapterInboxMsg::Input(t)) => {
                term.print(&t);
                term.newline();
                return ReadOutcome::Line(t);
            }
            Some(AdapterInboxMsg::SetAutoMode(on)) => *automode = on,
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                let (out, events) = editor.feed_bytes(&bytes);
                if !out.is_empty() {
                    term.write(&out);
                }
                for ev in events {
                    match ev {
                        LineEvent::Submit(line) => return ReadOutcome::Line(line),
                        LineEvent::Interrupt => {
                            editor.buf.clear();
                            editor.cursor = 0;
                            term.note("(C-c)");
                            term.prompt();
                        }
                        LineEvent::Eof => return ReadOutcome::Eof,
                    }
                }
            }
            Some(AdapterInboxMsg::PtyResize { .. }) => {}
            Some(AdapterInboxMsg::ToolDecision { .. }) => {}
        }
    }
}

/// Run one tool with approval gating + interrupt support. Mirrors the
/// headless version but renders into the PTY and reads y/n/a from
/// PtyInput when prompting.
async fn run_one_tool(
    call: &ToolCall,
    registry: &ToolRegistry,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    term: &Terminal<'_>,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    automode: &mut bool,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            term.tool_use(&call.name, &serde_json::to_string(&call.input).unwrap_or_default());
            term.tool_result(false, &format!("unknown tool: {}", call.name));
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
    term.tool_use(&call.name, &args_summary);
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
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
                term.tool_result(false, &msg);
                return Ok(ToolOutcome { ok: false, output: msg });
            }
            ApprovalOutcome::Approve => term.print("y\r\n"),
            ApprovalOutcome::Automode => {
                term.print("a\r\n");
                *automode = true;
            }
        }
    }

    let outcome = run_with_interrupt(tool, call.input.clone(), tool_ctx, inbox).await;
    match &outcome {
        Ok(o) => {
            term.tool_result(o.ok, &o.output);
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: o.ok,
                output: o.output.clone(),
            });
        }
        Err(reason) => {
            term.tool_result(false, &format!("({reason})"));
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("({reason})"),
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

async fn run_with_interrupt(
    tool: &dyn crate::tools::Tool,
    input: serde_json::Value,
    ctx: &ToolCtx,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> std::result::Result<ToolOutcome, String> {
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let client_cell = std::sync::Mutex::new(ctx.client.get().cloned());
    let tool_fut = async {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
        };
        if let Some(c) = client_cell.lock().unwrap().clone() {
            let _ = local_ctx.client.set(c);
        }
        tool.run(input, &local_ctx).await
    };
    tokio::select! {
        biased;
        kind = wait_for_interrupt(inbox) => {
            match kind {
                InterruptKind::Stop => Err("stop".into()),
                _ => Err("interrupt".into()),
            }
        }
        res = tool_fut => res.map_err(|e| format!("tool error: {e}")),
    }
}

enum InterruptKind {
    Stop,
    Interrupt,
    Channel,
}

async fn wait_for_interrupt(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> InterruptKind {
    loop {
        match inbox.recv().await {
            None => return InterruptKind::Channel,
            Some(AdapterInboxMsg::Stop) => return InterruptKind::Stop,
            Some(AdapterInboxMsg::Interrupt) => return InterruptKind::Interrupt,
            Some(AdapterInboxMsg::PtyInput(bytes)) => {
                if bytes.contains(&0x03) {
                    return InterruptKind::Interrupt;
                }
            }
            Some(_) => {}
        }
    }
}
