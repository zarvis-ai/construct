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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Kind of a structured chat [`Item::Message`], driving its styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// A user prompt (gray `❯` prefix).
    User,
    /// Assistant prose (default style).
    Assistant,
    /// Model reasoning trace (dim).
    Reasoning,
}

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
    /// A structured chat message from a session that emits no PTY for
    /// its conversation (headless harnesses). Rendered as synthesized,
    /// role-styled text. Streaming deltas arrive as many consecutive
    /// `Message` items of the same kind; only the first of a run sets
    /// `break_before`, so a run renders as one continuous block.
    Message {
        kind: MessageKind,
        text: String,
        break_before: bool,
    },
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
    /// Drives the running-tool elapsed counter + the "show controls
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
    /// Legacy `[bg]` button hit zone on the header row (relative to
    /// the rendered area). New renders use keyboard hints instead,
    /// so this is normally `None`.
    pub bg_button: Option<(u16, u16)>,
    /// Legacy `[kill]` button hit zone on the header row.
    pub kill_button: Option<(u16, u16)>,
    /// Header row's screen index.
    pub header_row: u16,
}

pub struct RenderOutput<'a> {
    /// Borrowed from the `ItemHistory`'s cached parser — lifetime is
    /// tied to the `&mut self` of [`ItemHistory::replay`]. Callers
    /// hand this to `tui_term::PseudoTerminal::new` (or read cells
    /// directly); they never need to own a [`vt100::Parser`].
    pub screen: &'a vt100::Screen,
    pub blocks: Vec<BlockHitRect>,
    /// Maximum scrollback offset accepted by the parser for this render.
    /// The visible `screen.scrollback()` is the current viewport; this value
    /// represents the full scrollable history extent for overlay scrollbars.
    pub max_scrollback: usize,
}

/// Per-block synthesized output + optional control geometry. Returned by
/// [`synth_block`] so the replay loop can both feed the parser and
/// record click-targets.
struct SynthOutput {
    bytes: Vec<u8>,
    /// Status row's offset within the block, in *visible rows*. If
    /// `None`, this block doesn't render a status row (typically a
    /// completed block) and no buttons exist for it.
    status_row_offset: Option<u16>,
    /// Legacy button column ranges on the status row. New renders use
    /// keyboard hints, so these remain `None`.
    bg_button_cols: Option<(u16, u16)>,
    kill_button_cols: Option<(u16, u16)>,
}

/// How long a block must be running before the keyboard-control hint
/// appears. Overridable via `AGENTD_TOOL_BUTTONS_AFTER_MS`.
/// Default lowered from 15s to 7s so the affordance is visible
/// before the typical user gives up on the tool. Auto-promote
/// still defaults to 60s — the hint just announces itself
/// earlier.
fn buttons_after_ms() -> u64 {
    std::env::var("AGENTD_TOOL_BUTTONS_AFTER_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(7_000)
}

fn tool_block_controls_ready(block: &ToolBlock) -> bool {
    block.started_at.elapsed().as_millis() as u64 >= buttons_after_ms()
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
    /// Shadow parser used exclusively to back mouse scrollback.
    ///
    /// The main parser (`cached.parser` in the cached path,
    /// rebuilt-per-frame in the full path) renders the *live*
    /// viewport faithfully — including alt-screen mode and
    /// whatever in-place redraws the child does. Neither path
    /// fills vt100's scrollback buffer for those harnesses, so
    /// mouse-wheel scroll-up has nothing to show.
    ///
    /// The shadow parser sees a *filtered* version of the PTY
    /// stream: alt-screen toggle escapes (`\x1b[?1049/1047/47
    /// h/l`) and bytes between an enter and exit are skipped, so
    /// the shadow stays in normal-screen mode and accumulates
    /// natural `\r\n`-scrolled content from before/after the
    /// alt-screen window. When the caller passes `scrollback > 0`
    /// to `replay`, we render from the shadow's screen instead
    /// of the main parser.
    shadow_parser: vt100::Parser,
    shadow_in_alt_screen: bool,
    shadow_last_snapshot: Vec<ShadowSnapshotLine>,
    shadow_dirty_since_snapshot: bool,
    shadow_snapshot_worthy_since_snapshot: bool,
    /// Last cols/rows the shadow was sized to, so we only call
    /// `set_size` when the dims actually changed (matches the
    /// main-parser caching idiom).
    shadow_cols: u16,
    shadow_rows: u16,
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
    /// Whether the next [`replay`] should account for a mutation.
    /// Set by every mutation; cleared by `replay`.
    pub dirty: bool,
    /// Persistent `vt100::Parser` reused across frames for sessions
    /// without tool blocks (claude / codex / shell) — these never
    /// have items mutate underfoot, so we can process only the
    /// items appended since the last replay and just `set_size` on
    /// resize instead of replaying the full history.
    /// Sessions with synthesized items (zarvis tool blocks and
    /// headless messages) keep signatures so unchanged frames reuse
    /// the parser and only mutations rebuild.
    cached: Option<CachedParser>,
}

/// Cached parser + the items-count it was last advanced to. The
/// parser also remembers its (cols, rows); on a size mismatch
/// `replay` calls `screen_mut().set_size()` instead of rebuilding.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ShadowSnapshotLine {
    Text(String),
    Blank,
}

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
    /// running controls appeared) vs. when the items list was only
    /// appended to. On mutation we rebuild; on append-only we just
    /// process the new tail through the persistent parser.
    /// Empty for the non-tool-block fast path (which doesn't need it).
    signatures: Vec<ItemSig>,
    /// Cumulative visible-line count up through `pending_consumed`.
    /// Lets `replay_full` skip re-counting the whole `pending_chunk`
    /// per frame for block-span row math — we just add the new
    /// tail's visible-line count.
    pending_visible_lines: usize,
    /// Cursor column after the pending bytes already accounted for
    /// by `pending_visible_lines`.
    pending_end_col: usize,
    /// Per-item rendered layout for `replay_full` — the visible-line
    /// count and (for tool blocks) the hit-rect metadata. Parallel to
    /// `self.items`. Lets steady-state frames skip the O(history)
    /// `count_visible_lines` / `synth_block` re-scan of every item:
    /// the unchanged prefix is reused, only new items are computed.
    /// Valid only while `cols` is unchanged (rebuild clears it).
    item_layouts: Vec<ItemLayout>,
}

/// Cached per-item render layout used by `replay_full`. Depends only
/// on the item contents and `cols` (not `rows`), so it survives every
/// frame that doesn't change column width or mutate an item.
#[derive(Clone)]
struct ItemLayout {
    /// Visible row count this item contributes (incl. soft wraps).
    lines: usize,
    /// Cursor column after this item renders. Line counting needs this
    /// because a later item may start on a partially-filled row.
    end_col: usize,
    /// Tool-block hit-rect metadata, empty for plain PTY chunks.
    blocks: Vec<BlockLayout>,
}

#[derive(Clone)]
struct BlockLayout {
    call_id: String,
    /// First visible row of the clickable region relative to the item.
    row_start_offset: usize,
    /// Exclusive end row of the clickable region relative to the item.
    row_end_offset: usize,
    /// Offset (in visible rows) of the block's status row from the
    /// block's first row; `None` if the block has no status row.
    status_row_offset: Option<usize>,
    bg_cols: Option<(u16, u16)>,
    kill_cols: Option<(u16, u16)>,
}

