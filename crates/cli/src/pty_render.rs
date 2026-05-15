//! Items-model PTY renderer for ratatui TUI consumers.
//!
//! Two streams flow into every PTY-backed session: raw PTY bytes
//! (chat content, banner, prompt) and structured tool-call events
//! (`ToolUse` + `ToolResult`). The adapter brackets each tool call
//! in the PTY stream with a custom OSC `7700` marker pair so a
//! ratatui client can:
//!
//!   1. **Skip** the original truncated tool-block bytes (everything
//!      between an open and matching close marker), and
//!   2. **Synthesize** its own rendering of the block from the
//!      structured `ToolUse` / `ToolResult` events that arrive in
//!      parallel.
//!
//! The win: each tool block becomes a first-class, height-mutable
//! UI element. Toggling expand/collapse on a block changes its
//! rendered height; the renderer just rebuilds a `vt100::Parser`
//! from the items list with the new height applied. No vt100
//! `IL` / `DL` escape gymnastics, no row tracking on the adapter
//! side, no constraint that the block must still be in the live
//! viewport.
//!
//! Sessions whose adapter never emits OSC `7700` markers (shell,
//! claude, codex, headless zarvis) just produce a single
//! [`Item::PtyChunk`] for the entire stream and render identically
//! to the old direct-parser pipeline.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// One element of the rendered session history.
#[derive(Debug, Clone)]
pub enum Item {
    /// Raw PTY bytes (chat, banner, prompt, assistant streaming
    /// text, slash popup, …). Fed verbatim to the playback parser.
    PtyChunk(Vec<u8>),
    /// A tool call rendered as a discrete, height-mutable block.
    /// Content is filled from structured `ToolUse` / `ToolResult`
    /// events; PTY bytes between the corresponding OSC `7700`
    /// markers are discarded.
    ToolBlock(ToolBlock),
}

#[derive(Debug, Clone)]
pub struct ToolBlock {
    pub call_id: String,
    pub tool: Option<String>,
    pub args_summary: Option<String>,
    pub output: Option<String>,
    pub ok: bool,
    /// Click target on the footer toggles this.
    pub expanded: bool,
    /// TUI-local wall clock for "when did this block first appear?".
    /// Drives the running-tool elapsed counter + the "show buttons
    /// after 15s" affordance. Not synced with the adapter's
    /// supervisor clock; close enough for display.
    pub started_at: Instant,
}

/// Cell-row range a tool block occupies in the current render's
/// visible screen. The mouse click handler hit-tests against this
/// to map a click → call_id for the toggle.
#[derive(Debug, Clone)]
pub struct BlockHitRect {
    pub call_id: String,
    /// Row indices are relative to the rendered area's top — i.e.
    /// directly comparable to `(click_row - panel_inner_y)`.
    pub row_start: u16,
    pub row_end: u16,
    /// `[bg]` button hit zone on the header row (relative to the
    /// rendered area). `None` if the block isn't showing buttons
    /// (already completed, or too young per `BUTTONS_AFTER_MS`).
    pub bg_button: Option<(u16, u16)>,
    /// `[kill]` button hit zone on the header row.
    pub kill_button: Option<(u16, u16)>,
    /// Header row's screen index — both buttons share it.
    pub header_row: u16,
}

pub struct RenderOutput {
    pub parser: vt100::Parser,
    pub blocks: Vec<BlockHitRect>,
}

/// Per-block synthesized output + button geometry. Returned by
/// [`synth_block`] so the replay loop can both feed the parser and
/// record click-targets.
struct SynthOutput {
    bytes: Vec<u8>,
    /// Status row's offset within the block, in *visible rows*. If
    /// `None`, this block doesn't render a status row (typically a
    /// completed block) and no buttons exist for it.
    status_row_offset: Option<u16>,
    /// Button column ranges on the status row. `None` when not
    /// rendered (block is too young, or already completed, etc.).
    bg_button_cols: Option<(u16, u16)>,
    kill_button_cols: Option<(u16, u16)>,
}

