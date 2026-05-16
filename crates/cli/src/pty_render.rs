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

pub struct RenderOutput<'a> {
    /// Borrowed from the `ItemHistory`'s cached parser — lifetime is
    /// tied to the `&mut self` of [`ItemHistory::replay`]. Callers
    /// hand this to `tui_term::PseudoTerminal::new` (or read cells
    /// directly); they never need to own a [`vt100::Parser`].
    pub screen: &'a vt100::Screen,
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
    /// Persistent `vt100::Parser` reused across frames for sessions
    /// without tool blocks (claude / codex / shell) — these never
    /// have items mutate underfoot, so we can process only the
    /// items appended since the last replay and just `set_size` on
    /// resize instead of replaying the full history.
    /// Sessions WITH tool blocks (zarvis) always rebuild because a
    /// block's synth bytes change as state evolves (elapsed counter,
    /// expand/collapse, output arrival).
    cached: Option<CachedParser>,
}

/// Cached parser + the items-count it was last advanced to. The
/// parser also remembers its (cols, rows); on a size mismatch
/// `replay` calls `screen_mut().set_size()` instead of rebuilding.
struct CachedParser {
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
    processed_count: usize,
    /// Length of `pending_chunk` already fed into the parser. Only
    /// non-zero when the cache was populated by a path that decided
    /// to feed pending bytes in-place (currently always 0 because
    /// `replay` flushes pending into a `PtyChunk` item up front for
    /// cached sessions).
    pending_consumed: usize,
    /// Per-item signature for `replay_full` — lets us detect when
    /// an existing item mutated (block hydrated, expanded toggled,
    /// running counter ticked) vs. when the items list was only
    /// appended to. On mutation we rebuild; on append-only we just
    /// process the new tail through the persistent parser.
    /// Empty for the non-tool-block fast path (which doesn't need it).
    signatures: Vec<ItemSig>,
    /// Cumulative visible-line count up through `pending_consumed`.
    /// Lets `replay_full` skip re-counting the whole `pending_chunk`
    /// per frame for block-span row math — we just add the new
    /// tail's visible-line count.
    pending_visible_lines: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemSig {
    Chunk(usize),
    Block {
        call_id: String,
        has_output: bool,
        ok: bool,
        expanded: bool,
        /// `Some(elapsed_sec)` while the block is running (its
        /// status row shows a live counter). `None` once the
        /// block has output — the synth bytes are stable.
        running_elapsed: Option<u64>,
    },
}

impl ItemSig {
    fn of(item: &Item) -> Self {
        match item {
            Item::PtyChunk(b) => ItemSig::Chunk(b.len()),
            Item::ToolBlock(b) => ItemSig::Block {
                call_id: b.call_id.clone(),
                has_output: b.output.is_some(),
                ok: b.ok,
                expanded: b.expanded,
                running_elapsed: if b.output.is_some() {
                    None
                } else {
                    Some(b.started_at.elapsed().as_secs())
                },
            },
        }
    }
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
            cached: None,
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

    /// Render the session's accumulated content into a `vt100::Screen`
    /// at the requested size. Two strategies:
    ///
    /// 1. **Tool-block sessions (zarvis):** rebuild the parser from
    ///    scratch every frame so tool blocks can reflect live state
    ///    (elapsed counters, expand/collapse, in-flight output).
    /// 2. **Non-tool sessions (claude / codex / shell):** keep the
    ///    parser alive across frames. On resize, call `set_size` and
    ///    skip replay — matches what a real terminal does for those
    ///    tools. On streaming, process only the newly-appended items
    ///    instead of the entire history.
    ///
    /// Scrollback offset is applied via `Screen::set_scrollback`
    /// after either path produces the screen.
    pub fn replay(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput<'_> {
        let has_blocks = self
            .items
            .iter()
            .any(|i| matches!(i, Item::ToolBlock(_)));
        if has_blocks {
            self.replay_full(cols, rows, scrollback)
        } else {
            self.replay_cached(cols, rows, scrollback)
        }
    }