fn suffix_rebuild_start(layouts: &[ItemLayout], changed_idx: usize, rows: u16) -> usize {
    let budget = reflow_budget_lines(rows);
    let mut retained_lines = 0usize;
    let mut start = layouts.len();
    while start > 0 && retained_lines < budget {
        start -= 1;
        retained_lines += layouts[start].lines;
    }
    start.min(changed_idx)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ItemSig {
    Chunk(usize),
    Block {
        call_id: String,
        has_output: bool,
        ok: bool,
        expanded: bool,
        /// `Some(controls_ready)` while the block is running. The
        /// status row stays stable until the control hint appears;
        /// a per-second elapsed counter forced full parser replays.
        running_controls_ready: Option<bool>,
    },
    /// Message items are immutable once pushed (streaming appends new
    /// items rather than growing one), so the text length + kind fully
    /// identify the synthesized bytes.
    Msg {
        kind: MessageKind,
        len: usize,
        break_before: bool,
    },
}

impl ItemSig {
    fn of(item: &Item) -> Self {
        match item {
            Item::PtyChunk(b) => ItemSig::Chunk(b.len()),
            Item::ToolBlock(b) => block_sig(b),
            Item::Message {
                kind,
                text,
                break_before,
            } => ItemSig::Msg {
                kind: *kind,
                len: text.len(),
                break_before: *break_before,
            },
        }
    }
}

fn block_sig(b: &ToolBlock) -> ItemSig {
    ItemSig::Block {
        call_id: b.call_id.clone(),
        has_output: b.output.is_some(),
        ok: b.ok,
        expanded: b.expanded,
        running_controls_ready: if b.output.is_some() {
            None
        } else {
            Some(tool_block_controls_ready(b))
        },
    }
}

/// How many output lines a collapsed tool block shows (must match
/// the zarvis-side cap so the synthesized version mirrors the
/// inline-stream version's information density).
/// Minimum geometry we'll feed into `vt100::Parser`. The crate
/// (0.16.2) underflows in `grid.rs::col_wrap` when rows or cols is
/// 1 and a wide character wraps — `prev_pos.row -= scrolled` goes
/// negative because the cursor is already at row 0. The 2×2 floor
/// is the smallest size that exercises only the safe code paths.
/// Real PTYs are never this small, but the orchestrator panel
/// shrinks its chat area to 1 row when the editor pane absorbs
/// most of a narrow panel, and `/remote-control`'s C-x x trip
/// exposed it. Removing this floor requires either a vt100
/// upstream fix or switching parsers.
pub const VT100_MIN_DIM: u16 = 2;

pub const TOOL_BLOCK_COLLAPSED_LINES: usize = 5;
pub const TOOL_BLOCK_EXPANDED_LINES: usize = 240;
/// Per-line truncation in collapsed mode (mirrors zarvis).
pub const TOOL_BLOCK_MAX_COLS: usize = 200;
const TOOL_BLOCK_HISTORY_LINE_CHARS: usize = TOOL_BLOCK_MAX_COLS * 2;

pub fn tool_output_preview_for_history(output: &str) -> String {
    let mut preview = String::new();
    let mut truncated = false;
    for (idx, line) in output.lines().enumerate() {
        if idx >= TOOL_BLOCK_EXPANDED_LINES {
            truncated = true;
            break;
        }
        if idx > 0 {
            preview.push('\n');
        }
        let mut chars = line.chars();
        for ch in chars.by_ref().take(TOOL_BLOCK_HISTORY_LINE_CHARS) {
            preview.push(ch);
        }
        if chars.next().is_some() {
            truncated = true;
            preview.push_str(" ...");
        }
    }
    if truncated {
        if !preview.is_empty() {
            preview.push('\n');
        }
        preview.push_str("[output truncated for TUI preview]");
    }
    preview
}

impl ItemHistory {
    pub fn new() -> Self {
        // Shadow parser: start at 80x24, gets resized at render
        // time. Same scrollback cap as the main parser via
        // `super::app::SCROLLBACK_MAX`.
        let shadow_parser = vt100::Parser::new(24, 80, super::app::SCROLLBACK_MAX);
        Self {
            shadow_parser,
            shadow_in_alt_screen: false,
            shadow_last_snapshot: Vec::new(),
            shadow_dirty_since_snapshot: false,
            shadow_snapshot_worthy_since_snapshot: false,
            shadow_cols: 80,
            shadow_rows: 24,
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

    /// Remove all accumulated items while preserving the current PTY
    /// geometry. Used by bootstrap when the transcript contains PTY
    /// ordering markers: rebuilding from the transcript preserves the
    /// chronological interleaving between raw bytes and transcript-only
    /// tool blocks, while older sessions without markers still fall back
    /// to pty.log replay.
    pub fn clear_items(&mut self) {
        let cols = self.shadow_cols;
        let rows = self.shadow_rows;
        *self = Self::new();
        self.set_pty_size(cols, rows);
    }

    /// Resize the shadow parser to match the PTY child's geometry.
    /// Call this before `feed_pty` whenever the caller knows the
    /// child's current size — bootstrap replay from `pty_replay`,
    /// every render frame, and any other path that knows the size.
    ///
    /// vt100's `set_size` doesn't reflow existing content, so the
    /// payoff is for *future* bytes: codex (and any normal-screen
    /// TUI) emits CSI cursor-positioning + scroll-region escapes
    /// that depend on terminal dimensions. If the shadow stays at
    /// the default 80×24 while the real PTY is 140×30, every
    /// out-of-range cursor position gets clamped and codex's UI
    /// state in the shadow drifts from what the user actually saw
    /// — scrollback then shows incoherent fragments instead of the
    /// real chat history.
    pub fn set_pty_size(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(VT100_MIN_DIM);
        let rows = rows.max(VT100_MIN_DIM);
        if self.shadow_cols != cols || self.shadow_rows != rows {
            self.snapshot_shadow_viewport();
            self.shadow_parser.screen_mut().set_size(rows, cols);
            self.shadow_cols = cols;
            self.shadow_rows = rows;
        }
    }

    /// Dimensions the live (non-shadow) parser was last built/sized at,
    /// or `None` before the first replay. Test-only: lets the render
    /// layer assert that editor-pane growth doesn't resize (and thus
    /// rebuild) the chat parser.
    #[cfg(test)]
    pub fn cached_dims(&self) -> Option<(u16, u16)> {
        self.cached.as_ref().map(|c| (c.cols, c.rows))
    }

    /// Feed a raw PTY byte chunk. Parses inline OSC `7700` markers
    /// out, drops the bytes between paired markers, and accumulates
    /// the rest into `Item::PtyChunk` entries.
    pub fn feed_pty(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.feed_byte(b);
        }
        // Mirror the bytes (with alt-screen filtering) into the
        // shadow parser used by mouse-wheel scrollback. See the
        // doc comment on `shadow_parser`.
        self.shadow_feed(bytes);
        self.dirty = true;
    }

    /// Route bytes to the shadow parser, skipping (a) the
    /// alt-screen toggle escape sequences themselves and (b)
    /// every byte that arrives while alt-screen is active. The
    /// goal is to keep the shadow in normal-screen mode so its
    /// natural `\r\n` accumulation can populate vt100's
    /// scrollback for the user-visible mouse-scroll-up path.
    fn shadow_feed(&mut self, bytes: &[u8]) {
        // (pattern, target state) — DEC private-mode toggles for
        // alt-screen across the three xterm variants. 1049 is the
        // modern "save+enter+restore" combo, 1047 is "enter and
        // keep save state from 1048", 47 is the original. We match
        // them all so we cover claude / any TUI child that picks
        // any of these.
        const TOGGLES: &[(&[u8], bool)] = &[
            (b"\x1b[?1049h", true),
            (b"\x1b[?1049l", false),
            (b"\x1b[?1047h", true),
            (b"\x1b[?1047l", false),
            (b"\x1b[?47h", true),
            (b"\x1b[?47l", false),
        ];
        let mut i = 0;
        while i < bytes.len() {
            // Probe for any toggle prefix at the current position.
            let mut matched: Option<(usize, bool)> = None;
            for (pat, target) in TOGGLES {
                if bytes[i..].starts_with(pat) {
                    matched = Some((pat.len(), *target));
                    break;
                }
            }
            if let Some((len, new_state)) = matched {
                // Drop the toggle bytes themselves AND flip state.
                // The shadow never enters alt-screen mode itself.
                self.shadow_in_alt_screen = new_state;
                i += len;
                continue;
            }
            if !self.shadow_in_alt_screen {
                if shadow_byte_starts_destructive_redraw(&bytes[i..]) {
                    self.snapshot_shadow_viewport();
                }
                if let Some(len) = csi_sequence_len(&bytes[i..]) {
                    self.shadow_parser.process(&bytes[i..i + len]);
                    if shadow_csi_is_snapshot_worthy(&bytes[i..i + len]) {
                        self.shadow_snapshot_worthy_since_snapshot = true;
                    }
                    i += len;
                    continue;
                }
                self.shadow_parser.process(&bytes[i..i + 1]);
                if shadow_byte_may_paint(bytes[i]) {
                    self.shadow_dirty_since_snapshot = true;
                }
                if shadow_byte_is_snapshot_worthy(bytes[i]) {
                    self.shadow_snapshot_worthy_since_snapshot = true;
                }
            }
            i += 1;
        }
    }

    fn snapshot_shadow_viewport(&mut self) {
        if !self.shadow_dirty_since_snapshot || !self.shadow_snapshot_worthy_since_snapshot {
            return;
        }
        let screen = self.shadow_parser.screen();
        let (rows, cols) = screen.size();
        let mut lines = Vec::new();
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                if let Some(cell) = screen.cell(row, col) {
                    line.push_str(cell.contents());
                }
            }
            let trimmed = line.trim_end().to_string();
            if trimmed.is_empty() {
                lines.push(ShadowSnapshotLine::Blank);
            } else {
                lines.push(ShadowSnapshotLine::Text(trimmed));
            }
        }
        trim_outer_blank_snapshot_lines(&mut lines);
        let text_rows = lines
            .iter()
            .filter(|line| matches!(line, ShadowSnapshotLine::Text(_)))
            .count();
        if !self.shadow_snapshot_worthy_since_snapshot && text_rows < 2 {
            self.shadow_dirty_since_snapshot = false;
            return;
        }
        if lines.is_empty() || lines == self.shadow_last_snapshot {
            self.shadow_dirty_since_snapshot = false;
            self.shadow_snapshot_worthy_since_snapshot = false;
            return;
        }
        self.shadow_last_snapshot = lines.clone();
        self.shadow_dirty_since_snapshot = false;
        self.shadow_snapshot_worthy_since_snapshot = false;
        for line in lines {
            match line {
                ShadowSnapshotLine::Text(text) => self.shadow_parser.process(text.as_bytes()),
                ShadowSnapshotLine::Blank => {}
            }
            self.shadow_parser.process(b"\r\n");
        }
    }

    /// Resize-and-render helper for the shadow path. Set its
    /// viewport dims to match what the caller wants and apply the
    /// scrollback offset. Cheap: just `set_size` (preserves grid +
    /// scrollback) and `set_scrollback`.
    fn render_shadow(&mut self, cols: u16, rows: u16, scrollback: usize) -> usize {
        self.set_pty_size(cols, rows);
        self.shadow_parser
            .screen_mut()
            .set_scrollback(super::app::SCROLLBACK_MAX);
        let max_scrollback = self.shadow_parser.screen().scrollback();
        self.shadow_parser.screen_mut().set_scrollback(scrollback);
        max_scrollback
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
            block.output = Some(tool_output_preview_for_history(&output));
            block.ok = ok;
        }
        if block.tool.is_some() {
            self.push_tool_block(block);
        } else {
            self.items.push(Item::ToolBlock(block));
        }
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

    fn push_tool_block(&mut self, block: ToolBlock) {
        self.items.push(Item::ToolBlock(block));
    }

    /// Apply a `ToolUse` event. The protocol's `ToolUse` carries no
    /// `call_id`, so pairing is FIFO. If an earlier OSC-open or
    /// `TaskStart` already created a block awaiting hydration,
    /// fill it in directly. Otherwise queue for the next OSC-open
    /// (legacy path).
    pub fn feed_tool_use(&mut self, tool: String, args_summary: String) {
        if let Some(idx) = self.pending_block_hydrations.pop_front() {
            if let Some(Item::ToolBlock(b)) = self.items.get_mut(idx) {
                b.tool = Some(tool.clone());
                b.args_summary = Some(args_summary.clone());
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
    pub fn feed_task_start(&mut self, call_id: String, tool: String, args_summary: String) {
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
            block.output = Some(tool_output_preview_for_history(&output));
            block.ok = ok;
        }
        self.push_tool_block(block);
        self.dirty = true;
    }

    /// Apply a `ToolResult` event. The protocol's `tool` field
    /// carries the *call_id* (zarvis convention), so we can match
    /// directly. If the OSC open hasn't arrived yet, stash for later.
    pub fn feed_tool_result(&mut self, call_id: &str, ok: bool, output: String) {
        let output = tool_output_preview_for_history(&output);
        if let Some(block) = self.find_block_mut(call_id) {
            block.output = Some(output);
            block.ok = ok;
        } else {
            self.pending_tool_results
                .insert(call_id.to_string(), (ok, output));
        }
        self.dirty = true;
    }

    /// Append a structured chat message (from a headless session that
    /// emits no PTY for its conversation). The adapter streams a
    /// `Message`/`Reasoning` event per delta, so consecutive deltas of
    /// the same kind are pushed as separate items that render
    /// contiguously — only the first of a run gets a leading break.
    /// Append-only keeps `replay` incremental (no full reparse).
    pub fn feed_message(&mut self, kind: MessageKind, text: &str) {
        if text.is_empty() {
            return;
        }
        let continues =
            matches!(self.items.last(), Some(Item::Message { kind: k, .. }) if *k == kind);
        let break_before = !continues && !self.items.is_empty();
        if !continues {
            // A new run starts here — close out any pending PTY bytes so
            // the message lands at the right point in the sequence.
            self.flush_chunk();
        }
        self.items.push(Item::Message {
            kind,
            text: text.to_string(),
            break_before,
        });
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
    /// 1. **Tool-block sessions (zarvis):** use per-item signatures to
    ///    reuse the parser across unchanged frames and rebuild only when
    ///    synthesized state changes (control hints, expand/collapse,
    ///    output arrival).
    /// 2. **Non-tool sessions (claude / codex / shell):** keep the
    ///    parser alive across frames. On resize, call `set_size` and
    ///    skip replay — matches what a real terminal does for those
    ///    tools. On streaming, process only the newly-appended items
    ///    instead of the entire history.
    ///
    /// Scrollback offset is applied via `Screen::set_scrollback`
    /// after either path produces the screen.
    pub fn replay(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput<'_> {
        // Keep the shadow's geometry in sync with the PTY's known
        // size every frame (not just when scrollback > 0). Codex
        // and other normal-screen TUIs emit CSI cursor-positioning
        // + scroll-region escapes that depend on terminal dims; if
        // the shadow stays at 80×24 default while bytes arrive
        // shaped for the real pane size, positions get clamped and
        // the shadow's screen / scrollback drifts from what the
        // user actually saw. This is a no-op when dims match.
        self.set_pty_size(cols, rows);

        // Any non-PTY item (a tool block, or a synthesized chat message
        // from a headless session) needs the synth-aware interleaving
        // path; the fast and shadow-scrollback paths handle raw PTY only.
        let needs_synth = self.items.iter().any(|i| !matches!(i, Item::PtyChunk(_)));
        // Mouse-wheel scrollback for non-tool-block sessions
        // (shell / claude / codex): divert to the shadow parser.
        // The shadow sees the same byte stream as main, with
        // alt-screen toggles + content filtered out, so it stays
        // in normal-screen mode and `set_scrollback` actually
        // exposes history (alt-screen has no scrollback by
        // design; codex's natural `\r\n` chat content still
        // flows through). At scroll=0 we fall back to the live
        // main parser so block hit-test geometry, animated tool
        // block counters, etc. stay correct.
        //
        // Tool-block sessions (zarvis) take the main parser path
        // even when scrolled. Two reasons:
        //   1. zarvis is `\r\n`-shaped chat; its main parser
        //      naturally populates scrollback, so the shadow
        //      isn't needed.
        //   2. The shadow only sees the truncated PTY-side
        //      preview (one ✓-glyph line per call); the
        //      synthesized multi-row `→ tool(args)` + status +
        //      body block lives only in the main parser path. If
        //      we diverted to the shadow as soon as the user
        //      scrolled, every tool block would collapse to one
        //      row and the surrounding chat would shift — the
        //      user perceives that as "the tool block
        //      disappeared". See
        //      `zarvis_tool_block_survives_one_row_of_scroll`
        //      for the regression repro.
        if scrollback > 0 && !needs_synth {
            let max_scrollback = self.render_shadow(cols, rows, scrollback);
            return RenderOutput {
                screen: self.shadow_parser.screen(),
                blocks: Vec::new(),
                max_scrollback,
            };
        }
        if needs_synth {
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
                parser: vt100::Parser::new(
                    rows.max(VT100_MIN_DIM),
                    cols.max(VT100_MIN_DIM),
                    super::app::SCROLLBACK_MAX,
                ),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
                pending_end_col: 0,
                item_layouts: Vec::new(),
            });
        }
        let cache = self.cached.as_mut().expect("just populated above");

        // Resize handling has two cases:
        //
        //   - Rows change but cols don't: `set_size` is enough.
        //     vt100 keeps the grid without replay and, importantly,
        //     avoids cursor-relocating reset escapes. The zarvis
        //     editor pane grows as the user types, shrinking
        //     `chat_area.height` and tripping this path on nearly
        //     every keystroke; closing the minibuffer grows the main
        //     pane and used to replay all accumulated PTY history on
        //     the UI thread. Prefer a cheap local resize here and let
        //     subsequent PTY output / SIGWINCH repaint exposed rows.
        //   - Cols changed: vt100 doesn't reflow soft-wrapped lines,
        //     so just `set_size` leaves prior content at the old
        //     wrap (looks narrow in a wider pane). Rebuild the parser
        //     from the items list so prior content re-wraps at the
        //     new width. Cost is one full replay on this frame; the
        //     cache continues to absorb streaming after. (The visible
        //     "history replay" on codex resize that this previously
        //     caused is now fixed at the render-loop level by
        //     coalescing PTY-chunk bursts before draw, so the rebuild
        //     cost is paid once per resize instead of per chunk.)
        if cache.cols != cols && cache.parser.screen().alternate_screen() {
            // Alt-screen app (codex / vim / htop): the grid has no
            // scrollback to reflow, and the app repaints itself on
            // the resize SIGWINCH the pane-size change triggers. A
            // full re-feed here would be wasted O(history) work AND
            // unsafe to bound — the `ESC[?1049h` alt-screen-enter
            // escape lives at the very start of the stream, so a
            // truncated tail would render into the normal screen.
            // Just resize the grid in place and let the app's
            // repaint settle the content.
            cache
                .parser
                .screen_mut()
                .set_size(rows.max(VT100_MIN_DIM), cols.max(VT100_MIN_DIM));
            cache.cols = cols;
            cache.rows = rows;
        } else if cache.cols != cols {
            cache.parser = vt100::Parser::new(
                rows.max(VT100_MIN_DIM),
                cols.max(VT100_MIN_DIM),
                super::app::SCROLLBACK_MAX,
            );
            cache.cols = cols;
            cache.rows = rows;
            // Re-feed only the tail that survives the parser's
            // SCROLLBACK_MAX-line ring at the new width. A non-tool
            // session's whole history accumulates in one unbounded
            // `pending_chunk`, so the old `pending_consumed = 0`
            // re-parsed the entire buffer on the UI thread on every
            // width change — the zoom/resize freeze (#230). When the
            // pending tail alone covers the retained window, skip the
            // (older) items entirely and re-feed only that tail.
            let pending_offset = reflow_tail_offset(&self.pending_chunk, reflow_budget_lines(rows));
            if pending_offset > 0 {
                cache.processed_count = self.items.len();
                cache.pending_consumed = pending_offset;
            } else {
                // Short session: feed everything (cheap), as before.
                cache.processed_count = 0;
                cache.pending_consumed = 0;
            }
        } else if cache.rows != rows {
            cache
                .parser
                .screen_mut()
                .set_size(rows.max(VT100_MIN_DIM), cols.max(VT100_MIN_DIM));
            cache.rows = rows;
        }

        // Feed newly-appended items through the live parser.
        if cache.processed_count < self.items.len() {
            for item in &self.items[cache.processed_count..] {
                if let Item::PtyChunk(b) = item {
                    cache.parser.process(b);
                }
                // Non-PtyChunk items (tool blocks / messages) can't occur
                // in this path (needs_synth is false), but be defensive.
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
                parser: vt100::Parser::new(
                    rows.max(VT100_MIN_DIM),
                    cols.max(VT100_MIN_DIM),
                    super::app::SCROLLBACK_MAX,
                ),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
                pending_end_col: 0,
                item_layouts: Vec::new(),
            };
            // Same scrollback-tail bound as the width-change rebuild:
            // only re-feed the pending tail that survives the ring.
            let pending_offset = reflow_tail_offset(&self.pending_chunk, reflow_budget_lines(rows));
            if pending_offset > 0 {
                cache.processed_count = self.items.len();
            } else {
                for item in &self.items {
                    if let Item::PtyChunk(b) = item {
                        cache.parser.process(b);
                    }
                }
            }
            cache.parser.process(&self.pending_chunk[pending_offset..]);
            cache.processed_count = self.items.len();
            cache.pending_consumed = self.pending_chunk.len();
        }

        cache
            .parser
            .screen_mut()
            .set_scrollback(super::app::SCROLLBACK_MAX);
        let max_scrollback = cache.parser.screen().scrollback();
        cache.parser.screen_mut().set_scrollback(scrollback);
        self.dirty = false;
        RenderOutput {
            screen: cache.parser.screen(),
            blocks: Vec::new(),
            max_scrollback,
        }
    }

    /// Tool-block-aware replay. Reuses the persistent parser when
    /// safe; falls back to a full rebuild only when an item
    /// mid-history actually mutated (block hydrated, expanded
    /// toggled, running controls appeared). Append-only
    /// changes (new chunks, new blocks at the tail) just feed the
    /// new suffix through the live parser — same shape as
    /// `replay_cached` but with per-block signatures so we know
    /// when invalidation is required.
    fn replay_full(&mut self, cols: u16, rows: u16, scrollback: usize) -> RenderOutput<'_> {
        // Per-item signatures: if anything in the prefix mutated,
        // we have to rebuild from that item onward because vt100 has
        // no "undo bytes" API.
        let current_sigs: Vec<ItemSig> = self.items.iter().map(ItemSig::of).collect();

        let flushed_pending_layout = self.cached.as_ref().and_then(|c| {
            let pending_shrank = self.pending_chunk.len() < c.pending_consumed;
            if !pending_shrank || !self.pending_chunk.is_empty() {
                return None;
            }
            match self.items.get(c.processed_count) {
                Some(Item::PtyChunk(bytes)) if bytes.len() >= c.pending_consumed => {
                    Some((
                        c.processed_count,
                        c.pending_consumed,
                        c.pending_visible_lines,
                        c.pending_end_col,
                    ))
                }
                _ => None,
            }
        });

        let rebuild_from = match &self.cached {
            None => true,
            Some(c) => {
                c.cols != cols
                    || current_sigs.len() < c.signatures.len()
                    || (self.pending_chunk.len() < c.pending_consumed
                        && flushed_pending_layout.is_none())
            }
        };
        let changed_idx = self.cached.as_ref().and_then(|c| {
            c.signatures
                .iter()
                .zip(current_sigs.iter())
                .position(|(a, b)| a != b)
        });
        let rebuild_from = if rebuild_from {
            Some(0)
        } else {
            self.cached.as_ref().and_then(|c| {
                changed_idx.map(|idx| suffix_rebuild_start(&c.item_layouts, idx, rows))
            })
        };

        if let Some(start) = rebuild_from {
            let preserved_layouts = self
                .cached
                .as_ref()
                .map(|cache| cache.item_layouts[..start.min(cache.item_layouts.len())].to_vec())
                .unwrap_or_default();
            self.cached = Some(CachedParser {
                parser: vt100::Parser::new(
                    rows.max(VT100_MIN_DIM),
                    cols.max(VT100_MIN_DIM),
                    super::app::SCROLLBACK_MAX,
                ),
                cols,
                rows,
                processed_count: 0,
                pending_consumed: 0,
                signatures: Vec::new(),
                pending_visible_lines: 0,
                pending_end_col: 0,
                item_layouts: preserved_layouts,
            });
        }
        let cache = self.cached.as_mut().expect("just populated above");
        if rebuild_from.is_none() && cache.rows != rows {
            cache
                .parser
                .screen_mut()
                .set_size(rows.max(VT100_MIN_DIM), cols.max(VT100_MIN_DIM));
            cache.rows = rows;
        }

        struct BlockSpan {
            call_id: String,
            abs_start: usize,
            abs_end: usize,
            status_abs_row: Option<usize>,
            bg_cols: Option<(u16, u16)>,
            kill_cols: Option<(u16, u16)>,
        }
        // Compute layouts only for items past the cached prefix.
        // Cached layouts for `[0..reuse_upto)` stay valid because the
        // non-rebuild path guarantees `cols` is unchanged (a cols
        // change forces a rebuild), and any signature mutation drops
        // layouts from the changed suffix onward. When a suffix
        // rebuild preserves old prefix layouts, those prefix rows are
        // retained only for absolute hit-rect math; the new parser is
        // fed from `start_processing_at`, which covers the retained
        // scrollback window and avoids replaying ancient history.
        // We no longer re-scan the whole history with
        // `count_visible_lines` / `synth_block` every frame. That
        // O(history)-per-frame re-scan was the zarvis typing lag.
        let start_processing_at = flushed_pending_layout
            .map(|(idx, _, _, _)| idx + 1)
            .unwrap_or_else(|| rebuild_from.unwrap_or(cache.processed_count));
        let reuse_upto = cache.item_layouts.len().min(self.items.len());
        let mut cursor_col = cache
            .item_layouts
            .last()
            .map(|layout| layout.end_col)
            .unwrap_or(0);
        for (idx, item) in self.items.iter().enumerate().skip(reuse_upto) {
            let layout = match item {
                Item::PtyChunk(b) => {
                    let uses_flushed_pending_layout = matches!(
                        flushed_pending_layout,
                        Some((flushed_idx, _, _, _)) if idx == flushed_idx
                    );
                    if uses_flushed_pending_layout {
                        let (_, consumed, flushed_lines, flushed_end_col) =
                            flushed_pending_layout.expect("checked above");
                        if consumed < b.len() {
                            let suffix = &b[consumed..];
                            cache.parser.process(suffix);
                            let metrics = visible_metrics_from_col(suffix, cols, flushed_end_col);
                            cursor_col = metrics.end_col;
                            ItemLayout {
                                lines: flushed_lines + metrics.lines,
                                end_col: metrics.end_col,
                                blocks: Vec::new(),
                            }
                        } else {
                            cursor_col = flushed_end_col;
                            ItemLayout {
                                lines: flushed_lines,
                                end_col: flushed_end_col,
                                blocks: Vec::new(),
                            }
                        }
                    } else if idx >= start_processing_at {
                        cache.parser.process(b);
                        let metrics = visible_metrics_from_col(b, cols, cursor_col);
                        cursor_col = metrics.end_col;
                        ItemLayout {
                            lines: metrics.lines,
                            end_col: metrics.end_col,
                            blocks: Vec::new(),
                        }
                    } else {
                        let metrics = visible_metrics_from_col(b, cols, cursor_col);
                        cursor_col = metrics.end_col;
                        ItemLayout {
                            lines: metrics.lines,
                            end_col: metrics.end_col,
                            blocks: Vec::new(),
                        }
                    }
                }
                Item::ToolBlock(block) => {
                    let synth = synth_block(block, cols);
                    if idx >= start_processing_at {
                        cache.parser.process(&synth.bytes);
                    }
                    let metrics = visible_metrics_from_col(&synth.bytes, cols, cursor_col);
                    cursor_col = metrics.end_col;
                    ItemLayout {
                        lines: metrics.lines,
                        end_col: metrics.end_col,
                        blocks: vec![BlockLayout {
                            call_id: block.call_id.clone(),
                            row_start_offset: 0,
                            row_end_offset: metrics.lines,
                            status_row_offset: synth.status_row_offset.map(|off| off as usize),
                            bg_cols: synth.bg_button_cols,
                            kill_cols: synth.kill_button_cols,
                        }],
                    }
                }
                Item::Message {
                    kind,
                    text,
                    break_before,
                } => {
                    let bytes = synth_message(*kind, text, *break_before);
                    if idx >= start_processing_at {
                        cache.parser.process(&bytes);
                    }
                    let metrics = visible_metrics_from_col(&bytes, cols, cursor_col);
                    cursor_col = metrics.end_col;
                    ItemLayout {
                        lines: metrics.lines,
                        end_col: metrics.end_col,
                        blocks: Vec::new(),
                    }
                }
            };
            cache.item_layouts.push(layout);
        }

        // Cumulative line positions + block-span hit rects, summed from
        // the (mostly cached) per-item layouts. O(items), no byte scan.
        let mut block_spans: Vec<BlockSpan> = Vec::new();
        let mut abs_line: usize = 0;
        for layout in &cache.item_layouts {
            let start = abs_line;
            abs_line += layout.lines;
            for bl in &layout.blocks {
                block_spans.push(BlockSpan {
                    call_id: bl.call_id.clone(),
                    abs_start: start + bl.row_start_offset,
                    abs_end: start + bl.row_end_offset,
                    status_abs_row: bl.status_row_offset.map(|off| start + off),
                    bg_cols: bl.bg_cols,
                    kill_cols: bl.kill_cols,
                });
            }
        }

        // Pending: feed only the new tail through the parser. The
        // running visible-line count is kept on the cache so we
        // never re-iterate the whole pending buffer (otherwise long
        // sessions pay O(history) per frame just to position block
        // spans).
        if rebuild_from.is_some() || flushed_pending_layout.is_some() {
            cache.pending_visible_lines = 0;
            cache.pending_end_col = cursor_col;
        }
        let pending_start = cache.pending_consumed.min(self.pending_chunk.len());
        if pending_start == 0 && cache.pending_visible_lines == 0 {
            cache.pending_end_col = cursor_col;
        }
        if pending_start < self.pending_chunk.len() {
            let suffix = &self.pending_chunk[pending_start..];
            cache.parser.process(suffix);
            let metrics = visible_metrics_from_col(suffix, cols, cache.pending_end_col);
            cache.pending_visible_lines += metrics.lines;
            cache.pending_end_col = metrics.end_col;
        }
        abs_line += cache.pending_visible_lines;

        cache.processed_count = self.items.len();
        cache.pending_consumed = self.pending_chunk.len();
        cache.signatures = current_sigs;

        cache
            .parser
            .screen_mut()
            .set_scrollback(super::app::SCROLLBACK_MAX);
        let max_scrollback = cache.parser.screen().scrollback();
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
            let row_end = span.abs_end.saturating_sub(visible_top).min(rows as usize) as u16;
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
            max_scrollback,
        }
    }
}