/// How long a block must be running before the `[bg]` / `[kill]`
/// buttons appear. Overridable via `AGENTD_TOOL_BUTTONS_AFTER_MS`.
/// Default lowered from 15s to 7s so the affordance is visible
/// before the typical user gives up on the tool. Auto-promote
/// still defaults to 60s — the buttons just announce themselves
/// earlier.
fn buttons_after_ms() -> u64 {
    std::env::var("AGENTD_TOOL_BUTTONS_AFTER_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(7_000)
}

#[derive(Debug, Default)]
enum OscState {
    #[default]
    Normal,
    Escape,
    Body(Vec<u8>),
}

/// Sequential record of everything that's flowed into a session,
/// with tool calls held as expand/collapse-aware blocks rather
/// than baked into the PTY stream.
pub struct ItemHistory {
    items: Vec<Item>,
    /// Bytes accumulating into the in-progress non-block chunk.
    pending_chunk: Vec<u8>,
    osc: OscState,
    /// Tracks whether the byte parser is currently inside an open
    /// block — bytes between markers are dropped, not chunked.
    in_block: bool,
    /// `ToolUse` events queued ahead of their matching OSC open
    /// marker. The parallel-safe path emits ToolUse before OSC
    /// open (events fire as tasks start; OSC fires when the
    /// FuturesOrdered consumer renders), so these wait here for
    /// the marker. Pairing is FIFO — Nth ToolUse attaches to Nth
    /// subsequent OSC open.
    pending_tool_uses: VecDeque<(String, String)>,
    /// Blocks that were created by an OSC open *before* their
    /// matching ToolUse arrived. The risky-tool path emits OSC
    /// open BEFORE the structured ToolUse (so the block exists,
    /// awaiting tool name + args). FIFO of items-indices to
    /// hydrate from incoming ToolUse events.
    pending_block_hydrations: VecDeque<usize>,
    /// `ToolResult` events that arrived before their matching OSC
    /// open. Keyed by call_id (the `tool` field on `ToolResult`
    /// carries call_id by zarvis convention).
    pending_tool_results: HashMap<String, (bool, String)>,
    /// Whether the next [`replay`] should rebuild from scratch.
    /// Set by every mutation; cleared by `replay`. Future-proofing
    /// for an incremental cache.
    pub dirty: bool,
}

/// How many output lines a collapsed tool block shows (must match
/// the zarvis-side cap so the synthesized version mirrors the
/// inline-stream version's information density).
pub const TOOL_BLOCK_COLLAPSED_LINES: usize = 5;
/// Per-line truncation in collapsed mode (mirrors zarvis).
pub const TOOL_BLOCK_MAX_COLS: usize = 200;

impl ItemHistory {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            pending_chunk: Vec::new(),
            osc: OscState::Normal,
            in_block: false,
            pending_tool_uses: VecDeque::new(),
            pending_block_hydrations: VecDeque::new(),
            pending_tool_results: HashMap::new(),
            dirty: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.pending_chunk.is_empty()
    }

    /// Feed a raw PTY byte chunk. Parses inline OSC `7700` markers
    /// out, drops the bytes between paired markers, and accumulates
    /// the rest into `Item::PtyChunk` entries.
    pub fn feed_pty(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
        self.dirty = true;
    }

    fn feed_byte(&mut self, b: u8) {
        match &mut self.osc {
            OscState::Normal => {
                if b == 0x1b {
                    self.osc = OscState::Escape;
                } else if !self.in_block {
                    self.pending_chunk.push(b);
                }
            }
            OscState::Escape => {
                if b == b']' {
                    self.osc = OscState::Body(Vec::new());
                } else {
                    // Not an OSC; restore the dropped ESC + this byte.
                    if !self.in_block {
                        self.pending_chunk.push(0x1b);
                        self.pending_chunk.push(b);
                    }
                    self.osc = OscState::Normal;
                }
            }
            OscState::Body(buf) => {
                if b == 0x07 {
                    let payload = std::mem::take(buf);
                    self.osc = OscState::Normal;
                    self.handle_osc(&payload);
                } else {
                    buf.push(b);
                    // Guard against runaway OSC payloads (shouldn't
                    // happen in practice; this is paranoia).
                    if buf.len() > 1024 {
                        let payload = std::mem::take(buf);
                        self.osc = OscState::Normal;
                        if !self.in_block {
                            self.pending_chunk.extend_from_slice(b"\x1b]");
                            self.pending_chunk.extend_from_slice(&payload);
                        }
                    }
                }
            }
        }
    }

    fn handle_osc(&mut self, payload: &[u8]) {
        let Ok(s) = std::str::from_utf8(payload) else {
            return;
        };
        if let Some(rest) = s.strip_prefix("7700;") {
            if let Some(call_id) = rest.strip_prefix("open;call=") {
                let call_id = call_id.split(';').next().unwrap_or("");
                if !call_id.is_empty() {
                    self.start_block(call_id);
                }
                return;
            }
            if let Some(call_id) = rest.strip_prefix("close;call=") {
                let call_id = call_id.split(';').next().unwrap_or("");
                if !call_id.is_empty() {
                    self.end_block(call_id);
                }
                return;
            }
            // Unknown 7700 variant — ignore.
            return;
        }
        // Some other OSC (e.g. terminal title sets). Pass through to
        // the chunk so the playback parser can do whatever it wants.
        if !self.in_block {
            self.pending_chunk.push(0x1b);
            self.pending_chunk.push(b']');
            self.pending_chunk.extend_from_slice(payload);
            self.pending_chunk.push(0x07);
        }
    }

    fn start_block(&mut self, call_id: &str) {
        // Flush any chat bytes accumulated so far so the new block
        // sits at the right point in the items sequence.
        self.flush_chunk();
        let mut block = ToolBlock {
            call_id: call_id.to_string(),
            tool: None,
            args_summary: None,
            output: None,
            ok: true,
            expanded: false,
            started_at: Instant::now(),
        };
        // Bidirectional pairing — works for both orderings:
        //  - Parallel-safe path: ToolUse arrives before OSC open;
        //    pending_tool_uses has it queued, pop FIFO.
        //  - Risky path: OSC open arrives before ToolUse; record
        //    this block's index in `pending_block_hydrations` so
        //    the next ToolUse can fill it in.
        if let Some((tool, summary)) = self.pending_tool_uses.pop_front() {
            block.tool = Some(tool);
            block.args_summary = Some(summary);
        } else {
            self.pending_block_hydrations.push_back(self.items.len());
        }
        if let Some((ok, output)) = self.pending_tool_results.remove(call_id) {
            block.output = Some(output);
            block.ok = ok;
        }
        self.items.push(Item::ToolBlock(block));
        self.in_block = true;
    }

    fn end_block(&mut self, _call_id: &str) {
        // Bytes between open and close were dropped on the floor
        // (the synthesized block replaces them entirely). Nothing
        // more to record here.
        self.in_block = false;
    }

    fn flush_chunk(&mut self) {
        if !self.pending_chunk.is_empty() {
            let bytes = std::mem::take(&mut self.pending_chunk);
            self.items.push(Item::PtyChunk(bytes));
        }
    }

    /// Apply a `ToolUse` event. The protocol's `ToolUse` carries no
    /// `call_id`, so pairing is FIFO. If an earlier OSC-open or
    /// `TaskStart` already created a block awaiting hydration,
    /// fill it in directly. Otherwise queue for the next OSC-open
    /// (legacy path).
    pub fn feed_tool_use(&mut self, tool: String, args_summary: String) {
        if let Some(idx) = self.pending_block_hydrations.pop_front() {
            if let Some(Item::ToolBlock(b)) = self.items.get_mut(idx) {
                b.tool = Some(tool);
                b.args_summary = Some(args_summary);
                self.dirty = true;
                return;
            }
        }
        self.pending_tool_uses.push_back((tool, args_summary));
        self.dirty = true;
    }

    /// Apply a `TaskStart` event. Unlike `ToolUse`, this carries an
    /// explicit `call_id` — so the block is created and hydrated
    /// in a single step. This is the primary block-creation path
    /// for new zarvis sessions; the OSC-fence path remains as a
    /// backstop for byte streams loaded from older `pty.log`
    /// files that still contain inline fenced tool blocks.
    pub fn feed_task_start(
        &mut self,
        call_id: String,
        tool: String,
        args_summary: String,
    ) {
        // Idempotent: if a block already exists for this call_id
        // (e.g., the OSC backstop fired first on a legacy log),
        // just hydrate it.
        if let Some(Item::ToolBlock(b)) = self
            .items
            .iter_mut()
            .find(|it| matches!(it, Item::ToolBlock(b) if b.call_id == call_id))
        {
            b.tool = Some(tool);
            b.args_summary = Some(args_summary);
            self.dirty = true;
            return;
        }
        // Flush current chunk so the new block sits at the right
        // point in the items sequence.
        self.flush_chunk();
        let mut block = ToolBlock {
            call_id: call_id.clone(),
            tool: Some(tool),
            args_summary: Some(args_summary),
            output: None,
            ok: true,
            expanded: false,
            started_at: Instant::now(),
        };
        if let Some((ok, output)) = self.pending_tool_results.remove(&call_id) {
            block.output = Some(output);
            block.ok = ok;
        }
        self.items.push(Item::ToolBlock(block));
        self.dirty = true;
    }

    /// Apply a `ToolResult` event. The protocol's `tool` field
    /// carries the *call_id* (zarvis convention), so we can match
    /// directly. If the OSC open hasn't arrived yet, stash for later.
    pub fn feed_tool_result(&mut self, call_id: &str, ok: bool, output: String) {
        if let Some(block) = self.find_block_mut(call_id) {
            block.output = Some(output);
            block.ok = ok;
        } else {
            self.pending_tool_results
                .insert(call_id.to_string(), (ok, output));
        }
        self.dirty = true;
    }

    fn find_block_mut(&mut self, call_id: &str) -> Option<&mut ToolBlock> {
        self.items.iter_mut().rev().find_map(|it| match it {
            Item::ToolBlock(b) if b.call_id == call_id => Some(b),
            _ => None,
        })
    }

    /// Toggle the expand state of a block. Returns true when a
    /// matching block was found (and toggled), false otherwise.
    pub fn toggle_block(&mut self, call_id: &str) -> bool {
        if let Some(block) = self.find_block_mut(call_id) {
            block.expanded = !block.expanded;
            self.dirty = true;
            true
        } else {
            false
        }
    }

    /// Build a fresh `vt100::Parser`, replay every item into it at
    /// the requested size, and return both the parser (for tui-term
    /// rendering) and the visible block ranges (for hit-testing).
    ///
    /// Scrollback offset is applied via `Parser::screen_mut().set_scrollback`
    /// AFTER the replay — same as the old direct-parser pipeline.
    pub fn replay(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput {
        // Flush a copy of the in-progress chunk so it shows up in
        // the rendered output without disturbing the appendable
        // state for future feeds.
        let mut parser = vt100::Parser::new(rows.max(1), cols.max(1), super::app::SCROLLBACK_MAX);
        struct BlockSpan {
            call_id: String,
            abs_start: usize,
            abs_end: usize,
            status_abs_row: Option<usize>,
            bg_cols: Option<(u16, u16)>,
            kill_cols: Option<(u16, u16)>,
        }
        let mut block_spans: Vec<BlockSpan> = Vec::new();
        let mut abs_line: usize = 0;

        let mut process = |parser: &mut vt100::Parser,
                           bytes: &[u8],
                           cols: u16,
                           abs_line: &mut usize| {
            *abs_line += count_visible_lines(bytes, cols);
            parser.process(bytes);
        };

        for item in &self.items {
            let start = abs_line;
            match item {
                Item::PtyChunk(b) => process(&mut parser, b, cols, &mut abs_line),
                Item::ToolBlock(block) => {
                    let synth = synth_block(block, cols);
                    process(&mut parser, &synth.bytes, cols, &mut abs_line);
                    let status_abs_row = synth
                        .status_row_offset
                        .map(|off| start + off as usize);
                    block_spans.push(BlockSpan {
                        call_id: block.call_id.clone(),
                        abs_start: start,
                        abs_end: abs_line,
                        status_abs_row,
                        bg_cols: synth.bg_button_cols,
                        kill_cols: synth.kill_button_cols,
                    });
                }
            }
        }
        // Any bytes that haven't formed a complete chunk yet — feed
        // them through too so the user sees streaming text live.
        if !self.pending_chunk.is_empty() {
            process(&mut parser, &self.pending_chunk, cols, &mut abs_line);
        }

        parser.screen_mut().set_scrollback(scrollback);

        // Map absolute-line ranges to current-frame screen rows.
        let total_lines = abs_line;
        let visible_top = total_lines
            .saturating_sub(rows as usize + scrollback);
        let mut blocks: Vec<BlockHitRect> = Vec::new();
        for span in block_spans {
            if span.abs_end <= visible_top {
                continue;
            }
            let row_start = span.abs_start
                .saturating_sub(visible_top)
                .min(rows as usize) as u16;
            let row_end = span.abs_end
                .saturating_sub(visible_top)
                .min(rows as usize) as u16;
            if row_end <= row_start {
                continue;
            }
            // Map status row's abs position → screen row. Skip
            // buttons that fall outside the visible window.
            let mut header_row = row_start;
            let mut bg_button = None;
            let mut kill_button = None;
            if let Some(abs_row) = span.status_abs_row {
                if abs_row >= visible_top && abs_row < visible_top + rows as usize {
                    header_row = (abs_row - visible_top) as u16;
                    bg_button = span.bg_cols;
                    kill_button = span.kill_cols;
                }
            }
            blocks.push(BlockHitRect {
                call_id: span.call_id,
                row_start,
                row_end,
                bg_button,
                kill_button,
                header_row,
            });
        }

        self.dirty = false;
        RenderOutput { parser, blocks }
    }
}

impl Default for ItemHistory {
    fn default() -> Self {
        Self::new()
    }
}

/// Approximate "visible row" count for a byte slice — counts `\n`
/// plus soft-wrap rows when a logical line exceeds `cols` printable
/// chars. ANSI CSI escapes (`\x1b[...<final>`), OSC (`\x1b]...\x07`),
/// and SS3 (`\x1b O <one byte>`) sequences are excluded from
/// the visible-column count.
///
/// Approximate: width is char-count, not display-width; CJK chars
/// undercount; combining marks overcount. Good enough for click
/// hit-testing — off-by-one is acceptable since blocks span
/// multiple rows.
fn count_visible_lines(bytes: &[u8], cols: u16) -> usize {
    let mut lines = 0usize;
    let mut col = 0usize;
    let cols = cols.max(1) as usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        i += 1;
        if b == 0x1b {
            // Skip CSI / OSC / SS3 / single-char ESC sequences.
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
                    // CSI: digits / ; / ? / final byte in 0x40..=0x7e.
                    i += 1;
                    while i < bytes.len() {
                        let c = bytes[i];
                        i += 1;
                        if (0x40..=0x7e).contains(&c) {
                            break;
                        }
                    }
                }
                b']' => {
                    // OSC: until BEL (0x07) or ST (\x1b\\).
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        i += 1;
                    }
                }
                b'O' => {
                    i += 2; // SS3 + one final byte
                }
                _ => i += 1,
            }
            continue;
        }
        match b {
            b'\n' => {
                lines += 1;
                col = 0;
            }
            b'\r' => col = 0,
            // Other control chars: ignore for width.
            0x00..=0x08 | 0x0b..=0x1f | 0x7f => {}
            _ => {
                col += 1;
                if col >= cols {
                    lines += 1;
                    col = 0;
                }
            }
        }
    }
    lines
}