    /// Persistent-parser fast path for sessions without tool blocks.
    /// The cache is invalidated on size mismatch via `set_size` (vt100
    /// preserves the grid contents). Items appended since the last
    /// frame are processed in-place; `pending_chunk` is processed
    /// every frame on top of the cache so streaming text shows live.
    fn replay_cached(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput<'_> {
        // Drop a stale cache from an earlier tool-block era.
        let need_reset = match &self.cached {
            None => true,
            Some(_) => false,
        };
        if need_reset {
            self.cached = Some(CachedParser {
                parser: vt100::Parser::new(rows.max(1), cols.max(1), super::app::SCROLLBACK_MAX),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
            });
        }
        let cache = self.cached.as_mut().expect("just populated above");

        // Resize handling has two cases:
        //
        //   - Rows changed but cols didn't: `set_size` is enough.
        //     vt100 keeps the grid; rows are added/removed at the
        //     bottom. No replay, no visible flicker.
        //   - Cols changed: vt100 doesn't reflow soft-wrapped lines,
        //     so just `set_size` leaves prior content at the old
        //     wrap (looks narrow in a wider pane). Rebuild the parser
        //     from the items list so prior content re-wraps at the
        //     new width. Cost is one full replay on this frame; the
        //     cache continues to absorb streaming after.
        if cache.cols != cols {
            cache.parser = vt100::Parser::new(rows.max(1), cols.max(1), super::app::SCROLLBACK_MAX);
            cache.cols = cols;
            cache.rows = rows;
            cache.processed_count = 0;
            cache.pending_consumed = 0;
        } else if cache.rows != rows {
            cache.parser.screen_mut().set_size(rows.max(1), cols.max(1));
            cache.rows = rows;
        }

        // Feed newly-appended items through the live parser.
        if cache.processed_count < self.items.len() {
            for item in &self.items[cache.processed_count..] {
                if let Item::PtyChunk(b) = item {
                    cache.parser.process(b);
                }
                // ToolBlocks can't occur in this path (has_blocks is
                // false), but be defensive.
            }
            cache.processed_count = self.items.len();
        }

        // Feed any newly-arrived pending bytes. Since pending_chunk
        // only grows (or gets flushed wholesale into items via
        // `flush_chunk` on OSC markers, which for non-tool sessions
        // never fire), we just process the suffix beyond what we've
        // already consumed.
        let pending_len = self.pending_chunk.len();
        if pending_len > cache.pending_consumed {
            let suffix = &self.pending_chunk[cache.pending_consumed..];
            cache.parser.process(suffix);
            cache.pending_consumed = pending_len;
        } else if pending_len < cache.pending_consumed {
            // pending_chunk was flushed or shrunk under us — rebuild
            // to stay consistent.
            *cache = CachedParser {
                parser: vt100::Parser::new(rows.max(1), cols.max(1), super::app::SCROLLBACK_MAX),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
            };
            for item in &self.items {
                if let Item::PtyChunk(b) = item {
                    cache.parser.process(b);
                }
            }
            cache.parser.process(&self.pending_chunk);
            cache.processed_count = self.items.len();
            cache.pending_consumed = self.pending_chunk.len();
        }

        cache.parser.screen_mut().set_scrollback(scrollback);
        self.dirty = false;
        RenderOutput {
            screen: cache.parser.screen(),
            blocks: Vec::new(),
        }
    }