impl Default for ItemHistory {
    fn default() -> Self {
        Self::new()
    }
}

fn trim_outer_blank_snapshot_lines(lines: &mut Vec<ShadowSnapshotLine>) {
    let first_text = lines
        .iter()
        .position(|line| matches!(line, ShadowSnapshotLine::Text(_)));
    let Some(first_text) = first_text else {
        lines.clear();
        return;
    };
    let last_text = lines
        .iter()
        .rposition(|line| matches!(line, ShadowSnapshotLine::Text(_)))
        .unwrap_or(first_text);
    if first_text > 0 || last_text + 1 < lines.len() {
        *lines = lines[first_text..=last_text].to_vec();
    }
}

fn shadow_byte_starts_destructive_redraw(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x1b[H")
        || bytes.starts_with(b"\x1b[1;1H")
        || bytes.starts_with(b"\x1b[2J")
        || bytes.starts_with(b"\x1b[J")
}

fn csi_sequence_len(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"\x1b[") {
        return None;
    }
    for (idx, b) in bytes.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(b) {
            return Some(idx + 1);
        }
    }
    None
}

fn shadow_byte_may_paint(b: u8) -> bool {
    b == b'\n' || b == b'\r' || b >= 0x20
}

fn shadow_byte_is_snapshot_worthy(b: u8) -> bool {
    b == b'\n' || b == b'\r'
}

fn shadow_csi_is_snapshot_worthy(seq: &[u8]) -> bool {
    matches!(seq.last().copied(), Some(b'H' | b'f'))
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
/// Extra lines re-fed beyond `SCROLLBACK_MAX` when rebuilding the
/// parser at a new width, as a "warmup" margin. The parser only
/// keeps `SCROLLBACK_MAX` lines, so the oldest re-fed lines scroll
/// off and are discarded — feeding a margin before the retained
/// window means the retained scrollback is parsed from terminal
/// state (SGR, cursor, modes) that the margin already
/// re-established, rather than cold from an arbitrary mid-stream
/// cut. 1024 lines is generous (a persistent escape effect almost
/// never spans that far in normal output) and cheap to re-parse.
const REFLOW_WARMUP_LINES: usize = 1024;

/// Lines to retain (scrollback + viewport + warmup) when
/// rebuilding the parser at a new width. The parser's ring keeps
/// `SCROLLBACK_MAX`; the rest is viewport + reflow warmup.
fn reflow_budget_lines(rows: u16) -> usize {
    super::app::SCROLLBACK_MAX + rows as usize + REFLOW_WARMUP_LINES
}

/// Byte offset into a raw PTY buffer such that `buf[offset..]`
/// contains at least `budget_lines` newline-delimited lines.
/// Used to bound the re-feed when rebuilding the parser at a new
/// width (zoom / list-toggle / resize): a non-tool session's
/// entire history accumulates in one unbounded `pending_chunk`,
/// and re-feeding all of it re-parsed megabytes on the UI thread
/// — the zoom/resize freeze (issue #230). Everything older than
/// the budget produces lines the `SCROLLBACK_MAX` ring discards
/// immediately, so it's wasted work. Counting newlines (not
/// wrapped rows) is a safe lower bound on visible rows — wrapping
/// only adds rows, so the suffix always covers at least the
/// retained window. Returns 0 when the buffer has fewer lines, so
/// short sessions behave exactly as before.
fn reflow_tail_offset(buf: &[u8], budget_lines: usize) -> usize {
    let mut seen = 0usize;
    let mut i = buf.len();
    while i > 0 {
        i -= 1;
        if buf[i] == b'\n' {
            seen += 1;
            if seen > budget_lines {
                return i + 1; // start just after this newline
            }
        }
    }
    0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisibleMetrics {
    lines: usize,
    end_col: usize,
}

fn visible_metrics_from_col(bytes: &[u8], cols: u16, start_col: usize) -> VisibleMetrics {
    let mut lines = 0usize;
    let mut col = start_col;
    let cols = cols.max(VT100_MIN_DIM) as usize;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            i += 1;
            if i >= bytes.len() {
                break;
            }
            match bytes[i] {
                b'[' => {
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
                    i += 1;
                    while i < bytes.len() {
                        if bytes[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                }
                b'O' => i = (i + 2).min(bytes.len()),
                _ => i = (i + 1).min(bytes.len()),
            }
            continue;
        }
        match b {
            b'\n' => {
                lines += 1;
                col = 0;
                i += 1;
            }
            b'\r' => {
                col = 0;
                i += 1;
            }
            0x00..=0x08 | 0x0b..=0x1f | 0x7f => {
                i += 1;
            }
            _ => {
                let Ok(s) = std::str::from_utf8(&bytes[i..]) else {
                    break;
                };
                let Some(ch) = s.chars().next() else {
                    break;
                };
                i += ch.len_utf8();
                let width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if width == 0 {
                    continue;
                }
                if col + width > cols {
                    lines += 1;
                    col = 0;
                }
                col += width;
                if col >= cols {
                    lines += 1;
                    col = 0;
                }
            }
        }
    }
    VisibleMetrics {
        lines,
        end_col: col,
    }
}

#[cfg(test)]
fn count_visible_lines_from_col(bytes: &[u8], cols: u16, start_col: usize) -> usize {
    visible_metrics_from_col(bytes, cols, start_col).lines
}

#[cfg(test)]
fn count_visible_lines(bytes: &[u8], cols: u16) -> usize {
    count_visible_lines_from_col(bytes, cols, 0)
}

/// Build the byte sequence representing a tool block at its current
/// state. Matches the visual idiom zarvis writes inline so
/// non-ratatui consumers and the items-model render stay coherent.
///
/// States rendered:
/// - **Running** (`output == None`): header + status row with
///   stable status row; a keyboard-control hint appears after the
///   `BUTTONS_AFTER_MS` threshold.
/// - **Backgrounded** (`output == BG_PLACEHOLDER_OUTPUT`): header +
///   status row with "in background"; `Esc` kill hint only.
/// - **Completed** (`output != None` and non-placeholder): header
///   + glyph + truncated body + optional expand/collapse footer.
/// Append `text` to `out`, normalizing newlines to CRLF so the vt100
/// parser advances rows correctly (a bare `\n` would line-feed without a
/// carriage return → staircase). Lone `\r` is dropped.
fn push_text_crlf(out: &mut Vec<u8>, text: &str) {
    let mut buf = [0u8; 4];
    for ch in text.chars() {
        match ch {
            '\r' => {}
            '\n' => out.extend_from_slice(b"\r\n"),
            _ => out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes()),
        }
    }
}

fn count_lines_up_to(text: &str, limit: usize) -> (usize, bool) {
    let mut count = 0usize;
    for _ in text.lines() {
        if count >= limit {
            return (count, true);
        }
        count += 1;
    }
    (count, false)
}

/// Synthesize terminal bytes for a structured chat [`Item::Message`].
/// vt100 handles column wrapping, so we only normalize newlines and add
/// role styling. `break_before` (set on the first item of a run) starts
/// the message on a fresh line, separating it from prior content.
fn synth_message(kind: MessageKind, text: &str, break_before: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 16);
    if break_before {
        out.extend_from_slice(b"\r\n");
    }
    match kind {
        MessageKind::Assistant => push_text_crlf(&mut out, text),
        MessageKind::User => {
            // Gray `❯ ` prompt glyph + gray body — mirrors how consumed
            // user input reads elsewhere in the TUI.
            out.extend_from_slice("\x1b[90m❯ ".as_bytes());
            push_text_crlf(&mut out, text);
            out.extend_from_slice(b"\x1b[0m");
        }
        MessageKind::Reasoning => {
            out.extend_from_slice(b"\x1b[2m");
            push_text_crlf(&mut out, text);
            out.extend_from_slice(b"\x1b[0m");
        }
    }
    out
}