/// Build the byte sequence representing a tool block at its current
/// state. Matches the visual idiom zarvis writes inline so
/// non-ratatui consumers and the items-model render stay coherent.
///
/// States rendered:
/// - **Running** (`output == None`): header + status row with
///   elapsed counter; `[bg]` and `[kill]` buttons appear after the
///   `BUTTONS_AFTER_MS` threshold.
/// - **Backgrounded** (`output == BG_PLACEHOLDER_OUTPUT`): header +
///   status row with "in background"; `[kill]` button only.
/// - **Completed** (`output != None` and non-placeholder): header
///   + glyph + truncated body + optional expand/collapse footer.
fn synth_block(block: &ToolBlock, cols: u16) -> SynthOutput {
    /// Placeholder string the zarvis adapter writes into a tool's
    /// `output` when it auto-backgrounds. Kept in sync via the
    /// `agentd_adapter_zarvis::tasks::BG_PLACEHOLDER_OUTPUT`
    /// constant — duplicated here only to avoid a cross-crate
    /// dep just for one string.
    const BG_PLACEHOLDER_OUTPUT: &str =
        "(running in background; will report when complete)";

    let mut out: Vec<u8> = Vec::with_capacity(128);
    // Leading blank — separates the block from prior chat content
    // (mirrors zarvis's `\r\n→ ...` line layout). This blank takes
    // one visible row, then the header.
    out.extend_from_slice(b"\r\n");

    let tool = block.tool.as_deref().unwrap_or("?");
    let args = block.args_summary.as_deref().unwrap_or("");
    let header = format!(
        "\x1b[1;33m→ {tool}\x1b[0m\x1b[2m({args})\x1b[0m\r\n"
    );
    out.extend_from_slice(header.as_bytes());
    // After the leading `\r\n` + header, we've emitted 2 rows.
    // status_row_offset will be 2 if we render a status row next.

    // Classify by output content.
    let output_opt = block.output.as_deref();
    let is_running = output_opt.is_none();
    let is_backgrounded =
        output_opt.map(|o| o == BG_PLACEHOLDER_OUTPUT).unwrap_or(false);
    let is_completed = !is_running && !is_backgrounded;

    let mut status_row_offset: Option<u16> = None;
    let mut bg_button_cols: Option<(u16, u16)> = None;
    let mut kill_button_cols: Option<(u16, u16)> = None;

    if is_running || is_backgrounded {
        status_row_offset = Some(2);
        let elapsed_secs = block.started_at.elapsed().as_secs();
        let buttons_ready = block.started_at.elapsed().as_millis() as u64
            >= buttons_after_ms();

        let show_bg_button = is_running && buttons_ready;
        let show_kill_button = (is_running || is_backgrounded) && buttons_ready;

        // New status-row shape — buttons FIRST at fixed columns,
        // status text after. This decouples the click target's
        // column from variable-width content (elapsed seconds,
        // tool name, Unicode glyph width differences). Stable
        // hit-test:
        //
        //   "  [bg] [kill] running 8s"
        //    01234567890123456789
        //
        // [bg]   → cols 2..6  (4 cells: '[', 'b', 'g', ']')
        // [kill] → cols 7..13 (6 cells: '[', 'k', 'i', 'l', 'l', ']')
        //
        // ASCII only. No `↻` — different terminals disagree on its
        // cell width and one off-by-one breaks click registration.
        let mut line = String::new();
        line.push_str("  "); // 2-cell left margin (cols 0,1)
        if show_bg_button {
            // "[bg]" — 4 cells from col 2 to col 6 (exclusive).
            line.push_str("\x1b[2m[\x1b[0m\x1b[1mbg\x1b[0m\x1b[2m]\x1b[0m");
            bg_button_cols = Some((2, 6));
        } else {
            // Pad with 4 spaces so the kill button stays at col 7.
            line.push_str("    ");
        }
        line.push(' '); // separator col 6
        if show_kill_button {
            line.push_str("\x1b[2m[\x1b[0m\x1b[1;31mkill\x1b[0m\x1b[2m]\x1b[0m");
            kill_button_cols = Some((7, 13));
        } else {
            line.push_str("      ");
        }
        line.push(' '); // separator col 13
        let status_text = if is_running {
            format!("running {elapsed_secs}s")
        } else {
            format!("in background {elapsed_secs}s")
        };
        line.push_str(&format!("\x1b[2;33m{status_text}\x1b[0m"));
        if is_running && buttons_ready {
            // Hint that the queue is live during a tool run. Same
            // dim styling as the elapsed counter so it reads as
            // metadata, not active output.
            line.push_str(
                "  \x1b[2m· type to queue next prompt\x1b[0m",
            );
        }
        line.push_str("\r\n");
        out.extend_from_slice(line.as_bytes());
    }

    if is_completed {
        let glyph = if block.ok {
            "\x1b[1;32m✓\x1b[0m"
        } else {
            "\x1b[1;31m✗\x1b[0m"
        };
        let output = output_opt.unwrap_or("");
        let total_lines = output.lines().count();
        let visible = if block.expanded {
            total_lines
        } else {
            TOOL_BLOCK_COLLAPSED_LINES.min(total_lines)
        };
        let max_col = (cols as usize)
            .saturating_sub(7)
            .min(TOOL_BLOCK_MAX_COLS)
            .max(8);

        if total_lines == 0 {
            let payload = format!("  {glyph}  \x1b[2m(no output)\x1b[0m\r\n");
            out.extend_from_slice(payload.as_bytes());
        } else {
            for (i, line) in output.lines().take(visible).enumerate() {
                let trimmed: String = line.chars().take(max_col).collect();
                if i == 0 {
                    let payload =
                        format!("  {glyph}  \x1b[2m{trimmed}\x1b[0m\r\n");
                    out.extend_from_slice(payload.as_bytes());
                } else {
                    let payload = format!("     \x1b[2m{trimmed}\x1b[0m\r\n");
                    out.extend_from_slice(payload.as_bytes());
                }
            }
        }

        if total_lines > 0 {
            if block.expanded && total_lines > TOOL_BLOCK_COLLAPSED_LINES {
                let footer =
                    "     \x1b[2;36m[click to collapse]\x1b[0m\r\n".to_string();
                out.extend_from_slice(footer.as_bytes());
            } else if !block.expanded && total_lines > TOOL_BLOCK_COLLAPSED_LINES {
                let remaining = total_lines - TOOL_BLOCK_COLLAPSED_LINES;
                let footer = format!(
                    "     \x1b[2;36m[+{remaining} lines — click to expand]\x1b[0m\r\n"
                );
                out.extend_from_slice(footer.as_bytes());
            }
        }
    }

    SynthOutput {
        bytes: out,
        status_row_offset,
        bg_button_cols,
        kill_button_cols,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_count(h: &ItemHistory) -> usize {
        h.items
            .iter()
            .filter(|it| matches!(it, Item::ToolBlock(_)))
            .count()
    }

    #[test]
    fn no_osc_yields_one_chunk_on_replay() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"hello\r\nworld\r\n");
        // No blocks parsed.
        assert_eq!(block_count(&h), 0);
        let out = h.replay(40, 10, 0);
        let cell = out.parser.screen().cell(0, 0).map(|c| c.contents()).unwrap_or_default();
        assert_eq!(cell, "h");
    }

    #[test]
    fn osc_open_close_creates_block() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"pre");
        // Block bytes are between markers.
        h.feed_pty(b"\x1b]7700;open;call=abc\x07");
        h.feed_pty(b"  inline truncated text\r\n");
        h.feed_pty(b"\x1b]7700;close;call=abc\x07");
        h.feed_pty(b"post");
        // pre + block + (post in pending)
        assert_eq!(block_count(&h), 1);
        // The bytes between markers were dropped.
        match &h.items[0] {
            Item::PtyChunk(b) => assert_eq!(b, b"pre"),
            _ => panic!(),
        }
        match &h.items[1] {
            Item::ToolBlock(b) => assert_eq!(b.call_id, "abc"),
            _ => panic!(),
        }
    }

    #[test]
    fn tool_events_before_marker_are_buffered() {
        let mut h = ItemHistory::new();
        // Events arrive first (in zarvis's emit order).
        h.feed_tool_use("shell".into(), "ls /tmp".into());
        h.feed_tool_result("c1", true, "file_a\nfile_b\nfile_c".into());
        // Then the PTY marker.
        h.feed_pty(b"\x1b]7700;open;call=c1\x07inline\x1b]7700;close;call=c1\x07");
        // The block was hydrated by FIFO-popping the ToolUse and
        // by call_id-matching the ToolResult.
        match &h.items[0] {
            Item::ToolBlock(b) => {
                assert_eq!(b.call_id, "c1");
                assert_eq!(b.tool.as_deref(), Some("shell"));
                assert_eq!(b.args_summary.as_deref(), Some("ls /tmp"));
                assert!(b.output.is_some());
                assert!(b.ok);
            }
            _ => panic!(),
        }
        assert!(h.pending_tool_uses.is_empty());
        assert!(h.pending_tool_results.is_empty());
    }

    #[test]
    fn osc_before_tool_use_still_pairs() {
        // Risky-tool ordering: OSC open fires first, then ToolUse,
        // then ToolResult (with body bytes in between, dropped).
        let mut h = ItemHistory::new();
        h.feed_pty(b"\x1b]7700;open;call=R1\x07");
        h.feed_tool_use("shell".into(), "echo hi".into());
        h.feed_tool_result("R1", true, "hi".into());
        h.feed_pty(b"\x1b]7700;close;call=R1\x07");
        match &h.items[0] {
            Item::ToolBlock(b) => {
                assert_eq!(b.tool.as_deref(), Some("shell"));
                assert_eq!(b.args_summary.as_deref(), Some("echo hi"));
                assert_eq!(b.output.as_deref(), Some("hi"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn fifo_pairing_handles_parallel_calls() {
        let mut h = ItemHistory::new();
        // Two ToolUse arrive before any markers, in call order.
        h.feed_tool_use("shell".into(), "ls".into());
        h.feed_tool_use("read_file".into(), "src/foo.rs".into());
        // Markers arrive in same order.
        h.feed_pty(b"\x1b]7700;open;call=A\x07x\x1b]7700;close;call=A\x07");
        h.feed_pty(b"\x1b]7700;open;call=B\x07y\x1b]7700;close;call=B\x07");
        let block_a = h.items.iter().find_map(|i| match i {
            Item::ToolBlock(b) if b.call_id == "A" => Some(b),
            _ => None,
        }).unwrap();
        let block_b = h.items.iter().find_map(|i| match i {
            Item::ToolBlock(b) if b.call_id == "B" => Some(b),
            _ => None,
        }).unwrap();
        assert_eq!(block_a.tool.as_deref(), Some("shell"));
        assert_eq!(block_b.tool.as_deref(), Some("read_file"));
    }

    #[test]
    fn toggle_block_expands_height() {
        let mut h = ItemHistory::new();
        let big_output = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.feed_tool_use("shell".into(), "ls".into());
        h.feed_tool_result("c1", true, big_output);
        h.feed_pty(b"\x1b]7700;open;call=c1\x07x\x1b]7700;close;call=c1\x07");
        let out1 = h.replay(40, 80, 0);
        let collapsed_rows = out1.blocks[0].row_end - out1.blocks[0].row_start;
        assert!(h.toggle_block("c1"));
        let out2 = h.replay(40, 80, 0);
        let expanded_rows = out2.blocks[0].row_end - out2.blocks[0].row_start;
        assert!(
            expanded_rows > collapsed_rows,
            "expand should grow the row range: collapsed={collapsed_rows} expanded={expanded_rows}"
        );
    }

    #[test]
    fn osc_split_across_feeds_still_parses() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"\x1b]7700;open;ca");
        h.feed_pty(b"ll=z\x07inside\x1b]7700;close;call=z\x07");
        assert_eq!(block_count(&h), 1);
    }

    #[test]
    fn count_visible_lines_handles_wrap() {
        // 80 cols, single line of 200 chars → 3 visible rows
        // (200 / 80 = 2.5, so 2 wraps → 3 rows? actually it's
        // the wrap algorithm in count_visible_lines: every cols
        // chars increments lines).
        let bytes: Vec<u8> = std::iter::repeat(b'a').take(200).collect();
        let n = count_visible_lines(&bytes, 80);
        // Exactly 200 / 80 = 2 wraps (one at col 80, another at 160).
        assert_eq!(n, 2);
    }

    #[test]
    fn count_visible_lines_ignores_csi() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[1;31m"); // SGR
        bytes.extend_from_slice(b"hello\n");
        bytes.extend_from_slice(b"\x1b[0m"); // reset
        let n = count_visible_lines(&bytes, 80);
        assert_eq!(n, 1);
    }
}