    /// Tool-block-aware replay. Reuses the persistent parser when
    /// safe; falls back to a full rebuild only when an item
    /// mid-history actually mutated (block hydrated, expanded
    /// toggled, running counter ticked to a new second). Append-only
    /// changes (new chunks, new blocks at the tail) just feed the
    /// new suffix through the live parser — same shape as
    /// `replay_cached` but with per-block signatures so we know
    /// when invalidation is required.
    fn replay_full(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput<'_> {
        // Per-item signatures: if anything in the prefix mutated,
        // we have to rebuild because vt100 has no "undo bytes" API.
        let current_sigs: Vec<ItemSig> = self.items.iter().map(ItemSig::of).collect();

        let needs_rebuild = match &self.cached {
            None => true,
            Some(c) => {
                c.cols != cols
                    || c.rows != rows
                    || current_sigs.len() < c.signatures.len()
                    || self.pending_chunk.len() < c.pending_consumed
                    || c.signatures
                        .iter()
                        .zip(current_sigs.iter())
                        .any(|(a, b)| a != b)
            }
        };

        if needs_rebuild {
            self.cached = Some(CachedParser {
                parser: vt100::Parser::new(rows.max(1), cols.max(1), super::app::SCROLLBACK_MAX),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
            });
        }
        let cache = self.cached.as_mut().expect("just populated above");

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

        // On rebuild, process every item; on incremental, just the
        // new tail past `processed_count`. We still iterate ALL items
        // so we can compute block-span row positions for hit-testing
        // (those depend on visible-line counts even for items we
        // don't re-feed to the parser).
        let start_processing_at = if needs_rebuild { 0 } else { cache.processed_count };
        for (idx, item) in self.items.iter().enumerate() {
            let start = abs_line;
            match item {
                Item::PtyChunk(b) => {
                    if idx >= start_processing_at {
                        abs_line += count_visible_lines(b, cols);
                        cache.parser.process(b);
                    } else {
                        abs_line += count_visible_lines(b, cols);
                    }
                }
                Item::ToolBlock(block) => {
                    let synth = synth_block(block, cols);
                    let lines = count_visible_lines(&synth.bytes, cols);
                    if idx >= start_processing_at {
                        cache.parser.process(&synth.bytes);
                    }
                    abs_line += lines;
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

        // Pending: feed only the new tail through the parser. The
        // running visible-line count is kept on the cache so we
        // never re-iterate the whole pending buffer (otherwise long
        // sessions pay O(history) per frame just to position block
        // spans).
        if needs_rebuild {
            cache.pending_visible_lines = 0;
        }
        let pending_start = cache.pending_consumed.min(self.pending_chunk.len());
        if pending_start < self.pending_chunk.len() {
            let suffix = &self.pending_chunk[pending_start..];
            cache.parser.process(suffix);
            cache.pending_visible_lines += count_visible_lines(suffix, cols);
        }
        abs_line += cache.pending_visible_lines;

        cache.processed_count = self.items.len();
        cache.pending_consumed = self.pending_chunk.len();
        cache.signatures = current_sigs;

        cache.parser.screen_mut().set_scrollback(scrollback);

        // Map absolute-line ranges to current-frame screen rows.
        let total_lines = abs_line;
        let visible_top = total_lines.saturating_sub(rows as usize + scrollback);
        let mut blocks: Vec<BlockHitRect> = Vec::new();
        for span in block_spans {
            if span.abs_end <= visible_top {
                continue;
            }
            let row_start = span
                .abs_start
                .saturating_sub(visible_top)
                .min(rows as usize) as u16;
            let row_end = span
                .abs_end
                .saturating_sub(visible_top)
                .min(rows as usize) as u16;
            if row_end <= row_start {
                continue;
            }
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
        RenderOutput {
            screen: cache.parser.screen(),
            blocks,
        }
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
        let cell = out.screen.cell(0, 0).map(|c| c.contents()).unwrap_or_default();
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

    /// PERF (suspect): tool-block sessions use `replay_full` — rebuild
    /// the parser from every item every frame so block synth bytes
    /// reflect live state (timers, expand/collapse). This is the
    /// orchestrator panel's path; that panel renders every frame
    /// regardless of which session is selected. If orchestrator
    /// history accumulates lots of bytes + a tool block, every
    /// frame does O(total bytes) work.
    #[test]
    fn replay_full_scales_with_history_size() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        // Force the full-rebuild path by adding a tool block.
        h.feed_tool_use("shell".into(), "ls".into());
        h.feed_pty(b"\x1b]7700;open;call=cX\x07x\x1b]7700;close;call=cX\x07");
        h.feed_tool_result("cX", true, "done".into());

        // Baseline: small history.
        let t0 = Instant::now();
        for _ in 0..100 {
            let _ = h.replay(cols, rows, 0);
        }
        let small_us = t0.elapsed().as_micros();

        // Grow the history: 50,000 lines of realistic chat content.
        let mut grow = Vec::with_capacity(2 * 1024 * 1024);
        for i in 0..50_000u32 {
            grow.extend_from_slice(b"\x1b[33m");
            grow.extend_from_slice(format!("line {i} of accumulated chat ").as_bytes());
            grow.extend_from_slice(b"\x1b[0m\r\n");
        }
        h.feed_pty(&grow);

        // Warm up the cache so we measure steady-state, not the
        // one-time post-growth rebuild.
        let t_warm = Instant::now();
        let _ = h.replay(cols, rows, 0);
        let warmup_us = t_warm.elapsed().as_micros();

        // Steady state: signatures stable, no new bytes — should be
        // essentially free.
        let t1 = Instant::now();
        for _ in 0..100 {
            let _ = h.replay(cols, rows, 0);
        }
        let large_us = t1.elapsed().as_micros();

        eprintln!(
            "replay_full: small {small_us} µs/100, warmup {warmup_us} µs (bootstrap), steady-large {large_us} µs/100 ({}x vs small)",
            large_us.max(1) / small_us.max(1)
        );
        // After warmup, steady state should be cheap (<5ms total for
        // 100 frames). The 3807× cliff this test originally exposed
        // would manifest as >100ms here.
        assert!(
            large_us < 10_000,
            "steady-state replay_full too slow: {large_us} µs/100 frames (warmup was {warmup_us} µs)"
        );
    }

    /// PERF: PseudoTerminal::render iterates cells of the parser's
    /// screen each frame. If a tall pane × wide pane × heavy SGR
    /// content makes that slow, the TUI lags even when nothing's
    /// streaming. Measures wall time for repeated full-pane renders
    /// of a screen populated with realistic content.
    #[test]
    fn pseudo_terminal_render_is_fast_per_frame() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;
        use std::time::Instant;

        let mut h = ItemHistory::new();
        let cols = 200u16;
        let rows = 60u16;
        // Fill a realistic-looking screen: many lines of text + SGR.
        let mut bytes = Vec::with_capacity(200_000);
        for i in 0..2_000u32 {
            bytes.extend_from_slice(b"\x1b[36m");
            bytes.extend_from_slice(format!("Ran git log --oneline -{i}").as_bytes());
            bytes.extend_from_slice(b"\x1b[0m\r\n");
            bytes.extend_from_slice(b"\x1b[90m");
            bytes.extend_from_slice(b"  some captured output that fills the row a bit");
            bytes.extend_from_slice(b"\x1b[0m\r\n");
        }
        h.feed_pty(&bytes);
        let out = h.replay(cols, rows, 0);

        // Render 100 times into a buffer; measure wall time.
        let area = Rect::new(0, 0, cols, rows);
        let t = Instant::now();
        for _ in 0..100 {
            let mut buf = Buffer::empty(area);
            let no_cursor = tui_term::widget::Cursor::default().visibility(false);
            let term = tui_term::widget::PseudoTerminal::new(out.screen).cursor(no_cursor);
            term.render(area, &mut buf);
        }
        let elapsed_us = t.elapsed().as_micros();
        let per_frame_us = elapsed_us / 100;
        eprintln!(
            "PseudoTerminal::render: 100 frames at {cols}x{rows} in {elapsed_us} µs ({per_frame_us} µs/frame)"
        );
        // Should be well under 5 ms/frame.
        assert!(
            per_frame_us < 5_000,
            "PseudoTerminal::render too slow: {per_frame_us} µs/frame"
        );
    }

    /// PERF: many small events (codex's animated progress indicators
    /// can fire dozens per second, each a handful of bytes — repaint
    /// the spinner, erase-and-rewrite the elapsed counter, etc.).
    /// Each event triggers `feed_pty` + a replay. If steady-state
    /// per-event cost climbs as the session ages, the user feels
    /// the "gets slower and slower" pattern.
    #[test]
    fn many_small_events_stay_fast_as_history_grows() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        // Pre-load history so we're measuring "events arriving on
        // top of a long session", not a cold start.
        let preload = vec![b'.'; 500_000];
        h.feed_pty(&preload);
        let _ = h.replay(cols, rows, 0);

        let mut buckets = Vec::new();
        for batch in 0..5 {
            let t = Instant::now();
            for _ in 0..1_000 {
                // 16-byte event — a spinner frame "\r⠋ working..."ish.
                h.feed_pty(b"\r* working...   ");
                let _ = h.replay(cols, rows, 0);
            }
            buckets.push((batch, t.elapsed().as_micros()));
        }
        for (i, us) in &buckets {
            eprintln!("batch {i}: 1000 events in {us} µs ({} ns/event)", us * 1000 / 1000);
        }
        // Sanity: a later batch shouldn't be drastically slower than
        // the first batch — that would indicate per-event cost
        // grows with history.
        let first = buckets.first().unwrap().1;
        let last = buckets.last().unwrap().1;
        assert!(
            last <= first * 3,
            "per-event cost is growing with history: first batch {first} µs, last batch {last} µs"
        );
    }

    /// PERF: a session with a large accumulated PTY history (the
    /// `pty_replay` snapshot a TUI restart loads) should not make
    /// each subsequent small `replay()` proportionally slow. The
    /// `ItemHistory` keeps a persistent parser for non-tool-block
    /// sessions, so steady-state per-frame cost should be bounded
    /// by NEW bytes since the last frame — not by total history.
    /// This test fails (or times out) if the cache is broken.
    #[test]
    fn replay_after_large_history_is_cheap_per_chunk() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        // Bootstrap: 2 MB of plausible PTY content (lines with
        // varying widths + occasional CSI escapes, no OSC 7700 so
        // it lands as one large pending chunk — same shape codex /
        // claude produce).
        let mut bootstrap = Vec::with_capacity(2 * 1024 * 1024);
        for i in 0..50_000u32 {
            bootstrap.extend_from_slice(b"\x1b[33msome line ");
            bootstrap.extend_from_slice(i.to_string().as_bytes());
            bootstrap.extend_from_slice(b" with reasonably long content to wrap\x1b[0m\r\n");
        }
        h.feed_pty(&bootstrap);
        // First replay does the one-time bootstrap processing.
        let t_first = Instant::now();
        let _ = h.replay(cols, rows, 0);
        let first_ms = t_first.elapsed().as_micros();

        // Steady state: 1000 small chunks ("typing" / "streaming"),
        // measured per-chunk.
        let small_chunk = b"x";
        let mut totals = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let t = Instant::now();
            h.feed_pty(small_chunk);
            let _ = h.replay(cols, rows, 0);
            totals.push(t.elapsed().as_micros());
        }
        let max_us = *totals.iter().max().unwrap();
        let avg_us: u128 = totals.iter().sum::<u128>() / totals.len() as u128;

        eprintln!(
            "bootstrap_first_replay_us={first_ms} steady_avg_us={avg_us} steady_max_us={max_us}"
        );

        // After bootstrap, each frame should take well under 1 ms on
        // any reasonable hardware — empirically <50 µs on a 2024
        // laptop. The threshold here is intentionally loose so we
        // don't get flaky CI; the test exists mostly to print the
        // numbers when we suspect the cache regressed.
        assert!(
            avg_us < 2_000,
            "steady-state replay avg too slow: {avg_us} µs/frame after {first_ms} µs bootstrap"
        );
    }
}