fn synth_block(block: &ToolBlock, cols: u16) -> SynthOutput {
    /// Placeholder string the zarvis adapter writes into a tool's
    /// `output` when it auto-backgrounds. Kept in sync via the
    /// `agentd_adapter_zarvis::tasks::BG_PLACEHOLDER_OUTPUT`
    /// constant — duplicated here only to avoid a cross-crate
    /// dep just for one string.
    const BG_PLACEHOLDER_OUTPUT: &str = "(running in background; will report when complete)";

    let mut out: Vec<u8> = Vec::with_capacity(128);
    // Leading blank — separates the block from prior chat content
    // (mirrors zarvis's `\r\n→ ...` line layout). This blank takes
    // one visible row, then the header.
    out.extend_from_slice(b"\r\n");

    append_tool_header(
        &mut out,
        block.tool.as_deref().unwrap_or("?"),
        block.args_summary.as_deref().unwrap_or(""),
        cols,
    );
    // After the leading `\r\n` + header, we've emitted at least 2 rows.
    // status_row_offset accounts for wider, explicitly wrapped headers.
    let header_rows = tool_header_rows(
        block.tool.as_deref().unwrap_or("?"),
        block.args_summary.as_deref().unwrap_or(""),
        cols,
    );

    // Classify by output content.
    let output_opt = block.output.as_deref();
    let is_running = output_opt.is_none();
    let is_backgrounded = output_opt
        .map(|o| o == BG_PLACEHOLDER_OUTPUT)
        .unwrap_or(false);
    let is_completed = !is_running && !is_backgrounded;

    let mut status_row_offset: Option<u16> = None;
    let bg_button_cols: Option<(u16, u16)> = None;
    let kill_button_cols: Option<(u16, u16)> = None;

    if is_running || is_backgrounded {
        status_row_offset = Some(1 + header_rows as u16);
        let controls_ready = tool_block_controls_ready(block);

        let mut line = String::new();
        line.push_str("  ");
        let status_text = if is_running {
            "running"
        } else {
            "in background"
        };
        line.push_str(&format!("\x1b[2;33m{status_text}\x1b[0m"));
        if controls_ready {
            if is_running {
                line.push_str("  \x1b[2m· Ctrl-b background · Esc kill · type to queue\x1b[0m");
            } else {
                line.push_str("  \x1b[2m· Esc kill\x1b[0m");
            }
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
        let (known_lines, has_more_lines) = count_lines_up_to(output, TOOL_BLOCK_EXPANDED_LINES);
        let visible = if block.expanded {
            known_lines
        } else {
            TOOL_BLOCK_COLLAPSED_LINES.min(known_lines)
        };
        let max_col = (cols as usize)
            .saturating_sub(7)
            .min(TOOL_BLOCK_MAX_COLS)
            .max(8);

        if known_lines == 0 {
            let payload = format!("  {glyph}  \x1b[2m(no output)\x1b[0m\r\n");
            out.extend_from_slice(payload.as_bytes());
        } else {
            for (i, line) in output.lines().take(visible).enumerate() {
                let trimmed: String = line.chars().take(max_col).collect();
                if i == 0 {
                    let payload = format!("  {glyph}  \x1b[2m{trimmed}\x1b[0m\r\n");
                    out.extend_from_slice(payload.as_bytes());
                } else {
                    let payload = format!("     \x1b[2m{trimmed}\x1b[0m\r\n");
                    out.extend_from_slice(payload.as_bytes());
                }
            }
        }

        if known_lines > 0 {
            if block.expanded && (known_lines > TOOL_BLOCK_COLLAPSED_LINES || has_more_lines) {
                if has_more_lines {
                    let footer = format!(
                        "     \x1b[2;36m[showing {visible}/{visible}+ lines — click to collapse]\x1b[0m\r\n"
                    );
                    out.extend_from_slice(footer.as_bytes());
                } else {
                    let footer = "     \x1b[2;36m[click to collapse]\x1b[0m\r\n".to_string();
                    out.extend_from_slice(footer.as_bytes());
                }
            } else if !block.expanded
                && (known_lines > TOOL_BLOCK_COLLAPSED_LINES || has_more_lines)
            {
                let remaining = known_lines.saturating_sub(TOOL_BLOCK_COLLAPSED_LINES);
                let plus = if has_more_lines { "+" } else { "" };
                let footer = format!(
                    "     \x1b[2;36m[+{remaining}{plus} lines — click to expand]\x1b[0m\r\n"
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

fn append_tool_header(out: &mut Vec<u8>, tool: &str, args: &str, cols: u16) {
    let args = compact_tool_args(args);
    let cols = cols.max(VT100_MIN_DIM) as usize;
    let tool_width = UnicodeWidthStr::width(tool);
    let prefix_width = 2 + tool_width + 1; // "→ " + tool + "("
    let suffix_width = 1; // ")"

    let arrow = "\x1b[2;32m→ \x1b[0m";
    let tool_style = "\x1b[1;92m";
    let dim = "\x1b[2m";
    let reset = "\x1b[0m";

    if args.is_empty() {
        out.extend_from_slice(
            format!("{arrow}{tool_style}{tool}{reset}{dim}(){reset}\r\n").as_bytes(),
        );
        return;
    }

    if prefix_width + UnicodeWidthStr::width(args.as_str()) + suffix_width <= cols {
        out.extend_from_slice(
            format!("{arrow}{tool_style}{tool}{reset}{dim}({args}){reset}\r\n").as_bytes(),
        );
        return;
    }

    out.extend_from_slice(format!("{arrow}{tool_style}{tool}{reset}{dim}({reset}\r\n").as_bytes());
    let body_width = cols.saturating_sub(2).max(8);
    for line in wrap_display_width(&args, body_width) {
        out.extend_from_slice(format!("  {dim}{line}{reset}\r\n").as_bytes());
    }
    out.extend_from_slice(format!("{dim}){reset}\r\n").as_bytes());
}

fn tool_header_rows(tool: &str, args: &str, cols: u16) -> usize {
    let args = compact_tool_args(args);
    let cols = cols.max(VT100_MIN_DIM) as usize;
    let prefix_width = 2 + UnicodeWidthStr::width(tool) + 1;
    if args.is_empty() || prefix_width + UnicodeWidthStr::width(args.as_str()) + 1 <= cols {
        1
    } else {
        2 + wrap_display_width(&args, cols.saturating_sub(2).max(8)).len()
    }
}

fn compact_tool_args(args: &str) -> String {
    args.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn wrap_display_width(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split(' ').filter(|w| !w.is_empty()) {
        let word_width = UnicodeWidthStr::width(word);
        if current.is_empty() {
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                lines.extend(split_word_display_width(word, width));
                current_width = 0;
            }
        } else if current_width + 1 + word_width <= width {
            current.push(' ');
            current.push_str(word);
            current_width += 1 + word_width;
        } else {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            if word_width <= width {
                current.push_str(word);
                current_width = word_width;
            } else {
                lines.extend(split_word_display_width(word, width));
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_word_display_width(word: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in word.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0).max(1);
        if !current.is_empty() && current_width + ch_width > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
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

    fn tool_block_ids(item: &Item) -> Vec<String> {
        match item {
            Item::ToolBlock(b) => vec![b.call_id.clone()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn no_osc_yields_one_chunk_on_replay() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"hello\r\nworld\r\n");
        // No blocks parsed.
        assert_eq!(block_count(&h), 0);
        let out = h.replay(40, 10, 0);
        let cell = out
            .screen
            .cell(0, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "h");
    }

    // --- zoom/resize freeze regression guards (issue #230) ---

    /// The tail-offset bound truncates a long buffer to ~budget
    /// lines from the end. Guards the core of the
    /// re-feed-only-the-tail fix.
    #[test]
    fn reflow_tail_offset_bounds_long_buffer() {
        let mut buf = Vec::new();
        for i in 0..100_000u32 {
            buf.extend_from_slice(format!("line{i}\n").as_bytes());
        }
        let budget = 6_000;
        let off = reflow_tail_offset(&buf, budget);
        assert!(off > 0, "a 100k-line buffer must be truncated for re-feed");
        let suffix_lines = buf[off..].iter().filter(|&&b| b == b'\n').count();
        assert!(
            suffix_lines <= budget + 1,
            "suffix must be bounded to ~budget lines, got {suffix_lines}"
        );
        // The retained tail must still end with the newest line.
        assert!(buf[off..].ends_with(b"line99999\n"));
    }

    /// A buffer with fewer than `budget` lines is fed whole
    /// (offset 0) — short sessions are unaffected by the bound.
    #[test]
    fn reflow_tail_offset_keeps_short_buffer_whole() {
        assert_eq!(reflow_tail_offset(b"a\nb\nc\n", 6_000), 0);
    }

    /// After a width change on a long streaming session, the
    /// bounded re-feed must still render the true tail (the newest
    /// lines), not drop content. Correctness guard for the bound.
    #[test]
    fn resize_long_history_renders_correct_tail() {
        let mut h = ItemHistory::new();
        // 20k fixed-width markers (7 cols, no wrap at width 50).
        for i in 1..=20_000u32 {
            h.feed_pty(format!("L{i:06}\r\n").as_bytes());
        }
        // Warm at one width, then force a width-change rebuild.
        let _ = h.replay(80, 24, 0);
        let screen = h.replay(50, 24, 0).screen.contents();
        assert!(
            screen.contains("L020000"),
            "newest line missing after resize; screen:\n{screen}"
        );
        assert!(
            screen.contains("L019999"),
            "second-newest line missing after resize"
        );
    }

    /// An alt-screen app (codex / vim / htop) must stay in
    /// alt-screen across a width change — the bounded re-feed must
    /// NOT be applied to it, because the `ESC[?1049h` enter escape
    /// lives at the very start of the (long) stream and truncating
    /// it would drop the parser back to the normal screen. Long
    /// enough to exceed the re-feed budget so the unconditional
    /// bound would visibly break it.
    #[test]
    fn resize_in_alt_screen_keeps_alt_grid() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"\x1b[?1049h"); // enter alt-screen once, at the start
        for i in 0..8_000u32 {
            h.feed_pty(format!("frame line {i}\r\n").as_bytes());
        }
        h.feed_pty(b"\x1b[HALT_MARKER"); // home + marker on the grid
        let _ = h.replay(80, 24, 0);
        let out = h.replay(50, 24, 0); // width change
        assert!(
            out.screen.alternate_screen(),
            "must stay in alt-screen after resize (enter escape not truncated)"
        );
        assert!(
            out.screen.contents().contains("ALT_MARKER"),
            "alt-screen content lost after resize"
        );
    }

    /// PERF regression: the width-change rebuild must NOT re-parse
    /// the entire (unbounded) history — that was the zoom/resize
    /// freeze. With the scrollback-tail bound the rebuild is
    /// constant regardless of history size; without it this blows
    /// the budget. Measures only the resize replay (the flood is
    /// warmup). Generous budget to stay non-flaky in debug CI while
    /// still failing by multiples if the bound is removed.
    #[test]
    fn resize_long_history_rebuild_is_bounded() {
        let mut h = ItemHistory::new();
        for i in 1..=300_000u32 {
            h.feed_pty(format!("L{i}\r\n").as_bytes());
        }
        let _ = h.replay(80, 24, 0); // warmup: initial full parse
        let _ = h.replay(80, 24, 0); // cached
        let t = std::time::Instant::now();
        let _ = h.replay(120, 30, 0); // width change → bounded rebuild
        let us = t.elapsed().as_micros();
        assert!(
            us < 120_000,
            "resize on a 300k-line history took {us} µs — bound removed? \
             (should re-feed only the scrollback tail, not all history)"
        );
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
        let block_a = h
            .items
            .iter()
            .find_map(|i| match i {
                Item::ToolBlock(b) if b.call_id == "A" => Some(b),
                _ => None,
            })
            .unwrap();
        let block_b = h
            .items
            .iter()
            .find_map(|i| match i {
                Item::ToolBlock(b) if b.call_id == "B" => Some(b),
                _ => None,
            })
            .unwrap();
        assert_eq!(block_a.tool.as_deref(), Some("shell"));
        assert_eq!(block_b.tool.as_deref(), Some("read_file"));
    }

    #[test]
    fn adjacent_same_tool_task_starts_stay_separate_blocks() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "read_file".into(), r#"{"path":"a.rs"}"#.into());
        h.feed_task_start("B".into(), "read_file".into(), r#"{"path":"b.rs"}"#.into());
        h.feed_tool_result("A", true, "a".into());
        h.feed_tool_result("B", false, "missing".into());

        assert_eq!(h.items.len(), 2);
        assert_eq!(tool_block_ids(&h.items[0]), vec!["A"]);
        assert_eq!(tool_block_ids(&h.items[1]), vec!["B"]);

        let text = screen_text(h.replay(100, 20, 0).screen, 20, 100);
        assert!(text.contains("→ read_file("), "{text}");
        assert!(text.contains("a.rs"), "{text}");
        assert!(text.contains("b.rs"), "{text}");
        assert!(!text.contains("× 2"), "{text}");
    }

    #[test]
    fn grouping_does_not_cross_different_tool_or_pty() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "read_file".into(), r#"{"path":"a.rs"}"#.into());
        h.feed_task_start("B".into(), "list_dir".into(), r#"{"path":"src"}"#.into());
        h.feed_pty(b"assistant prose\r\n");
        h.feed_task_start("C".into(), "read_file".into(), r#"{"path":"c.rs"}"#.into());

        assert!(matches!(h.items[0], Item::ToolBlock(_)));
        assert!(matches!(h.items[1], Item::ToolBlock(_)));
        assert!(matches!(h.items[2], Item::PtyChunk(_)));
        assert!(matches!(h.items[3], Item::ToolBlock(_)));
    }

    #[test]
    fn adjacent_same_tool_blocks_expose_individual_hit_rects() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "shell".into(), "echo a".into());
        h.feed_task_start("B".into(), "shell".into(), "echo b".into());
        h.feed_tool_result("A", true, "a\nmore a\nstill a".into());
        h.feed_tool_result("B", true, "b\nmore b\nstill b".into());

        let out = h.replay(100, 40, 0);
        let text = screen_text(out.screen, 40, 100);
        assert!(text.contains("→ shell"), "{text}");
        let ids: Vec<&str> = out.blocks.iter().map(|b| b.call_id.as_str()).collect();
        assert!(ids.contains(&"A"), "{ids:?}");
        assert!(ids.contains(&"B"), "{ids:?}");

        assert!(h.toggle_block("A"));
        match &h.items[0] {
            Item::ToolBlock(block) => assert!(block.expanded, "child output should expand"),
            other => panic!("expected tool block, got {other:?}"),
        }
    }

    #[test]
    fn late_tool_use_hydration_stays_separate_from_previous_block() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "shell".into(), "echo a".into());
        h.feed_pty(b"\x1b]7700;open;call=B\x07inline");
        h.feed_tool_use("shell".into(), "echo b".into());
        h.feed_tool_result("B", true, "b".into());
        h.feed_pty(b"\x1b]7700;close;call=B\x07");

        assert_eq!(h.items.len(), 2);
        match &h.items[1] {
            Item::ToolBlock(block) => {
                assert_eq!(block.call_id, "B");
                assert_eq!(block.tool.as_deref(), Some("shell"));
                assert_eq!(block.args_summary.as_deref(), Some("echo b"));
                assert_eq!(block.output.as_deref(), Some("b"));
            }
            other => panic!("expected tool block, got {other:?}"),
        }
    }

    #[test]
    fn tool_header_compacts_and_wraps_multiline_args() {
        let mut h = ItemHistory::new();
        h.feed_task_start(
            "c1".into(),
            "shell".into(),
            "set -euo pipefail\n        branch=docs-readme-remote-control\n        wt=.claude/worktrees/$branch\n        git fetch origin main".into(),
        );
        let text = screen_text(h.replay(48, 12, 0).screen, 12, 48);

        assert!(text.contains("→ shell("), "{text}");
        assert!(text.contains("branch=docs-readme-remote-control"), "{text}");
        assert!(
            !text.contains("        branch="),
            "header should compact inherited shell indentation:\n{text}"
        );
    }

    #[test]
    fn feed_message_coalesces_runs_and_marks_breaks() {
        let mut h = ItemHistory::new();
        // Streaming deltas of the same kind → one contiguous run; only the
        // first item of a run sets `break_before` (and not the very first
        // item in an empty history).
        h.feed_message(MessageKind::Assistant, "Hel");
        h.feed_message(MessageKind::Assistant, "lo");
        h.feed_message(MessageKind::Reasoning, "thinking");
        h.feed_message(MessageKind::Assistant, "done");
        let breaks: Vec<(MessageKind, bool)> = h
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Message {
                    kind, break_before, ..
                } => Some((*kind, *break_before)),
                _ => None,
            })
            .collect();
        assert_eq!(
            breaks,
            vec![
                (MessageKind::Assistant, false), // first item ever
                (MessageKind::Assistant, false), // continues the run
                (MessageKind::Reasoning, true),  // kind change → new run
                (MessageKind::Assistant, true),  // kind change → new run
            ]
        );
        // Empty deltas are ignored.
        let before = h.items.len();
        h.feed_message(MessageKind::Assistant, "");
        assert_eq!(h.items.len(), before);
    }

    #[test]
    fn headless_message_history_renders_prose_and_tool_blocks() {
        // A headless session emits structured Message/ToolUse/ToolResult
        // with no PTY. The TUI must render the assistant prose (streamed as
        // deltas) interleaved with tool blocks — none of which is in a PTY.
        let mut h = ItemHistory::new();
        h.feed_message(MessageKind::Assistant, "Looking ");
        h.feed_message(MessageKind::Assistant, "into it.");
        h.feed_task_start("c1".into(), "shell".into(), "ls".into());
        h.feed_tool_result("c1", true, "file.txt".into());
        h.feed_message(MessageKind::Assistant, "Found one file.");

        let text = screen_text(h.replay(60, 16, 0).screen, 16, 60);
        assert!(
            text.contains("Looking into it."),
            "assistant prose missing:\n{text}"
        );
        assert!(text.contains("→ shell("), "tool block missing:\n{text}");
        assert!(text.contains("file.txt"), "tool output missing:\n{text}");
        assert!(
            text.contains("Found one file."),
            "post-tool prose missing:\n{text}"
        );
    }

    #[test]
    fn reasoning_and_user_messages_render() {
        let mut h = ItemHistory::new();
        h.feed_message(MessageKind::User, "do the thing");
        h.feed_message(MessageKind::Reasoning, "let me think");
        h.feed_message(MessageKind::Assistant, "ok");
        let text = screen_text(h.replay(60, 12, 0).screen, 12, 60);
        assert!(
            text.contains("❯ do the thing"),
            "user prompt missing:\n{text}"
        );
        assert!(text.contains("let me think"), "reasoning missing:\n{text}");
        assert!(text.contains("ok"), "assistant missing:\n{text}");
    }

    #[test]
    fn tool_header_uses_bright_tool_name_style() {
        let block = ToolBlock {
            call_id: "c1".into(),
            tool: Some("read_file".into()),
            args_summary: Some("/tmp/example".into()),
            output: None,
            ok: true,
            expanded: false,
            started_at: Instant::now(),
        };
        let rendered = String::from_utf8(synth_block(&block, 80).bytes).unwrap();
        assert!(
            rendered.contains("\x1b[1;92mread_file\x1b[0m"),
            "{rendered:?}"
        );
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
    fn expanded_tool_output_is_capped_inline() {
        let mut block = ToolBlock {
            call_id: "c1".into(),
            tool: Some("read_file".into()),
            args_summary: Some("large.log".into()),
            output: Some(
                (0..(TOOL_BLOCK_EXPANDED_LINES + 20))
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            ok: true,
            expanded: true,
            started_at: Instant::now(),
        };

        let rendered = String::from_utf8(synth_block(&block, 100).bytes).unwrap();
        assert!(
            rendered.contains(&format!(
                "showing {}/{}+ lines",
                TOOL_BLOCK_EXPANDED_LINES, TOOL_BLOCK_EXPANDED_LINES
            )),
            "{rendered:?}"
        );
        assert!(rendered.contains("click to collapse"), "{rendered:?}");
        assert!(
            rendered.contains(&format!("line {}", TOOL_BLOCK_EXPANDED_LINES - 1)),
            "{rendered:?}"
        );
        assert!(
            !rendered.contains(&format!("line {}", TOOL_BLOCK_EXPANDED_LINES)),
            "{rendered:?}"
        );

        block.expanded = false;
        let collapsed = String::from_utf8(synth_block(&block, 100).bytes).unwrap();
        assert!(
            collapsed.contains(&format!(
                "+{}+ lines",
                TOOL_BLOCK_EXPANDED_LINES - TOOL_BLOCK_COLLAPSED_LINES
            )),
            "{collapsed:?}"
        );
    }

    #[test]
    fn tool_result_history_preview_is_bounded() {
        let huge = (0..10_000usize)
            .map(|i| format!("line {i} {}", "x".repeat(2_000)))
            .collect::<Vec<_>>()
            .join("\n");
        let preview = tool_output_preview_for_history(&huge);
        assert!(
            preview.len() < 120_000,
            "preview should be bounded, got {} bytes",
            preview.len()
        );
        assert!(preview.contains("[output truncated for TUI preview]"));
        assert!(preview.contains("line 0"));
        assert!(!preview.contains("line 9999"));
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
            eprintln!(
                "batch {i}: 1000 events in {us} µs ({} ns/event)",
                us * 1000 / 1000
            );
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

    // ============================================================
    // Per-harness regression suite
    //
    // Each agentd-supported harness writes to the PTY in a different
    // shape (shell = plain stdout, claude = alt-screen TUI, zarvis =
    // chat + tool blocks, orchestrator = chat + EditorState, codex =
    // normal-screen TUI with accumulated history). Bugs that show up
    // in one frequently don't show up in another — these tests pin
    // each pattern's expected behavior so a future change can't
    // silently regress one harness while the other three still look
    // fine in manual testing.
    //
    // Scenarios per harness:
    //   * "after bootstrap" — TUI restart path: a single big
    //     `feed_pty` (the `pty_replay` snapshot from the daemon)
    //     followed by `replay`. Asserts the screen reflects the
    //     content, no panic, no garbage.
    //   * "after resize" — pane dims change between `replay` calls.
    //     Asserts the screen still reflects the content correctly.
    //   * "typing perf" — many small events arriving on top of a
    //     populated history; per-event cost stays bounded.
    //   * "resize perf" — cost of resizing an already-populated
    //     session. Currently passes for sessions whose accumulated
    //     `pending_chunk` is small (shell / zarvis / orchestrator /
    //     claude-via-alt-screen) and FAILS for codex (which
    //     accumulates a lot of normal-screen content).
    // ============================================================

    use std::time::Instant;

    /// Heuristic for "did this `replay` re-feed the whole history?".
    /// Returns the wall time the call took. If a rebuild fired it
    /// scales with history size (10s of ms at MB scale); incremental
    /// `set_size`-only calls return in single-digit µs.
    fn time_replay(h: &mut ItemHistory, cols: u16, rows: u16) -> u128 {
        let t = Instant::now();
        let _ = h.replay(cols, rows, 0);
        t.elapsed().as_micros()
    }

    // ----- shell (`bash`-style: plain stdout, small history) -----

    fn shell_feed_minimal(h: &mut ItemHistory) {
        // Tiny shell session: prompt + a couple of commands.
        h.feed_pty(b"$ ls\r\nfile_a  file_b  file_c\r\n$ pwd\r\n/home/user\r\n$ ");
    }

    #[test]
    fn shell_renders_after_bootstrap() {
        let mut h = ItemHistory::new();
        shell_feed_minimal(&mut h);
        let out = h.replay(80, 24, 0);
        let cell = out
            .screen
            .cell(0, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "$", "shell prompt should be on row 0 col 0");
    }

    #[test]
    fn shell_renders_after_resize() {
        let mut h = ItemHistory::new();
        shell_feed_minimal(&mut h);
        let _ = h.replay(80, 24, 0);
        let out = h.replay(120, 30, 0);
        let cell = out
            .screen
            .cell(0, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "$", "content survives shell session resize");
    }

    #[test]
    fn shell_typing_is_fast() {
        let mut h = ItemHistory::new();
        shell_feed_minimal(&mut h);
        let _ = h.replay(80, 24, 0);
        let t = Instant::now();
        for _ in 0..200 {
            h.feed_pty(b"x");
            let _ = h.replay(80, 24, 0);
        }
        let us = t.elapsed().as_micros();
        assert!(us < 50_000, "200 typing events at shell-scale: {us} µs");
    }

    // ----- claude (alt-screen TUI) -----

    fn claude_feed_alt_screen(h: &mut ItemHistory) {
        // Enter alt screen, draw a few status lines, leave the
        // cursor at a known position. Real claude does much more,
        // but the *shape* (alt-screen + small bounded content) is
        // what matters for our parser.
        h.feed_pty(b"\x1b[?1049h"); // enter alt screen
        h.feed_pty(b"\x1b[H"); // cursor home
        h.feed_pty(b"claude\r\n");
        h.feed_pty(b"> hello\r\n");
        h.feed_pty(b"\x1b[2;1H"); // status row
    }

    #[test]
    fn claude_renders_after_bootstrap() {
        let mut h = ItemHistory::new();
        claude_feed_alt_screen(&mut h);
        let out = h.replay(80, 24, 0);
        let cell = out
            .screen
            .cell(0, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "c", "claude banner present in alt-screen buffer");
    }

    #[test]
    fn claude_renders_after_resize() {
        let mut h = ItemHistory::new();
        claude_feed_alt_screen(&mut h);
        let _ = h.replay(80, 24, 0);
        let out = h.replay(120, 30, 0);
        let cell = out
            .screen
            .cell(0, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert_eq!(cell, "c", "claude content survives resize");
    }

    /// claude's alt-screen content is bounded by the viewport, so
    /// resizing should not be expensive even if the session has
    /// existed for a while.
    #[test]
    fn claude_resize_is_cheap() {
        let mut h = ItemHistory::new();
        claude_feed_alt_screen(&mut h);
        // Simulate some passage of time / additional small redraws.
        for i in 0..200 {
            h.feed_pty(b"\x1b[H");
            h.feed_pty(format!("line {i}\r\n").as_bytes());
        }
        let _ = h.replay(80, 24, 0);
        let t = Instant::now();
        let _ = h.replay(120, 30, 0);
        let us = t.elapsed().as_micros();
        assert!(
            us < 5_000,
            "claude resize too slow: {us} µs (expected <5ms)"
        );
    }

    // ----- zarvis (chat text + occasional tool blocks) -----

    fn zarvis_feed_chat(h: &mut ItemHistory) {
        // A few chat exchanges. zarvis bytes look much like a plain
        // CLI for the chat portion — the special bits are
        // tool-block events (TaskStart/feed_tool_use), not OSC
        // markers anymore. Use OSC here to exercise the tool-block
        // path so this hits `replay_full`.
        h.feed_pty(b"\xe2\x9d\xaf hi\r\n");
        h.feed_pty(b"\xe2\x97\x8f hello -- what can I do?\r\n");
        h.feed_tool_use("shell".into(), "ls".into());
        h.feed_pty(b"\x1b]7700;open;call=z1\x07x\x1b]7700;close;call=z1\x07");
        h.feed_tool_result("z1", true, "file_a\nfile_b".into());
        h.feed_pty(b"\xe2\x97\x8f done.\r\n");
    }

    /// Daemon-restart restore path for a zarvis session.
    ///
    /// `bootstrap_terminal` reads pty.log via `pty_replay` and
    /// feeds the bytes to a fresh `ItemHistory`. The OSC fences
    /// inside the bytes create `ToolBlock` items, but the
    /// structured `tool` / `args` / `output` data lives in
    /// transcript events — which the daemon does NOT re-broadcast
    /// on subscribe. So without an explicit transcript replay step,
    /// each block reconstructed from the fences would render as
    /// `→ ?` with no body (the dim-styled args + `[+N lines]`
    /// footer the user sees live would disappear).
    ///
    /// The fix is in `bootstrap_terminal` (`app.rs`): after
    /// `pty_replay` it also fetches the transcript and routes
    /// `ToolUse` / `ToolResult` events through
    /// `feed_tool_use` / `feed_tool_result`, which (via the
    /// FIFO `pending_block_hydrations` queue and the call_id match
    /// in `feed_tool_result`) reattach the structured data to the
    /// blocks created by `feed_pty`.
    ///
    /// This test verifies that path end-to-end at the items-model
    /// layer: feed pty bytes first (matching how `bootstrap_terminal`
    /// does it), THEN replay transcript-style events, and check the
    /// reconstructed `ToolBlock` carries the same `tool` /
    /// `args_summary` / `output` it would have had if it'd been
    /// built live.
    #[test]
    fn zarvis_restart_restore_rehydrates_tool_block_details() {
        // Pre-restart: build the full live byte stream the adapter
        // would have produced, mirroring `interactive.rs`'s actual
        // sequence: tool_block_open → tool_use header → tool_result_body
        // → tool_block_close.
        let mut live_bytes: Vec<u8> = Vec::new();
        live_bytes.extend_from_slice(b"\xe2\x9d\xaf list the cwd\r\n");
        // OSC open for call "c1"
        live_bytes.extend_from_slice(b"\x1b]7700;open;call=c1\x07");
        // tool_use header (dim args)
        live_bytes
            .extend_from_slice(b"\r\n\x1b[1;32m\xe2\x86\x92 shell\x1b[0m\x1b[2m(ls)\x1b[0m\r\n");
        // tool_result_body (dim content + the `[+N lines]` footer)
        live_bytes.extend_from_slice(b"  \x1b[1;32m\xe2\x9c\x93\x1b[0m  \x1b[2mfile_a\x1b[0m\r\n");
        live_bytes.extend_from_slice(b"     \x1b[2mfile_b\x1b[0m\r\n");
        live_bytes.extend_from_slice(
            b"     \x1b[2;36m[+5 lines \xe2\x80\x94 click to expand]\x1b[0m\r\n",
        );
        live_bytes.extend_from_slice(b"\x1b]7700;close;call=c1\x07");
        live_bytes.extend_from_slice(b"\xe2\x97\x8f done.\r\n");

        // Restore path: feed pty bytes first (mirrors
        // `bootstrap_terminal`'s `feed_pty`), THEN replay the
        // structured transcript events as the bootstrap fix does
        // via `feed_tool_use` / `feed_tool_result`. The
        // `pending_block_hydrations` FIFO inside `ItemHistory`
        // attaches each ToolUse to the next pending block (by
        // arrival order); `feed_tool_result` matches by call_id.
        let mut h = ItemHistory::new();
        h.feed_pty(&live_bytes);
        // Transcript replay: one ToolUse + one ToolResult for "c1".
        h.feed_tool_use("shell".into(), "ls".into());
        let full_output: String = (0..8)
            .map(|i| format!("file_{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.feed_tool_result("c1", true, full_output.clone());

        let has_tool_block = h.items.iter().any(|i| matches!(i, Item::ToolBlock(_)));
        assert!(
            has_tool_block,
            "post-restore feed should reconstruct the ToolBlock from OSC fences"
        );
        let block = h
            .items
            .iter()
            .find_map(|i| match i {
                Item::ToolBlock(b) => Some(b.clone()),
                _ => None,
            })
            .expect("a tool block exists");
        // The fix: structured fields are now populated, so
        // `synth_block` will re-emit the dim-styled args + output
        // + `[+N lines]` footer just like it does for a live
        // session.
        assert_eq!(
            block.tool.as_deref(),
            Some("shell"),
            "ToolBlock.tool should be filled by transcript replay"
        );
        assert_eq!(
            block.args_summary.as_deref(),
            Some("ls"),
            "ToolBlock.args_summary should be filled by transcript replay"
        );
        assert_eq!(
            block.output.as_deref(),
            Some(full_output.as_str()),
            "ToolBlock.output should be filled by transcript replay"
        );
        assert_eq!(block.ok, true);
    }

    #[test]
    fn zarvis_renders_after_bootstrap() {
        let mut h = ItemHistory::new();
        zarvis_feed_chat(&mut h);
        let out = h.replay(80, 24, 0);
        // Don't assert exact cells (tool-block synth shifts rows);
        // just confirm we have a block recorded.
        assert!(
            !out.blocks.is_empty(),
            "zarvis tool block should be hit-testable"
        );
    }

    #[test]
    fn zarvis_renders_after_resize() {
        let mut h = ItemHistory::new();
        zarvis_feed_chat(&mut h);
        let _ = h.replay(80, 24, 0);
        let out = h.replay(120, 30, 0);
        assert!(!out.blocks.is_empty(), "zarvis blocks survive resize");
    }

    /// Regression: when the zarvis editor pane grows (user types
    /// → multi-line input), `chat_area.height` shrinks, so the
    /// next `replay` call passes a smaller `rows`. The cached
    /// parser must absorb that via `set_size` without sending any
    /// cursor-relocating escapes — otherwise streaming bytes that
    /// arrive after the resize land at row 0 and overwrite history.
    ///
    /// This test uses a chat-only session (no tool blocks) so it
    /// exercises `replay_cached`, the path where the
    /// previously-shipped `\x1b[r\x1b[?7h\x1b[?6l\x1b[4l` reset
    /// would fire after every height change and visibly clobber
    /// the top row.
    #[test]
    fn replay_cached_rows_shrink_does_not_reset_cursor() {
        let mut h = ItemHistory::new();
        // Plenty of history so the parser cursor parks at the
        // bottom row.
        for i in 0..50 {
            h.feed_pty(format!("history line {i:02}\r\n").as_bytes());
        }
        // Render at the initial size (editor pane = 1 line, chat
        // area = 23 rows).
        let _ = h.replay(80, 23, 0);

        // Editor pane grows (user typed a second line). chat_area
        // shrinks to 22 rows.
        let _ = h.replay(80, 22, 0);

        // Streaming byte arrives — the next chunk from the
        // adapter. If the resize path injected a cursor-resetting
        // escape, this would land at row 0.
        const MARKER: &str = "POST_RESIZE_BYTE";
        h.feed_pty(format!("{MARKER}\r\n").as_bytes());
        let out = h.replay(80, 22, 0);

        let row0: String = (0..80)
            .filter_map(|c| out.screen.cell(0, c).map(|x| x.contents()))
            .collect();
        assert!(
            !row0.contains(MARKER),
            "post-resize streaming byte landed at row 0 \
             (cursor-reset side effect from the resize escape \
             sequence): row 0 = {row0:?}"
        );
    }

    /// Regression: closing the minibuffer / remote-control dialog grows
    /// the main PTY pane. That used to replay all accumulated PTY history
    /// synchronously so newly exposed rows were immediately repopulated;
    /// long codex sessions could freeze the TUI for seconds. Row-only
    /// growth should be a cheap `set_size` that preserves the live parser
    /// and lets subsequent PTY output / SIGWINCH repaint exposed rows.
    #[test]
    fn replay_cached_rows_grow_does_not_reprocess_history() {
        let mut h = ItemHistory::new();
        for i in 0..3_000 {
            h.feed_pty(format!("history line {i:02}\r\n").as_bytes());
        }
        const MARKER: &str = "BOTTOM_MARKER";
        h.feed_pty(MARKER.as_bytes());

        // Completion popup visible: chat area is shorter.
        let _ = h.replay(80, 5, 0);
        let processed_before = h
            .cached
            .as_ref()
            .map(|cache| (cache.processed_count, cache.pending_consumed))
            .expect("cached parser should exist");

        // Completion popup hidden / minibuffer closed: chat area grows.
        // This must not reset the cache and reprocess all prior bytes.
        let out = h.replay(80, 10, 0);
        assert_eq!(out.screen.size(), (10, 80));
        let processed_after = h
            .cached
            .as_ref()
            .map(|cache| (cache.processed_count, cache.pending_consumed))
            .expect("cached parser should still exist");
        assert_eq!(
            processed_after, processed_before,
            "row-only growth should resize the cached parser without replaying history"
        );
    }

    /// Same row-growth regression as above, but for sessions with
    /// tool blocks. Those use `replay_full`, which has separate
    /// cache-invalidation rules for block hit-test layout and used to
    /// force a full rebuild when only `rows` increased.
    #[test]
    fn replay_full_rows_grow_does_not_reprocess_history() {
        let mut h = ItemHistory::new();
        for i in 0..250 {
            h.feed_pty(format!("prompt {i}\r\n").as_bytes());
            h.feed_tool_use("shell".into(), format!("echo {i}"));
            h.feed_pty(
                format!("\x1b]7700;open;call=z{i}\x07x\x1b]7700;close;call=z{i}\x07").as_bytes(),
            );
            h.feed_tool_result(&format!("z{i}"), true, format!("result {i}"));
            h.feed_pty(format!("done {i}\r\n").as_bytes());
        }

        // Popup visible: main pane is shorter.
        let _ = h.replay(80, 5, 0);
        let before = h
            .cached
            .as_ref()
            .map(|cache| {
                (
                    cache.processed_count,
                    cache.pending_consumed,
                    cache.signatures.len(),
                    cache.item_layouts.len(),
                )
            })
            .expect("cached parser should exist");

        // Popup hidden: rows grow, columns and history are unchanged.
        let out = h.replay(80, 10, 0);
        assert_eq!(out.screen.size(), (10, 80));
        assert!(
            !out.blocks.is_empty(),
            "tool block hit rects survive row growth"
        );
        let after = h
            .cached
            .as_ref()
            .map(|cache| {
                (
                    cache.processed_count,
                    cache.pending_consumed,
                    cache.signatures.len(),
                    cache.item_layouts.len(),
                )
            })
            .expect("cached parser should still exist");
        assert_eq!(
            after, before,
            "row-only growth should resize replay_full's parser without rebuilding"
        );
    }

    /// User-reported regression: after loading a zarvis session
    /// with history, the *next* PTY bytes (user input / agent
    /// output) land at the top of the viewport instead of after
    /// the existing history. This test reproduces the scenario.
    #[test]
    fn zarvis_new_content_after_bootstrap_does_not_overwrite_first_row() {
        let mut h = ItemHistory::new();
        // Bootstrap: load history (mirrors what bootstrap_terminal
        // does after `pty_replay` returns the snapshot bytes).
        zarvis_feed_chat(&mut h);
        // Add several screens of additional chat so the viewport
        // is "full" — exercises the scrollback path where the
        // cursor should be parked at the bottom row after history.
        for i in 0..30 {
            h.feed_pty(format!("\x1b[36muser\x1b[0m: history msg {i}\r\n").as_bytes());
            h.feed_pty(format!("\x1b[35massistant\x1b[0m: reply {i}\r\n").as_bytes());
        }
        // Initial render — what the user sees right after opening
        // the session.
        let _ = h.replay(80, 24, 0);

        // Now new bytes arrive — user typed something OR zarvis
        // emitted a follow-up message. These are exactly the bytes
        // the user reported as "overwriting the first row".
        const MARKER: &str = "ZARVIS_NEW_LINE_MARKER";
        h.feed_pty(format!("\x1b[36muser\x1b[0m: {MARKER}\r\n").as_bytes());
        let out = h.replay(80, 24, 0);

        // Read row 0 of the viewport. The marker MUST NOT appear
        // there — the bug is "new content lands at row 0 and
        // overwrites whatever history was there".
        let row0: String = (0..80)
            .filter_map(|c| out.screen.cell(0, c).map(|x| x.contents()))
            .collect();
        assert!(
            !row0.contains(MARKER),
            "new content landed at the top of the viewport \
             (overwriting history) instead of after the history. \
             row 0 = {row0:?}"
        );

        // Sanity: the marker must appear *somewhere* in the
        // viewport, otherwise the test is vacuous (e.g. scrolled
        // past).
        let found_at: Option<u16> = (0..24).find(|&r| {
            let line: String = (0..80)
                .filter_map(|c| out.screen.cell(r, c).map(|x| x.contents()))
                .collect();
            line.contains(MARKER)
        });
        assert!(
            found_at.is_some(),
            "new content should be visible somewhere in the viewport"
        );
        eprintln!("new content rendered at row {:?} / 24", found_at);
    }

    #[test]
    fn zarvis_resize_steady_state_is_cheap() {
        let mut h = ItemHistory::new();
        zarvis_feed_chat(&mut h);
        // Warm the cache at the post-bootstrap state.
        let _ = h.replay(80, 24, 0);
        let _ = h.replay(80, 24, 0);
        // Resize (cols change). Since this session has tool blocks
        // it goes through `replay_full`, which uses signature-based
        // incremental — but cols change does invalidate at the
        // moment. Document the current bound rather than asserting
        // it's free.
        let t = Instant::now();
        let _ = h.replay(120, 30, 0);
        let us = t.elapsed().as_micros();
        assert!(us < 5_000, "zarvis resize too slow: {us} µs");
    }

    /// PERF / regression: typing into a long zarvis session was
    /// "super laggy". zarvis history has tool blocks, so it takes the
    /// `replay_full` path. Unlike the streaming-bootstrap case (where
    /// the bulk sits in `pending_chunk` and is handled incrementally),
    /// a real session FLUSHES chat into many `Item::PtyChunk` entries
    /// interleaved with `Item::ToolBlock`s. `replay_full` re-walked
    /// every item calling `count_visible_lines` / `synth_block` on
    /// every frame to position tool-block hit rects — O(total history
    /// bytes) per frame. Each keystroke triggers a redraw (via the
    /// EditorState round-trip), so the per-frame cost was paid on
    /// every character → the lag.
    ///
    /// Steady-state re-render (same size, nothing changed) must be
    /// cheap regardless of history size.
    #[test]
    fn zarvis_steady_state_render_is_cheap_with_many_items() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        // 300 chat-then-tool cycles. Each chat span is flushed into a
        // PtyChunk item when the following tool block opens, so we end
        // up with ~600 sizable items — the shape of a long working
        // session, not one giant pending chunk.
        for i in 0..300u32 {
            let mut chat = Vec::with_capacity(1500);
            for j in 0..15 {
                chat.extend_from_slice(
                    format!(
                        "\x1b[33mline {i}.{j} of accumulated assistant chat content\x1b[0m\r\n"
                    )
                    .as_bytes(),
                );
            }
            h.feed_pty(&chat);
            let call = format!("c{i}");
            h.feed_tool_use("shell".into(), format!("cmd {i}"));
            h.feed_pty(
                format!("\x1b]7700;open;call={call}\x07out\x1b]7700;close;call={call}\x07")
                    .as_bytes(),
            );
            h.feed_tool_result(&call, true, "ok".into());
        }

        // Warm the cache so we measure steady state, not the one-time
        // post-bootstrap rebuild.
        let _ = h.replay(cols, rows, 0);
        let _ = h.replay(cols, rows, 0);

        // Steady state: same size, no new bytes, no signature changes —
        // this is what a keystroke's redraw does. Should be cheap.
        let frames = 100u32;
        let t = Instant::now();
        for _ in 0..frames {
            let _ = h.replay(cols, rows, 0);
        }
        let total_us = t.elapsed().as_micros();
        let per_frame = total_us / frames as u128;
        eprintln!("zarvis steady-state: {per_frame} µs/frame ({total_us} µs / {frames})");

        // A no-op re-render must not re-scan the whole history. Loose
        // bound to avoid CI flakiness; the pre-fix cost was ~10-50×
        // this on the same hardware.
        assert!(
            per_frame < 300,
            "zarvis steady-state render too slow: {per_frame} µs/frame — replay_full is re-scanning history every frame"
        );
    }

    #[test]
    fn zarvis_tool_expand_collapse_rebuilds_only_retained_suffix() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        for i in 0..1_200u32 {
            let mut chat = Vec::with_capacity(900);
            for j in 0..8 {
                chat.extend_from_slice(
                    format!(
                        "\x1b[33mhistory {i}.{j} before the clicked tool block\x1b[0m\r\n"
                    )
                    .as_bytes(),
                );
            }
            h.feed_pty(&chat);
            let call = format!("c{i}");
            h.feed_tool_use("shell".into(), format!("cmd {i}"));
            h.feed_pty(
                format!("\x1b]7700;open;call={call}\x07out\x1b]7700;close;call={call}\x07")
                    .as_bytes(),
            );
            h.feed_tool_result(&call, true, "ok".into());
        }

        let target = "target";
        h.feed_tool_use("read_file".into(), "produce large output".into());
        h.feed_pty(
            format!("\x1b]7700;open;call={target}\x07out\x1b]7700;close;call={target}\x07")
                .as_bytes(),
        );
        let output = (0..600u32)
            .map(|i| format!("expanded output line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.feed_tool_result(target, true, output);

        // Warm the cache in the collapsed state.
        let _ = h.replay(cols, rows, 0);
        let _ = h.replay(cols, rows, 0);

        assert!(h.toggle_block(target));
        let expand_t = Instant::now();
        let expanded = h.replay(cols, rows, 0);
        let expand_us = expand_t.elapsed().as_micros();
        assert!(
            expanded
                .blocks
                .iter()
                .any(|hit| hit.call_id == target),
            "expanded block hit rect should survive suffix rebuild: {:?}",
            expanded.blocks
        );

        assert!(h.toggle_block(target));
        let collapse_t = Instant::now();
        let collapsed = h.replay(cols, rows, 0);
        let collapse_us = collapse_t.elapsed().as_micros();
        assert!(
            collapsed
                .blocks
                .iter()
                .any(|hit| hit.call_id == target),
            "collapsed block hit rect should survive suffix rebuild"
        );

        eprintln!(
            "zarvis expand/collapse after long history: expand {expand_us} µs, collapse {collapse_us} µs"
        );
        assert!(
            expand_us < 80_000,
            "tool expand replay too slow after long history: {expand_us} µs"
        );
        assert!(
            collapse_us < 80_000,
            "tool collapse replay too slow after long history: {collapse_us} µs"
        );
    }

    #[test]
    fn zarvis_tool_start_reuses_already_rendered_pending_text() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        for i in 0..800u32 {
            let mut chat = Vec::with_capacity(900);
            for j in 0..8 {
                chat.extend_from_slice(
                    format!("\x1b[33mhistory {i}.{j} before live pending text\x1b[0m\r\n")
                        .as_bytes(),
                );
            }
            h.feed_pty(&chat);
            let call = format!("c{i}");
            h.feed_task_start(call.clone(), "shell".into(), format!("cmd {i}"));
            h.feed_tool_result(&call, true, "ok".into());
        }
        let _ = h.replay(cols, rows, 0);
        let _ = h.replay(cols, rows, 0);

        let mut live_pending = Vec::with_capacity(40_000);
        for i in 0..900u32 {
            live_pending.extend_from_slice(
                format!("\x1b[36massistant live pending line {i}\x1b[0m\r\n").as_bytes(),
            );
        }
        h.feed_pty(&live_pending);
        let _ = h.replay(cols, rows, 0);

        let target = "new-tool";
        h.feed_task_start(target.into(), "read_file".into(), "src/main.rs".into());
        let t = Instant::now();
        let rendered = h.replay(cols, rows, 0);
        let tool_start_us = t.elapsed().as_micros();
        assert!(
            rendered.blocks.iter().any(|hit| hit.call_id == target),
            "new tool block hit rect should render after pending flush"
        );
        eprintln!("zarvis tool start after live pending: {tool_start_us} µs");
        assert!(
            tool_start_us < 80_000,
            "tool start replay too slow after pending flush: {tool_start_us} µs"
        );
    }

    #[test]
    fn zarvis_tool_start_reuses_partially_rendered_pending_text() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        for i in 0..1_200u32 {
            let mut chat = Vec::with_capacity(900);
            for j in 0..8 {
                chat.extend_from_slice(
                    format!("\x1b[33mhistory {i}.{j} before partial pending\x1b[0m\r\n")
                        .as_bytes(),
                );
            }
            h.feed_pty(&chat);
            let call = format!("c{i}");
            h.feed_task_start(call.clone(), "shell".into(), format!("cmd {i}"));
            h.feed_tool_result(&call, true, "ok".into());
        }
        let _ = h.replay(cols, rows, 0);
        let _ = h.replay(cols, rows, 0);

        let rendered_prefix = (0..600u32)
            .map(|i| format!("\x1b[36malready rendered pending line {i}\x1b[0m\r\n"))
            .collect::<String>();
        h.feed_pty(rendered_prefix.as_bytes());
        let _ = h.replay(cols, rows, 0);

        let unrendered_suffix = (0..600u32)
            .map(|i| format!("\x1b[35mnot yet rendered pending line {i}\x1b[0m\r\n"))
            .collect::<String>();
        h.feed_pty(unrendered_suffix.as_bytes());

        let target = "partial-new-tool";
        h.feed_task_start(target.into(), "read_file".into(), "src/main.rs".into());
        let t = Instant::now();
        let rendered = h.replay(cols, rows, 0);
        let tool_start_us = t.elapsed().as_micros();
        assert!(
            rendered.blocks.iter().any(|hit| hit.call_id == target),
            "new tool block hit rect should render after partial pending flush"
        );
        eprintln!("zarvis tool start after partial pending: {tool_start_us} µs");
        assert!(
            tool_start_us < 80_000,
            "tool start replay too slow after partial pending flush: {tool_start_us} µs"
        );
    }

    #[test]
    fn ungrouped_tool_update_stays_bounded() {
        use std::time::Instant;
        let mut h = ItemHistory::new();
        let cols = 100u16;
        let rows = 30u16;

        for i in 0..2_000u32 {
            let call = format!("g{i}");
            h.feed_task_start(call.clone(), "shell".into(), format!("cmd {i}"));
            h.feed_tool_result(&call, true, "ok".into());
        }
        let _ = h.replay(cols, rows, 0);
        let _ = h.replay(cols, rows, 0);

        let call = "group-new";
        h.feed_task_start(call.into(), "shell".into(), "cmd new".into());
        let start_t = Instant::now();
        let started = h.replay(cols, rows, 0);
        let start_us = start_t.elapsed().as_micros();
        assert!(
            started.blocks.iter().any(|hit| hit.call_id == call),
            "new tool hit rect should render after append"
        );

        h.feed_tool_result(call, true, "ok".into());
        let result_t = Instant::now();
        let finished = h.replay(cols, rows, 0);
        let result_us = result_t.elapsed().as_micros();
        assert!(
            finished.blocks.iter().any(|hit| hit.call_id == call),
            "new tool hit rect should survive result"
        );
        eprintln!(
            "ungrouped tool update after 2000 calls: start {start_us} µs, result {result_us} µs"
        );
        assert!(
            start_us < 80_000,
            "ungrouped tool start replay too slow: {start_us} µs"
        );
        assert!(
            result_us < 80_000,
            "ungrouped tool result replay too slow: {result_us} µs"
        );
    }

    #[test]
    fn live_tool_start_after_render_stays_append_only() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "shell".into(), "cmd a".into());
        h.feed_tool_result("A", true, "ok".into());
        let _ = h.replay(100, 30, 0);

        h.feed_task_start("B".into(), "shell".into(), "cmd b".into());
        assert_eq!(h.items.len(), 2, "{:?}", h.items);
        assert!(matches!(h.items[0], Item::ToolBlock(_)), "{:?}", h.items);
        assert!(matches!(h.items[1], Item::ToolBlock(_)), "{:?}", h.items);

        let before = h
            .cached
            .as_ref()
            .map(|cache| cache.processed_count)
            .expect("cache warmed");
        let _ = h.replay(100, 30, 0);
        let after = h.cached.as_ref().map(|cache| cache.processed_count);
        assert_eq!(before, 1);
        assert_eq!(after, Some(2));
    }

    #[test]
    fn live_late_tool_use_hydration_after_render_stays_append_only() {
        let mut h = ItemHistory::new();
        h.feed_task_start("A".into(), "shell".into(), "cmd a".into());
        h.feed_tool_result("A", true, "ok".into());
        let _ = h.replay(100, 30, 0);

        h.feed_pty(b"\x1b]7700;open;call=B\x07inline");
        h.feed_tool_use("shell".into(), "cmd b".into());
        h.feed_tool_result("B", true, "ok".into());
        assert_eq!(h.items.len(), 2, "{:?}", h.items);
        assert!(matches!(h.items[0], Item::ToolBlock(_)), "{:?}", h.items);
        assert!(matches!(h.items[1], Item::ToolBlock(_)), "{:?}", h.items);
    }

    // ----- orchestrator / minibuffer (zarvis-like content, no
    // tool blocks in the common case) -----

    fn orchestrator_feed(h: &mut ItemHistory) {
        // Orchestrator chat + a single observation echo. No tool
        // blocks (those are rare in orchestrator). Treated as a
        // shell-like history through `replay_cached`.
        for _ in 0..50 {
            h.feed_pty(b"orchestrator observation log entry...\r\n");
        }
        h.feed_pty(b"> ");
    }

    #[test]
    fn orchestrator_renders_after_bootstrap() {
        let mut h = ItemHistory::new();
        orchestrator_feed(&mut h);
        let out = h.replay(60, 6, 0);
        let cell = out
            .screen
            .cell(5, 0)
            .map(|c| c.contents())
            .unwrap_or_default();
        assert!(!cell.is_empty(), "orchestrator panel last row populated");
    }

    /// Regression: `C-x x` on a narrow / tall layout used to crash
    /// the TUI because the orchestrator panel's chat area can
    /// shrink to 1 row (editor pane absorbs the rest), and
    /// vt100-0.16.2's `col_wrap` underflows when rows / cols is 1
    /// and a wide character forces a wrap.
    ///
    /// `ItemHistory::replay` now floors parser geometry to
    /// `VT100_MIN_DIM` (2) at every callsite. This test feeds a
    /// PTY stream containing a wide char + wrap, calls `replay`
    /// with degenerate dims, and just checks we don't panic.
    #[test]
    fn replay_with_degenerate_dims_does_not_panic() {
        let mut h = ItemHistory::new();
        // Wide char ("中") followed by ASCII that will need to
        // wrap. Combined with `\r\n` markers this exercises the
        // col_wrap path that was crashing.
        h.feed_pty(b"hi\xe4\xb8\xad\xe6\x96\x87wrap-stress-with-narrow-screen\r\n");
        for (cols, rows) in [(1u16, 1u16), (1, 2), (2, 1), (1, 80), (80, 1), (2, 2)] {
            let _ = h.replay(cols, rows, 0);
        }
    }

    #[test]
    fn orchestrator_resize_is_cheap() {
        let mut h = ItemHistory::new();
        orchestrator_feed(&mut h);
        let _ = h.replay(60, 6, 0);
        let t = Instant::now();
        let _ = h.replay(80, 8, 0);
        let us = t.elapsed().as_micros();
        assert!(us < 5_000, "orchestrator panel resize too slow: {us} µs");
    }

    // ----- codex (normal-screen, accumulated history) -----

    fn codex_feed_long_session(h: &mut ItemHistory) {
        // Codex doesn't use alt-screen by default; its bytes stack
        // up in the main scrollback. After a real session of any
        // length the `pending_chunk` is megabytes of conversation +
        // tool output. Simulate that scale here.
        let mut buf = Vec::with_capacity(1_500_000);
        for i in 0..30_000u32 {
            buf.extend_from_slice(b"\x1b[36muser\x1b[0m: hi from line ");
            buf.extend_from_slice(i.to_string().as_bytes());
            buf.extend_from_slice(b"\r\n");
            buf.extend_from_slice(b"\x1b[32massistant\x1b[0m: doing the thing\r\n");
        }
        h.feed_pty(&buf);
    }

    /// Count how many of the visible screen rows have ANY non-blank
    /// cell. Useful as a "history is on screen" check that doesn't
    /// depend on the exact cursor position.
    fn populated_rows(screen: &vt100::Screen, rows: u16, cols: u16) -> usize {
        (0..rows)
            .filter(|&r| {
                (0..cols).any(|c| {
                    screen
                        .cell(r, c)
                        .map(|cell| !cell.contents().is_empty() && cell.contents() != " ")
                        .unwrap_or(false)
                })
            })
            .count()
    }

    #[test]
    fn codex_renders_after_bootstrap() {
        let mut h = ItemHistory::new();
        codex_feed_long_session(&mut h);
        let out = h.replay(80, 24, 0);
        let n = populated_rows(out.screen, 24, 80);
        assert!(
            n >= 20,
            "expected most rows to show codex history, got {n}/24 populated"
        );
    }

    #[test]
    fn codex_renders_after_resize() {
        let mut h = ItemHistory::new();
        codex_feed_long_session(&mut h);
        let _ = h.replay(80, 24, 0);
        let out = h.replay(120, 30, 0);
        let n = populated_rows(out.screen, 30, 120);
        assert!(
            n >= 20,
            "history should still be on screen after resize, got {n}/30 populated"
        );
    }

    /// Codex's cols-change resize is O(history) at the items-model
    /// layer — we rebuild the parser from scratch so prior content
    /// re-wraps at the new width. That's intentional and unavoidable
    /// for correctness (vt100 doesn't reflow soft-wrapped lines via
    /// `set_size`). The user-visible "history replay" cascade the
    /// user reports is NOT this O(history) call — it's the
    /// per-PtyChunk render in the main app loop while codex's
    /// SIGWINCH redraw streams in. That bug is fixed at the
    /// render-loop level (drain pending notifications before each
    /// `terminal.draw`); see
    /// `codex_sigwinch_redraw_arrives_as_chunks_user_sees_animation`
    /// for the mechanism repro.
    ///
    /// This test just documents the bound: even O(history) should
    /// stay well under 1 s for the ~1.5 MB session size we test
    /// with, on a debug build.
    #[test]
    fn codex_resize_bound_is_reasonable() {
        let mut h = ItemHistory::new();
        codex_feed_long_session(&mut h);
        let _ = h.replay(80, 24, 0); // warm
        let resize_us = time_replay(&mut h, 120, 30);
        eprintln!("codex resize wall time at ~1.5MB history: {resize_us} µs");
        assert!(
            resize_us < 1_000_000,
            "codex resize unreasonably slow at {resize_us} µs"
        );
    }

    /// Counterpart for the *initial render* path: a long codex
    /// session's pty_log gets bootstrapped via one big `feed_pty`
    /// (mimics what `bootstrap_terminal` does after TUI restart),
    /// then a single `replay` builds the first frame. Measures
    /// just the first-frame cost — should be one-time, not
    /// recurring.
    #[test]
    fn codex_initial_render_after_restart_is_bounded() {
        let mut h = ItemHistory::new();
        codex_feed_long_session(&mut h);
        // First replay — this is what `bootstrap_terminal` triggers
        // on TUI restart for a focused/pinned codex session.
        let t = Instant::now();
        let out = h.replay(80, 24, 0);
        let first_us = t.elapsed().as_micros();
        eprintln!("codex initial render after restart: {first_us} µs");
        let n = populated_rows(out.screen, 24, 80);
        assert!(
            n >= 20,
            "codex restart should show populated viewport, got {n}/24"
        );
        // First render absorbs the whole pty_log — expect tens to
        // a few hundred ms for ~1.5 MB through vt100 in a debug
        // build. This is unavoidable (we must feed the bytes once).
        // The bound here is loose enough not to flake on a loaded
        // machine but still flags the multi-second rebuild case.
        assert!(
            first_us < 500_000,
            "codex initial render absurdly slow: {first_us} µs"
        );

        // Second replay (no new bytes, dims same) should be near-
        // instant — proves the first cost is one-time.
        let t2 = Instant::now();
        let _ = h.replay(80, 24, 0);
        let second_us = t2.elapsed().as_micros();
        eprintln!("codex second render (no changes): {second_us} µs");
        assert!(
            second_us < 1_000,
            "follow-up render not cached: {second_us} µs (should be <1ms)"
        );
    }

    // ============================================================
    // Pinned-session tests
    //
    // The pin strip shows each pinned session as a small tile at the
    // bottom of the view pane. Each tile drives `history.replay()`
    // (same ItemHistory cache as the main view) and renders via
    // `render_pty_tail`, which iterates the screen's bottom-N rows
    // into a ratatui Buffer. With many pinned sessions every frame
    // pays the per-session cost N times.
    //
    // Tests below exercise:
    //   * tile correctness after bootstrap / resize,
    //   * single-tile render performance,
    //   * aggregate cost when many sessions are pinned.
    //
    // They render through `tui_term::PseudoTerminal` into a fresh
    // ratatui Buffer the same way the TUI does, so cell-level
    // assertions reflect what the outer terminal would receive.
    // ============================================================

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    /// Render a session's screen into a fresh ratatui Buffer the
    /// same way `render_pty_tail` does for a pinned tile (via
    /// `PseudoTerminal::render` into the tile's Rect).
    fn render_tile(h: &mut ItemHistory, area: Rect) -> Buffer {
        // Replay at the main view's cols so the wrap matches what
        // the focused session would render — pin strip currently
        // does this (see render_pin_strip).
        let out = h.replay(area.width.max(60), area.height.max(3), 0);
        let mut buf = Buffer::empty(area);
        let no_cursor = tui_term::widget::Cursor::default().visibility(false);
        tui_term::widget::PseudoTerminal::new(out.screen)
            .cursor(no_cursor)
            .render(area, &mut buf);
        buf
    }

    /// Count cells with a non-blank symbol in the buffer.
    fn populated_cells(buf: &Buffer) -> usize {
        let area = *buf.area();
        let mut n = 0;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell(ratatui::layout::Position { x, y }) {
                    let s = cell.symbol();
                    if !s.is_empty() && s != " " {
                        n += 1;
                    }
                }
            }
        }
        n
    }

    /// Regression: when a session is both visible in the main view
    /// and shown in the pin strip, the pin tile rendering must not
    /// clobber the main view's rendering on subsequent frames.
    ///
    /// Mechanism of the bug (pre-fix): each `ItemHistory` carries a
    /// cached vt100 parser. `replay(cols, rows, _)` resizes that
    /// parser to the requested dims. The old pin-strip code called
    /// `replay(tile.width, tile.height, 0)` — i.e. the pin tile's
    /// narrow dims — which shrank the shared parser. The next
    /// main-view render at the wider dims re-resized the parser,
    /// but the prior pending-chunk content had already been
    /// re-fed/reflowed at the narrow width inside the cache.
    /// Visible effect: main view appeared clipped at the pin's
    /// width, content wrapped wrong, rows looked truncated.
    ///
    /// New behavior: pin tile renders the parser at the *main*
    /// view's dims (same as the main view itself), then
    /// `render_pty_tail` crops to the tile's smaller Rect for
    /// display. Cache stays stable.
    #[test]
    fn pinned_tile_render_does_not_clobber_main_view() {
        fn feed(h: &mut ItemHistory) {
            for i in 0..30 {
                h.feed_pty(
                    format!(
                        "session line {i:02} with enough content that it would wrap visibly differently at narrow vs wide widths and we can detect cache thrash\r\n"
                    )
                    .as_bytes(),
                );
            }
        }

        // Reference: main view rendered alone, no pin tile in the loop.
        let mut ref_hist = ItemHistory::new();
        feed(&mut ref_hist);
        let _ = render_main_view_buffer(&mut ref_hist, 120, 30); // warm
        let reference = render_main_view_buffer(&mut ref_hist, 120, 30);

        // With pin tile in the loop: render main view, then pin
        // tile, then main view again. The two main-view renders
        // must produce identical content.
        let mut shared = ItemHistory::new();
        feed(&mut shared);
        let _ = render_main_view_buffer(&mut shared, 120, 30); // warm
        let main_before = render_main_view_buffer(&mut shared, 120, 30);

        // Simulate render_pin_strip's NEW behavior: replay at main
        // dims, ignore the crop step (we only care about cache
        // state here; the crop is a pure read of the produced
        // screen and can't affect the cache).
        let (pin_cols, pin_rows) = (30u16, 6u16);
        let (main_cols, main_rows) = (120u16, 30u16);
        let cols = main_cols.max(pin_cols).max(1);
        let rows = main_rows.max(pin_rows).max(1);
        let _ = shared.replay(cols, rows, 0);

        let main_after = render_main_view_buffer(&mut shared, 120, 30);

        assert_eq!(
            reference.content(),
            main_before.content(),
            "two consecutive main-view renders should match"
        );
        assert_eq!(
            main_before.content(),
            main_after.content(),
            "main view render after pin tile render must equal main view render before (pin tile must not thrash the cache)"
        );

        // Note: the OLD pin-strip behavior (replay at narrow tile
        // dims) didn't actually corrupt the *content* of the next
        // main render — `replay_cached` rebuilds the parser on
        // cols-change, so the next main render recovered correctly
        // from the cache thrash. The fix's value is purely in
        // performance: rebuilding on every frame is O(history),
        // and pinning a long-history session would make every
        // frame pay that cost. By keeping the parser at main-view
        // dims across both render paths the cache stays warm.
    }

    #[test]
    fn pinned_tile_renders_content_after_bootstrap() {
        let mut h = ItemHistory::new();
        for i in 0..20 {
            h.feed_pty(format!("pinned session line {i}\r\n").as_bytes());
        }
        let tile_area = Rect::new(0, 0, 40, 5);
        let buf = render_tile(&mut h, tile_area);
        let n = populated_cells(&buf);
        assert!(
            n > 20,
            "pinned tile should show recent content, got {n} populated cells"
        );
    }

    #[test]
    fn pinned_tile_renders_content_after_resize() {
        let mut h = ItemHistory::new();
        for i in 0..20 {
            h.feed_pty(format!("pinned session line {i}\r\n").as_bytes());
        }
        // First render at one tile size, then at another (simulating
        // user widening the list pane → narrower tile).
        let _ = render_tile(&mut h, Rect::new(0, 0, 40, 5));
        let buf = render_tile(&mut h, Rect::new(0, 0, 25, 5));
        let n = populated_cells(&buf);
        assert!(
            n > 10,
            "narrower pin tile should still show content, got {n} populated cells"
        );
    }

    #[test]
    fn pinned_tile_single_render_is_fast() {
        let mut h = ItemHistory::new();
        // A long-lived pinned session — realistic chat history.
        for i in 0..5_000 {
            h.feed_pty(format!("pinned line {i} of accumulated chat\r\n").as_bytes());
        }
        let tile_area = Rect::new(0, 0, 60, 4);
        // Warm.
        let _ = render_tile(&mut h, tile_area);
        // Measure 100 steady-state renders.
        let t = Instant::now();
        for _ in 0..100 {
            let _ = render_tile(&mut h, tile_area);
        }
        let us = t.elapsed().as_micros();
        let per = us / 100;
        eprintln!("pinned tile render: 100 frames in {us} µs ({per} µs/frame)");
        assert!(
            per < 1_000,
            "single pin tile render too slow: {per} µs/frame at 5000-line history"
        );
    }

    #[test]
    fn many_pinned_sessions_render_within_frame_budget() {
        // 20 pinned sessions, each with a modest history. Aggregate
        // render cost across all of them should fit comfortably in
        // one TUI frame (we tick at ~120 ms; budget here is much
        // tighter).
        let n_pinned = 20;
        let mut histories: Vec<ItemHistory> = (0..n_pinned)
            .map(|i| {
                let mut h = ItemHistory::new();
                for line in 0..200 {
                    h.feed_pty(format!("session {i} line {line}\r\n").as_bytes());
                }
                h
            })
            .collect();
        let tile_area = Rect::new(0, 0, 50, 4);
        // Warm every cache.
        for h in histories.iter_mut() {
            let _ = render_tile(h, tile_area);
        }
        // Measure aggregate render across all pinned tiles for 50
        // frames.
        let frames = 50;
        let t = Instant::now();
        for _ in 0..frames {
            for h in histories.iter_mut() {
                let _ = render_tile(h, tile_area);
            }
        }
        let us = t.elapsed().as_micros();
        let per_frame = us / frames as u128;
        let per_tile = per_frame / n_pinned as u128;
        eprintln!(
            "{n_pinned} pin tiles, {frames} frames: total {us} µs, {per_frame} µs/frame, \
             {per_tile} µs/tile"
        );
        assert!(
            per_frame < 10_000,
            "{n_pinned} pinned tiles render too slow per frame: {per_frame} µs"
        );
    }

    #[test]
    fn many_pinned_sessions_resize_within_budget() {
        // Resizing the TUI when many sessions are pinned triggers a
        // replay per pinned tile (the tile's cols may change). Make
        // sure the aggregate doesn't blow the frame budget.
        let n_pinned = 10;
        let mut histories: Vec<ItemHistory> = (0..n_pinned)
            .map(|i| {
                let mut h = ItemHistory::new();
                for line in 0..1_000 {
                    h.feed_pty(format!("pinned-{i} chat line {line}\r\n").as_bytes());
                }
                h
            })
            .collect();
        // Warm at one size.
        for h in histories.iter_mut() {
            let _ = render_tile(h, Rect::new(0, 0, 60, 4));
        }
        // Resize (cols change for each tile).
        let t = Instant::now();
        for h in histories.iter_mut() {
            let _ = render_tile(h, Rect::new(0, 0, 80, 4));
        }
        let us = t.elapsed().as_micros();
        eprintln!("{n_pinned} pin tiles on resize: total {us} µs");
        // Currently this passes because the per-tile content is
        // small (1000 lines) so the rebuild is fast. With a long
        // codex-like history per tile it would fail — same root
        // cause as the codex test above. Threshold loose enough to
        // not be flaky.
        assert!(
            us < 50_000,
            "{n_pinned} pinned tiles too slow on resize: {us} µs"
        );
    }

    // ============================================================
    // Buffer-paint-cost measurement
    //
    // After a pane resize, ratatui considers every cell in the new
    // buffer "different" from the previous frame's (the previous
    // buffer was a different size), so crossterm emits a cursor-
    // position + char update for every visible cell. The outer
    // terminal then paints them — and on slow / remote / nested
    // terminals (ssh, tmux) that paint is visible as the "history
    // replay" cascade users see.
    //
    // These tests don't pass/fail per se — they print per-harness
    // populated-cell counts so we can compare codex against the
    // others and see whether codex's denser viewport is what makes
    // the outer-terminal paint look like a replay.
    // ============================================================

    /// Count cells with a non-blank symbol OR a non-default style in
    /// a Buffer. Roughly the unit of work the outer terminal does on
    /// a full repaint — empty/default cells are nearly free to print
    /// (one ` ` byte each), populated/styled cells cost ~10-30 bytes
    /// of crossterm output per cell.
    fn populated_or_styled_cells(buf: &Buffer) -> (usize, usize) {
        let area = *buf.area();
        let default_style = ratatui::style::Style::default();
        let mut populated = 0;
        let mut styled = 0;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buf.cell(ratatui::layout::Position { x, y }) {
                    let s = cell.symbol();
                    if !s.is_empty() && s != " " {
                        populated += 1;
                    }
                    if cell.style() != default_style {
                        styled += 1;
                    }
                }
            }
        }
        (populated, styled)
    }

    /// Render a session's screen into a fresh Buffer at the requested
    /// size (same path the live TUI uses for the focused view pane).
    fn render_main_view_buffer(h: &mut ItemHistory, w: u16, hgt: u16) -> Buffer {
        let area = Rect::new(0, 0, w, hgt);
        let out = h.replay(w, hgt, 0);
        let mut buf = Buffer::empty(area);
        let no_cursor = tui_term::widget::Cursor::default().visibility(false);
        tui_term::widget::PseudoTerminal::new(out.screen)
            .cursor(no_cursor)
            .render(area, &mut buf);
        buf
    }

    /// Print per-harness cell-paint counts after a resize. ratatui's
    /// real-world paint cost on a dim-changed pane is ~proportional
    /// to these numbers (every cell counts as "different" from the
    /// previous frame's buffer, since dims changed).
    ///
    /// Expected pattern (working hypothesis): codex's count is
    /// significantly higher than claude's or zarvis's, because
    /// codex's normal-screen scroll fills the viewport with content
    /// while claude's alt-screen + zarvis's chat-bubble layout leave
    /// more rows sparse.
    #[test]
    fn buffer_paint_cost_per_harness_after_resize() {
        let (w_pre, h_pre) = (80u16, 24u16);
        let (w_post, h_post) = (120u16, 30u16);

        // --- shell ---
        let mut shell = ItemHistory::new();
        shell_feed_minimal(&mut shell);
        let _ = render_main_view_buffer(&mut shell, w_pre, h_pre);
        let shell_buf = render_main_view_buffer(&mut shell, w_post, h_post);
        let (shell_p, shell_s) = populated_or_styled_cells(&shell_buf);

        // --- claude (alt-screen) ---
        let mut claude = ItemHistory::new();
        claude_feed_alt_screen(&mut claude);
        // Simulate a realistic chat session inside alt-screen.
        for i in 0..50 {
            claude.feed_pty(format!("\x1b[H\x1b[36mline {i}\x1b[0m message {i}\r\n").as_bytes());
        }
        let _ = render_main_view_buffer(&mut claude, w_pre, h_pre);
        let claude_buf = render_main_view_buffer(&mut claude, w_post, h_post);
        let (claude_p, claude_s) = populated_or_styled_cells(&claude_buf);

        // --- zarvis (chat + tool block) ---
        let mut zarvis = ItemHistory::new();
        zarvis_feed_chat(&mut zarvis);
        // Add more chat to make it comparable.
        for i in 0..50 {
            zarvis.feed_pty(format!("\x1b[1;36m> hi {i}\x1b[0m\r\n").as_bytes());
            zarvis.feed_pty(format!("\x1b[1;35m* response {i}\x1b[0m\r\n").as_bytes());
        }
        let _ = render_main_view_buffer(&mut zarvis, w_pre, h_pre);
        let zarvis_buf = render_main_view_buffer(&mut zarvis, w_post, h_post);
        let (zarvis_p, zarvis_s) = populated_or_styled_cells(&zarvis_buf);

        // --- codex (normal-screen, dense scroll) ---
        let mut codex = ItemHistory::new();
        codex_feed_long_session(&mut codex);
        let _ = render_main_view_buffer(&mut codex, w_pre, h_pre);
        let codex_buf = render_main_view_buffer(&mut codex, w_post, h_post);
        let (codex_p, codex_s) = populated_or_styled_cells(&codex_buf);

        let total_cells = (w_post as usize) * (h_post as usize);
        eprintln!("buffer paint cost @ {w_post}x{h_post} ({total_cells} cells total):");
        eprintln!(
            "  shell:   populated={shell_p:>5}  styled={shell_s:>5}  ({:.1}% populated)",
            100.0 * shell_p as f64 / total_cells as f64
        );
        eprintln!(
            "  claude:  populated={claude_p:>5}  styled={claude_s:>5}  ({:.1}% populated)",
            100.0 * claude_p as f64 / total_cells as f64
        );
        eprintln!(
            "  zarvis:  populated={zarvis_p:>5}  styled={zarvis_s:>5}  ({:.1}% populated)",
            100.0 * zarvis_p as f64 / total_cells as f64
        );
        eprintln!(
            "  codex:   populated={codex_p:>5}  styled={codex_s:>5}  ({:.1}% populated)",
            100.0 * codex_p as f64 / total_cells as f64
        );

        // Sanity: every harness should produce non-empty buffers.
        assert!(shell_p > 0);
        assert!(claude_p > 0);
        assert!(zarvis_p > 0);
        assert!(codex_p > 0);
    }

    /// Approximate crossterm payload size for a full repaint of a
    /// given Buffer, in bytes. Per-cell cost is roughly:
    ///   ~10 bytes cursor-position + ~10 bytes SGR + ~1-4 bytes char.
    /// This is a *rough proxy* for what the outer terminal has to
    /// digest after a dim-changed pane — the actual cost depends on
    /// SGR change patterns + UTF-8 widths, but the order of
    /// magnitude is what matters for the codex-vs-claude comparison.
    fn approximate_paint_bytes(buf: &Buffer) -> usize {
        let area = *buf.area();
        let mut total = 0usize;
        let mut last_style: Option<ratatui::style::Style> = None;
        for y in area.top()..area.bottom() {
            // Per-row cursor reposition. ~8 bytes for "\x1b[r;cH".
            total += 8;
            for x in area.left()..area.right() {
                let Some(cell) = buf.cell(ratatui::layout::Position { x, y }) else {
                    continue;
                };
                let s = cell.symbol();
                if Some(cell.style()) != last_style {
                    // SGR change: 10-20 bytes (color + bold + reset etc.).
                    total += 15;
                    last_style = Some(cell.style());
                }
                // Char itself.
                total += s.len().max(1);
            }
        }
        total
    }

    #[test]
    fn approximate_paint_bytes_per_harness_after_resize() {
        let (w_pre, h_pre) = (80u16, 24u16);
        let (w_post, h_post) = (120u16, 30u16);

        let mut shell = ItemHistory::new();
        shell_feed_minimal(&mut shell);
        let _ = render_main_view_buffer(&mut shell, w_pre, h_pre);
        let shell_bytes =
            approximate_paint_bytes(&render_main_view_buffer(&mut shell, w_post, h_post));

        let mut claude = ItemHistory::new();
        claude_feed_alt_screen(&mut claude);
        for i in 0..50 {
            claude.feed_pty(format!("\x1b[H\x1b[36mline {i}\x1b[0m\r\n").as_bytes());
        }
        let _ = render_main_view_buffer(&mut claude, w_pre, h_pre);
        let claude_bytes =
            approximate_paint_bytes(&render_main_view_buffer(&mut claude, w_post, h_post));

        let mut zarvis = ItemHistory::new();
        zarvis_feed_chat(&mut zarvis);
        for i in 0..50 {
            zarvis.feed_pty(format!("\x1b[1;36m> hi {i}\x1b[0m\r\n").as_bytes());
            zarvis.feed_pty(format!("\x1b[1;35m* resp {i}\x1b[0m\r\n").as_bytes());
        }
        let _ = render_main_view_buffer(&mut zarvis, w_pre, h_pre);
        let zarvis_bytes =
            approximate_paint_bytes(&render_main_view_buffer(&mut zarvis, w_post, h_post));

        let mut codex = ItemHistory::new();
        codex_feed_long_session(&mut codex);
        let _ = render_main_view_buffer(&mut codex, w_pre, h_pre);
        let codex_bytes =
            approximate_paint_bytes(&render_main_view_buffer(&mut codex, w_post, h_post));

        eprintln!("approximate paint bytes after {w_post}x{h_post} resize:");
        eprintln!("  shell:   {shell_bytes:>8} bytes");
        eprintln!("  claude:  {claude_bytes:>8} bytes");
        eprintln!("  zarvis:  {zarvis_bytes:>8} bytes");
        eprintln!("  codex:   {codex_bytes:>8} bytes");
    }

    // ============================================================
    // Repro: visible "history replay" cascade on codex
    //
    // The user reports: when a codex session is first opened OR
    // when the terminal is resized, codex's transcript scrolls past
    // visibly, frame by frame, like a recording being played back.
    // It's NOT a momentary flicker — the user sees the content
    // animating.
    //
    // Hypothesis: codex's TUI, in response to SIGWINCH (or on
    // initial spawn), re-emits its current conversation log to the
    // PTY in a burst. That burst leaves the codex binary as one
    // logical redraw but reaches the daemon (and then the TUI) as
    // multiple PTY read() chunks (PTY OS buffers are ~4 KiB; a
    // 30-row × 120-col viewport with SGR codes is easily >4 KiB).
    // Each chunk fires a `BroadcastMsg::Event(PtyChunk)` to
    // attached clients. The TUI calls `feed_pty` on each chunk and
    // re-renders the focused tile per frame, so the user sees
    // codex's redraw paint in real time — exactly the "replay"
    // sensation.
    //
    // This test reproduces that mechanism in isolation: feed the
    // exact same total bytes either (a) as one chunk + one render,
    // or (b) as many chunks + render between each. (a) lands on a
    // single final frame (no replay perception). (b) produces a
    // sequence of *visibly different* intermediate frames — the
    // bug as the user perceives it.
    //
    // The fix lives at the app/render-loop level (coalesce frames
    // during a PTY chunk burst), not in the items model. This test
    // exists to give the fix a concrete target: after the fix, the
    // chunked feed should still be ~1 visible transition.
    // ============================================================
    #[test]
    fn codex_sigwinch_redraw_arrives_as_chunks_user_sees_animation() {
        // Step 1: a session that already has some history (the
        // viewport before SIGWINCH).
        let mut h_one_shot = ItemHistory::new();
        codex_feed_long_session(&mut h_one_shot);
        let _ = h_one_shot.replay(120, 30, 0);

        let mut h_chunked = ItemHistory::new();
        codex_feed_long_session(&mut h_chunked);
        let _ = h_chunked.replay(120, 30, 0);

        // Step 2: build the redraw codex emits in response to
        // SIGWINCH. Shape: clear-screen + home, then ~30 rows of
        // styled content (the visible conversation log). 30 lines
        // × ~150 bytes (with SGR) ≈ 4.5 KB — larger than one PTY
        // OS read buffer, so it WILL arrive as 2+ chunks in real
        // life.
        let mut redraw_bytes = Vec::with_capacity(8_000);
        redraw_bytes.extend_from_slice(b"\x1b[2J\x1b[H");
        for row in 0..30u32 {
            redraw_bytes.extend_from_slice(b"\x1b[1;36muser\x1b[0m: ");
            redraw_bytes.extend_from_slice(
                format!("redrawn line {row:02} of codex transcript ").as_bytes(),
            );
            redraw_bytes.extend_from_slice(b"\x1b[35m(model)\x1b[0m ");
            redraw_bytes
                .extend_from_slice(b"some additional content to make the row realistic\r\n");
        }

        // --- (a) one-shot feed: feed all bytes at once, render once
        h_one_shot.feed_pty(&redraw_bytes);
        let final_one_shot = render_main_view_buffer(&mut h_one_shot, 120, 30);

        // --- (b) chunked feed: split into 6 chunks (≈ realistic
        // PTY read fragmentation; a 64 KiB ring drains in several
        // ~4 KiB reads when the OS buffer fills repeatedly).
        let n_chunks = 6;
        let chunk_size = redraw_bytes.len().div_ceil(n_chunks);
        let mut intermediate_frames: Vec<Buffer> = Vec::with_capacity(n_chunks);
        for chunk in redraw_bytes.chunks(chunk_size) {
            h_chunked.feed_pty(chunk);
            intermediate_frames.push(render_main_view_buffer(&mut h_chunked, 120, 30));
        }

        // The one-shot final frame and the chunked final frame
        // should match (correctness: same input → same screen).
        let final_chunked = intermediate_frames.last().unwrap();
        assert_eq!(
            final_one_shot.content(),
            final_chunked.content(),
            "one-shot and chunked feeds should converge to the same final frame"
        );

        // Now count: how many of the intermediate frames differ
        // from the *one-shot final* frame? Each one is a frame the
        // user sees that isn't the "settled" state — i.e. visible
        // animation of the redraw.
        let mut transient_frames = 0usize;
        for f in &intermediate_frames {
            if f.content() != final_one_shot.content() {
                transient_frames += 1;
            }
        }

        eprintln!(
            "codex SIGWINCH redraw delivered as {n_chunks} chunks: \
             {transient_frames} of {n_chunks} intermediate frames \
             differ from the final settled state \
             (each one is what the user sees as a 'replay frame')"
        );

        // The bug as the user perceives it: this number is > 1.
        // After the render-coalescing fix at the app level this
        // should be 0 or 1 (we render once when the burst settles).
        // We assert *the bug exists* here so the test fails when
        // the fix lands — at which point this assertion should be
        // inverted to assert the *absence* of the cascade.
        assert!(
            transient_frames > 1,
            "expected per-chunk rendering to produce >1 transient \
             intermediate frames (the user-visible replay cascade); \
             got {transient_frames}. If this assertion fails because \
             the cascade is gone, invert it: the bug is fixed."
        );
    }

    // ============================================================
    // Mouse-scrollback regression
    //
    // User report: scrolling up in a codex or claude session view
    // doesn't show older history. Scrolling is wired through
    // `App::adjust_scrollback` → `view_scrollback` → passed as the
    // 3rd arg to `ItemHistory::replay`, which forwards to
    // `vt100::Screen::set_scrollback`. For that to actually show
    // older lines, the parser's scrollback buffer must contain
    // them — and vt100 only fills scrollback when content
    // *naturally* scrolls off the top of the viewport (newline at
    // bottom row pushes the top row into scrollback).
    //
    // Two harness patterns break this:
    //
    // 1. **Alt-screen** (claude): `\x1b[?1049h` switches to a
    //    separate buffer that has no scrollback by design — same
    //    as a real terminal running vim/htop/less. Scrolling up
    //    in the TUI does nothing because there's literally nothing
    //    in the alt-screen's scrollback.
    //
    // 2. **In-place redraw** (codex): when the child clears and
    //    re-paints its viewport with `\x1b[2J\x1b[H` (or
    //    `\x1b[H\x1b[J`) instead of letting content scroll off
    //    naturally, the cleared rows do NOT enter scrollback. The
    //    viewport advances but the buffer doesn't grow. Codex's
    //    chat output in real life mixes both patterns (some
    //    streaming `\r\n` content + occasional viewport
    //    redraws); the redraw passes erase whatever was visible
    //    before they ran.
    //
    // These tests document both failure modes. A real fix would
    // either (a) snapshot pre-redraw viewport rows into scrollback
    // before `\x1b[2J` clears them, (b) keep an items-model
    // companion buffer of every `PtyChunk` so scrollback isn't
    // dependent on what vt100 retained, or (c) for alt-screen,
    // surface the underlying main-screen scrollback when the user
    // scrolls.
    // ============================================================

    /// Helper: count rows in a screen that have ANY non-blank
    /// content. Used as a "does this look like a populated
    /// viewport" check.
    fn screen_populated_rows(screen: &vt100::Screen, rows: u16, cols: u16) -> usize {
        (0..rows)
            .filter(|&r| {
                (0..cols).any(|c| {
                    screen
                        .cell(r, c)
                        .map(|cell| !cell.contents().is_empty() && cell.contents() != " ")
                        .unwrap_or(false)
                })
            })
            .count()
    }

    /// Concat every populated row's content for comparison. Two
    /// scrollback positions should produce different concat'd
    /// strings if scrollback is doing anything.
    fn screen_text(screen: &vt100::Screen, rows: u16, cols: u16) -> String {
        let mut out = String::new();
        for r in 0..rows {
            for c in 0..cols {
                if let Some(cell) = screen.cell(r, c) {
                    out.push_str(&cell.contents());
                }
            }
            out.push('\n');
        }
        out
    }

    /// Sanity baseline: shell-style output (pure `\r\n` lines) DOES
    /// populate scrollback, so a scrolled-back render shows older
    /// rows than a live render. This is the contract that codex
    /// and claude break.
    #[test]
    fn shell_scrollback_shows_history() {
        let mut h = ItemHistory::new();
        for i in 0..200u32 {
            h.feed_pty(format!("shell line {i:04}\r\n").as_bytes());
        }
        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled = screen_text(h.replay(80, 24, 50).screen, 24, 80);
        assert_ne!(
            live, scrolled,
            "shell scrollback should expose older rows; if these match, \
             vt100 isn't accumulating history"
        );
        // The scrolled-back view should reach further back than the
        // live view — concretely, it shows lines ≤ 150 while live
        // shows lines 176-199.
        assert!(
            scrolled.contains("shell line 0150") || scrolled.contains("shell line 0100"),
            "scrolled view should show older line numbers, got rows:\n{scrolled}"
        );
        assert!(
            live.contains("shell line 0199"),
            "live view should show the most recent line, got rows:\n{live}"
        );
    }

    /// claude (alt-screen) — with the shadow parser fix, the
    /// shadow stays in normal-screen mode and accumulates the
    /// pre-alt-screen content. Scrolling up exposes whatever was
    /// on the main screen *before* claude grabbed it.
    #[test]
    fn claude_scrollback_shows_pre_alt_screen_history() {
        let mut h = ItemHistory::new();
        // Simulate a shell session that ran before launching
        // claude — these lines should be reachable via the
        // mouse-scroll-up shadow buffer.
        for i in 0..50u32 {
            h.feed_pty(format!("shell line before claude {i:04}\r\n").as_bytes());
        }
        // Now claude enters alt-screen and renders its TUI.
        claude_feed_alt_screen(&mut h);
        for i in 0..200u32 {
            h.feed_pty(format!("\x1b[Halt line {i:04}\r\n").as_bytes());
        }
        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled = screen_text(h.replay(80, 24, 30).screen, 24, 80);
        assert_ne!(
            live, scrolled,
            "shadow parser should expose pre-alt-screen content; \
             live shows claude's alt-screen view, scrolled shows shell history"
        );
        assert!(
            scrolled.contains("shell line before claude"),
            "scrolled-back view should show shell content from before \
             claude entered alt-screen, got:\n{scrolled}"
        );
        // Live view still shows claude's alt-screen content.
        assert!(
            live.contains("claude") || live.contains("alt line"),
            "live view should still render claude's alt-screen output"
        );
    }

    /// vt100 keeps `\x1b[2J`-cleared content in scrollback (the
    /// `J` clears the visible viewport but does NOT truncate the
    /// back-buffer). Documents the contract for the redraw shape
    /// that codex's resize path uses; this case alone wouldn't
    /// break the user's mouse-scroll experience.
    #[test]
    fn vt100_retains_pre_clear_lines_in_scrollback() {
        let mut h = ItemHistory::new();
        for i in 0..100u32 {
            h.feed_pty(format!("pre {i:04}\r\n").as_bytes());
        }
        h.feed_pty(b"\x1b[2J\x1b[H");
        for i in 0..5u32 {
            h.feed_pty(format!("post {i:04}\r\n").as_bytes());
        }
        let scrolled = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled_back = screen_text(h.replay(80, 24, 100).screen, 24, 80);
        assert_ne!(scrolled, scrolled_back);
        assert!(
            scrolled_back.contains("pre"),
            "scrollback should still expose pre-clear lines"
        );
    }

    /// codex-style normal-screen chat — natural `\r\n` lines flow
    /// through both main and shadow parser. The shadow's
    /// scrollback exposes older chat content when the user scrolls.
    /// (The pure TUI-redraw case where the child *never* emits
    /// `\r\n` still won't populate scrollback, but real codex chat
    /// is line-based.)
    #[test]
    fn codex_scrollback_shows_chat_history() {
        let mut h = ItemHistory::new();
        for i in 0..200u32 {
            h.feed_pty(format!("\x1b[36muser\x1b[0m: chat msg {i:04}\r\n").as_bytes());
        }
        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled = screen_text(h.replay(80, 24, 80).screen, 24, 80);
        assert_ne!(live, scrolled, "scrollback should expose older chat lines");
        // At scroll=80 with 24-row viewport over 200 messages,
        // the viewport sits around msgs 96..=119. Anchor the
        // assertion on something inside that window.
        assert!(
            scrolled.contains("chat msg 0100"),
            "scrolled view should expose older messages around msg 0100, got:\n{scrolled}"
        );
    }

    /// Real codex sessions emit cursor positioning (`\x1b[r;cH`)
    /// and DECSTBM scroll regions sized for the *actual* PTY —
    /// frequently with row counts > 24 and col counts > 80. If the
    /// shadow parser is left at the default 80×24 while bytes
    /// arrive shaped for a 140×30 pane, every out-of-range cursor
    /// position gets clamped and codex's UI state in the shadow
    /// becomes incoherent. The user's symptom was: scroll back in
    /// an unzoomed codex view jumps past recent content because
    /// the shadow doesn't reflect what codex actually rendered.
    ///
    /// The fix has two halves, both pinned here:
    ///   1. `replay(cols, rows, _)` resizes the shadow to match
    ///      the pane on every frame — not just scrollback>0.
    ///   2. `set_pty_size(cols, rows)` is a public hook so the
    ///      bootstrap path (which feeds rehydrated bytes BEFORE
    ///      the first render) can pre-size the shadow.
    #[test]
    fn replay_keeps_shadow_in_sync_with_pty_dims() {
        let mut h = ItemHistory::new();
        // Default before any replay: vt100 chose the seed values
        // (80×24), which is wrong for almost every real pane.
        assert_eq!((h.shadow_cols, h.shadow_rows), (80, 24));

        let _ = h.replay(140, 30, 0); // scrollback==0 must still resize
        assert_eq!(
            (h.shadow_cols, h.shadow_rows),
            (140, 30),
            "replay at scrollback=0 must size the shadow to the pane"
        );

        let _ = h.replay(100, 40, 5); // scrollback>0 path also resizes
        assert_eq!((h.shadow_cols, h.shadow_rows), (100, 40));
    }

    #[test]
    fn set_pty_size_resizes_shadow_before_feed() {
        // Simulates `bootstrap_terminal`: caller knows the PTY
        // dims, sizes the shadow, THEN feeds the rehydrated bytes
        // so codex's cursor-positioning is interpreted correctly.
        let mut h = ItemHistory::new();
        h.set_pty_size(140, 30);
        assert_eq!((h.shadow_cols, h.shadow_rows), (140, 30));

        // Defensive clamps for absurd inputs. Floor is
        // `VT100_MIN_DIM` (=2), not 1 — see the constant's
        // comment for the vt100-0.16.2 col_wrap underflow bug
        // this is guarding against.
        h.set_pty_size(0, 0);
        assert_eq!(
            (h.shadow_cols, h.shadow_rows),
            (VT100_MIN_DIM, VT100_MIN_DIM),
        );
    }

    /// Symptom-level regression for the unzoomed-codex scroll bug.
    ///
    /// User-visible: scrolling back in an unzoomed codex view
    /// showed incoherent / wrapped scrollback (lines split across
    /// rows, missing recent content). Root cause: the shadow
    /// parser was seeded at the vt100 default 80×24 and was only
    /// resized when the user first scrolled — so every byte that
    /// arrived before that point (the entire session, in the live
    /// path) was wrapped/clamped at 80 cols. After the user
    /// finally scrolled, `set_size` resized the shadow but vt100
    /// does NOT reflow existing scrollback; the lines stayed
    /// wrapped at 80 cols and the rendered output was broken.
    ///
    /// This test pins the symptom: feed 100 chat lines that fit
    /// in 120 cols but wrap at 80, then scroll back. The full,
    /// unwrapped chat line must appear on a single screen row.
    /// Pre-fix this fails (lines are split across two rows
    /// because the shadow processed them at 80×24).
    #[test]
    fn codex_unzoomed_scrollback_shows_unwrapped_lines() {
        let cols: u16 = 120;
        let rows: u16 = 30;

        let mut h = ItemHistory::new();
        // Live render loop runs a frame before bytes arrive — the
        // first `replay` is what establishes the shadow's
        // geometry for the live path. We simulate that here.
        let _ = h.replay(cols, rows, 0);

        // Pad each chat line to a width that exceeds the default
        // shadow's 80 cols but fits comfortably in 120 cols. A
        // single-row, single-line assertion only succeeds if the
        // shadow saw these bytes at 120-col width.
        let pad: String = "x".repeat(95);
        for i in 0..100u32 {
            h.feed_pty(format!("chat-line-{i:03} {pad}\r\n").as_bytes());
        }

        let scrolled = screen_text(h.replay(cols, rows, 30).screen, rows, cols);
        // The full padded line for msg 0050 should appear on one
        // screen row — i.e., contiguous in `scrolled` with no
        // intervening `\n`. screen_text inserts `\n` between rows,
        // so any `\n` inside the searched substring would indicate
        // the line got wrapped during shadow processing.
        let needle = format!("chat-line-050 {pad}");
        assert!(
            scrolled.contains(&needle),
            "scrolled shadow view should contain the unwrapped chat line; got:\n{scrolled}"
        );
    }

    /// Codex can repaint as a true TUI-style child: every frame is
    /// cursor positioning + erase-line, with no natural `\r\n` scroll.
    /// The shadow parser snapshots each visible frame before the next
    /// top-left redraw so mouse wheel / `C-x [` still expose older
    /// frames.
    #[test]
    fn codex_pure_tui_redraw_scrollback_exposes_older_frames() {
        let mut h = ItemHistory::new();
        for turn in 0..50u32 {
            h.feed_pty(b"\x1b[H");
            for row in 0..10u32 {
                h.feed_pty(format!("\x1b[{};1H\x1b[K", row + 1).as_bytes());
                h.feed_pty(format!("turn {turn:03} row {row:02}").as_bytes());
            }
        }
        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled = screen_text(h.replay(80, 24, 100).screen, 24, 80);
        assert_ne!(
            live, scrolled,
            "scrollback should expose earlier in-place redraw frames"
        );
        assert!(
            scrolled.contains("turn 0") || scrolled.contains("turn 1"),
            "scrolled view should include older redraw frames, got:\n{scrolled}"
        );
    }

    /// Codex-style repaint snapshots used to drop every empty row
    /// before appending the frame into shadow scrollback. That made
    /// older pages look like a compact list of text with no visual
    /// spacing between prompt / assistant / tool sections.
    #[test]
    fn codex_shadow_snapshot_preserves_internal_blank_rows() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"\x1b[1;1H");
        h.feed_pty(b"User prompt");
        h.feed_pty(b"\x1b[3;1H");
        h.feed_pty(b"Assistant reply");
        h.feed_pty(b"\x1b[5;1H");
        h.feed_pty(b"Tool result");

        // Trigger snapshot of the current viewport before Codex-style
        // full-screen repaint. The blank rows between text rows should
        // be copied into the shadow history rather than compacted away.
        h.feed_pty(b"\r\n");
        h.feed_pty(b"\x1b[1;1H");
        h.feed_pty(b"Next frame");

        let scrolled = screen_text(h.replay(80, 10, 1).screen, 10, 80);
        let expected = "User prompt\n\nAssistant reply\n\nTool result";
        assert!(
            scrolled.contains(expected),
            "scrolled snapshot should preserve internal blank rows, got:\n{scrolled}"
        );
    }

    #[test]
    fn codex_shadow_does_not_duplicate_single_row_repaints() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"\x1b[1;1HTop message");
        h.feed_pty(b"\x1b[1;1HTop message extended");
        h.feed_pty(b"\x1b[1;1HTop message extended again");

        let scrolled = screen_text(h.replay(80, 10, 1).screen, 10, 80);
        assert!(
            !scrolled.contains("Top message\nTop message extended"),
            "single-row repaint prefixes should not be snapshotted as duplicate scrollback rows, got:\n{scrolled}"
        );
    }

    #[test]
    fn codex_shadow_snapshots_on_resize_before_reflow() {
        let mut h = ItemHistory::new();
        h.feed_pty(b"line before resize\r\n");
        h.set_pty_size(100, 20);
        let scrolled = screen_text(h.replay(100, 10, 1).screen, 10, 100);
        assert!(
            scrolled.contains("line before resize"),
            "resize should snapshot pending line-based history before changing shadow geometry, got:\n{scrolled}"
        );
    }

    /// REGRESSION: zarvis tool-call rendering "disappears" the
    /// moment the user scrolls.
    ///
    /// Mechanism: the live view (`scrollback = 0`) renders a
    /// `ToolBlock` as a multi-row synthesized UI (`→ tool(args)`
    /// header + status row + optional output body — roughly 4-5
    /// rows for a typical result). As soon as the user scrolls, the
    /// renderer diverts to the shadow parser, which only sees the
    /// raw PTY bytes the adapter wrote — the truncated one-line
    /// preview (`✓ <line>`). Effect: the multi-row synthesized
    /// block collapses to one row, every other row shifts by the
    /// missing rows, and the user perceives "the tool block
    /// disappeared".
    ///
    /// This test pins down the symptom by checking that the SAME
    /// tool block visible in the live render survives into a render
    /// where the user has scrolled only a single row.
    #[test]
    fn zarvis_tool_block_survives_one_row_of_scroll() {
        let mut h = ItemHistory::new();
        // Tool block: OSC open + truncated zarvis preview + close,
        // followed by structured ToolUse/ToolResult so the live path
        // can synthesize the rich block.
        h.feed_pty(b"\x1b]7700;open;call=t1\x07");
        h.feed_pty(b"  \x1b[1;32m\xe2\x9c\x93\x1b[0m  \x1b[2mTOOL-PREVIEW-LINE\x1b[0m\r\n");
        h.feed_pty(b"\x1b]7700;close;call=t1\x07");
        h.feed_tool_use("shell".to_string(), "ls".to_string());
        h.feed_tool_result("t1", true, "TOOL-PREVIEW-LINE".to_string());

        // Just enough chat to fill the viewport — the block stays
        // visible at the top in the live render.
        for i in 0..10u32 {
            h.feed_pty(format!("chat after line {i:02}\r\n").as_bytes());
        }

        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        // User scrolls a single row. Before the fix this diverted
        // to the shadow parser and lost the synthesized rendering.
        let scrolled = screen_text(h.replay(80, 24, 1).screen, 24, 80);

        // Sanity: the synthesized block IS in the live render. The
        // header line is part of `synth_block`'s output, fed through
        // vt100 so the ANSI gets stripped from `cell.contents()`.
        assert!(
            live.contains("→ shell"),
            "synthesized block header should be in live render, got:\n{live}",
        );

        // The regression assertion: ONE row of scroll should not
        // wipe the synthesized rendering. Today it does — scrolled
        // view only has the one-line truncated preview, no header.
        assert!(
            scrolled.contains("→ shell"),
            "REGRESSION: scrolling a single row replaces the synthesized \
             tool block (`→ tool(args)` + status row + body) with the \
             one-line zarvis preview from the shadow parser. The user \
             perceives the tool block as 'disappearing' since several \
             rows of UI vanish and every row below shifts up. \
             Got:\n{scrolled}",
        );
    }

    #[test]
    fn running_tool_block_shows_keyboard_hints_not_buttons() {
        let mut h = ItemHistory::new();
        h.feed_task_start(
            "t1".to_string(),
            "shell".to_string(),
            "sleep 60".to_string(),
        );
        if let Some(Item::ToolBlock(block)) = h.items.last_mut() {
            block.started_at = Instant::now() - std::time::Duration::from_secs(8);
        }

        let out = h.replay(100, 12, 0);
        let text = screen_text(out.screen, 12, 100);

        assert!(text.contains("running"), "missing running status:\n{text}");
        assert!(
            text.contains("Ctrl-b background"),
            "missing background key hint:\n{text}"
        );
        assert!(text.contains("Esc kill"), "missing kill key hint:\n{text}");
        assert!(
            !text.contains("[bg]") && !text.contains("[kill]"),
            "button labels should not render:\n{text}"
        );
        assert!(
            out.blocks
                .iter()
                .all(|hit| hit.bg_button.is_none() && hit.kill_button.is_none()),
            "button hit zones should not be exposed for text hints: {:?}",
            out.blocks
        );
    }

    #[test]
    fn running_tool_signature_ignores_elapsed_seconds_until_controls_appear() {
        let mut h = ItemHistory::new();
        h.feed_task_start(
            "t1".to_string(),
            "shell".to_string(),
            "sleep 60".to_string(),
        );
        let Item::ToolBlock(block) = h.items.last().expect("tool block") else {
            panic!("expected tool block");
        };
        let mut fresh = block.clone();
        let mut older = block.clone();
        let threshold = buttons_after_ms();
        let below_threshold = threshold.saturating_sub(1).min(1_000);
        older.started_at = Instant::now() - std::time::Duration::from_millis(below_threshold);

        assert_eq!(
            ItemSig::of(&Item::ToolBlock(fresh.clone())),
            ItemSig::of(&Item::ToolBlock(older)),
            "elapsed seconds alone must not invalidate the tool-block parser cache"
        );

        if threshold > 0 {
            fresh.started_at =
                Instant::now() - std::time::Duration::from_millis(threshold.saturating_add(1));
            assert_ne!(
                ItemSig::of(&Item::ToolBlock(block.clone())),
                ItemSig::of(&Item::ToolBlock(fresh)),
                "the signature should change once the control hint becomes visible"
            );
        }
    }

    #[test]
    fn backgrounded_tool_block_shows_kill_key_hint_only() {
        let mut h = ItemHistory::new();
        h.feed_task_start(
            "t1".to_string(),
            "shell".to_string(),
            "sleep 60".to_string(),
        );
        h.feed_tool_result(
            "t1",
            true,
            "(running in background; will report when complete)".to_string(),
        );
        if let Some(Item::ToolBlock(block)) = h.items.last_mut() {
            block.started_at = Instant::now() - std::time::Duration::from_secs(8);
        }

        let text = screen_text(h.replay(100, 12, 0).screen, 12, 100);

        assert!(
            text.contains("in background"),
            "missing background status:\n{text}"
        );
        assert!(text.contains("Esc kill"), "missing kill key hint:\n{text}");
        assert!(
            !text.contains("Ctrl-b background"),
            "backgrounded tools should not show background hint:\n{text}"
        );
        assert!(
            !text.contains("[bg]") && !text.contains("[kill]"),
            "button labels should not render:\n{text}"
        );
    }

    /// REGRESSION: a fresh TUI re-attaching to an existing zarvis
    /// session must show tool blocks just like the session that
    /// originally rendered them — including in scrollback.
    ///
    /// `bootstrap_terminal` in `app.rs` does two steps to rehydrate
    /// PTY-backed history:
    ///   1. `client.pty_replay(id)` returns the PTY bytes the daemon
    ///      buffered in `pty.log` + the in-memory ring.
    ///   2. `client.transcript(id, …)` is replayed through
    ///      `apply_transcript_to_local_state`, which forwards events
    ///      to the history.
    ///
    /// Current zarvis interactive sessions do NOT write OSC 7700
    /// fences to the PTY (they're defined in `interactive.rs` but
    /// never called); the OSC backstop only fires for `pty.log`
    /// files left over from older zarvis builds. New sessions
    /// communicate tool blocks exclusively through
    /// `SessionEvent::TaskStart` (which carries the `call_id`) +
    /// `ToolUse` + `ToolResult`. If `apply_transcript_to_local_state`
    /// drops `TaskStart` on the floor (which it did before this
    /// fix), no `ToolBlock` items exist after bootstrap and the
    /// `has_blocks` check in `replay()` is false — `replay_cached`
    /// runs and the user sees raw chat with no synthesized blocks
    /// at any scroll position.
    ///
    /// This test simulates a current zarvis session: PTY bytes
    /// without OSC fences, plus a TaskStart + ToolResult on the
    /// transcript side. After both replays the synthesized block
    /// must appear in both live and scrolled renders.
    #[test]
    fn zarvis_tool_block_visible_after_bootstrap_via_task_start() {
        let mut h = ItemHistory::new();
        // Step 1: pty_replay bytes — pure chat, no OSC fences (that's
        // what current zarvis adapters actually write).
        for i in 0..10u32 {
            h.feed_pty(format!("chat line {i:02}\r\n").as_bytes());
        }
        // Step 2: transcript replay — TaskStart carries the call_id
        // and is the canonical block-creation event for new zarvis
        // sessions. apply_transcript_to_local_state must forward it.
        h.feed_task_start("t1".to_string(), "shell".to_string(), "ls".to_string());
        h.feed_tool_result("t1", true, "OUTPUT-LINE".to_string());

        let live = screen_text(h.replay(80, 24, 0).screen, 24, 80);
        let scrolled = screen_text(h.replay(80, 24, 1).screen, 24, 80);

        assert!(
            live.contains("→ shell"),
            "live render after bootstrap should show synthesized header — \
             feed_task_start must create a ToolBlock. got:\n{live}",
        );
        assert!(
            scrolled.contains("→ shell"),
            "scrolled render after bootstrap should keep the synthesized \
             header (companion to zarvis_tool_block_survives_one_row_of_scroll \
             — same fix, different bootstrap source). got:\n{scrolled}",
        );
    }
}
