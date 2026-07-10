//! Fork + subagent lineage tree: pure construction and a boxed-lane
//! diagram layout (each session a bordered box, each session's own
//! timeline a vertical lane below its box, forks branching right with
//! labeled arrows and merging back with return arrows — see `flatten`),
//! decoupled from `App` and ratatui so the layout can be unit-tested as
//! plain text (specs/0080-lineage-preview-on-harness-label.md).
//!
//! A session has at most one incoming lineage edge — either it was forked
//! from a parent (`forked_from`, spec 0078) or it is a subagent parented to
//! one (`parent_session_id`, spec 0014); a session is never both. That means
//! the full lineage graph is a strict tree, never a general DAG, which is
//! what makes a plain recursive walk (no cycle-breaking beyond a defensive
//! guard) sufficient.

use std::collections::{HashMap, HashSet};

use agentd_protocol::{ForkMergeMode, SessionKind, SessionState, SessionSummary};

/// Levels rendered below the tree's root before a subtree collapses into a
/// "+N more" marker (spec: "depth/breadth cap").
pub const MAX_DEPTH: usize = 6;
/// Children rendered per node before the rest collapse into a "+N more"
/// marker.
pub const MAX_SIBLINGS: usize = 12;

/// What kind of edge connects a node to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageEdge {
    /// The tree's root — no incoming edge.
    Root,
    /// Mergeable sibling via `forked_from` (spec 0078).
    Fork,
    /// True parent/child helper via `parent_session_id` (spec 0014).
    Subagent,
}

/// Fork-specific terminal state, derived from [`SessionSummary::merge`].
/// Meaningless for `LineageEdge::Subagent` / `LineageEdge::Root` nodes —
/// those are always `Open`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkStatus {
    /// Not a fork, or a fork with no merge outcome recorded yet — still
    /// mergeable.
    Open,
    /// `ForkMergeMode::Result`: closed back into the parent at the point the
    /// result was injected into its transcript.
    Merged,
    /// `ForkMergeMode::Discard`: dead-ended without a result.
    Discarded,
}

impl ForkStatus {
    pub fn of(summary: &SessionSummary) -> ForkStatus {
        match summary.merge.as_ref().map(|m| m.mode) {
            Some(ForkMergeMode::Result) => ForkStatus::Merged,
            Some(ForkMergeMode::Discard) => ForkStatus::Discarded,
            None => ForkStatus::Open,
        }
    }
}

/// One child slot in a node's children list: a real node, or a collapsed
/// run of nodes the depth/breadth cap dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageChild {
    Node(LineageNode),
    /// `count` additional nodes exist here but were not materialized —
    /// either extra siblings beyond [`MAX_SIBLINGS`], or (when this marker
    /// is a node's only child) its direct children, dropped because
    /// [`MAX_DEPTH`] was reached.
    More(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageNode {
    pub session_id: String,
    pub edge: LineageEdge,
    pub children: Vec<LineageChild>,
}

/// Whether `session_id` has any lineage relationship worth showing: it was
/// itself forked from a parent, or at least one other session in `sessions`
/// points back at it via `forked_from`/`parent_session_id`. Used to gate the
/// lineage preview trigger (the pane title bar's harness label) on ordinary
/// sessions that have nothing to show — cheaper than [`build_tree`] since it
/// doesn't walk to the root or materialize the full tree, just answers
/// yes/no for `session_id` itself.
pub fn has_lineage(session_id: &str, sessions: &[SessionSummary]) -> bool {
    sessions.iter().any(|s| {
        if s.id == session_id {
            s.forked_from.is_some()
        } else {
            (matches!(s.kind, SessionKind::Subagent)
                && s.parent_session_id.as_deref() == Some(session_id))
                || s.forked_from
                    .as_ref()
                    .is_some_and(|f| f.session_id == session_id)
        }
    })
}

/// Build the lineage tree containing `focus_id`: walk up through fork
/// (`forked_from`) and subagent (`parent_session_id`) parent links to the
/// topmost ancestor, then materialize the tree back down from there. `None`
/// when `focus_id` isn't among `sessions` (e.g. it was deleted while the
/// popup was open).
pub fn build_tree(focus_id: &str, sessions: &[SessionSummary]) -> Option<LineageNode> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    by_id.get(focus_id)?;
    let root_id = root_of(focus_id, &by_id);
    let mut visited = HashSet::new();
    build_subtree(&root_id, &by_id, LineageEdge::Root, 0, &mut visited)
}

fn parent_of(s: &SessionSummary) -> Option<&str> {
    s.forked_from
        .as_ref()
        .map(|f| f.session_id.as_str())
        .or(s.parent_session_id.as_deref())
}

fn root_of(focus_id: &str, by_id: &HashMap<&str, &SessionSummary>) -> String {
    let mut current = focus_id.to_string();
    let mut seen = HashSet::new();
    while seen.insert(current.clone()) {
        let Some(s) = by_id.get(current.as_str()) else {
            break;
        };
        match parent_of(s).filter(|p| by_id.contains_key(p)) {
            Some(p) => current = p.to_string(),
            None => break,
        }
    }
    current
}

fn build_subtree(
    id: &str,
    by_id: &HashMap<&str, &SessionSummary>,
    edge: LineageEdge,
    depth: usize,
    visited: &mut HashSet<String>,
) -> Option<LineageNode> {
    // Defensive cycle guard: a well-formed lineage graph is a tree (every
    // session has at most one parent edge), so this should never trip. It
    // exists so corrupted/adversarial data can't hang the render loop.
    if !visited.insert(id.to_string()) {
        return None;
    }
    by_id.get(id)?;

    let mut kids: Vec<(&SessionSummary, LineageEdge)> = Vec::new();
    for s in by_id.values() {
        if matches!(s.kind, SessionKind::Subagent) && s.parent_session_id.as_deref() == Some(id) {
            kids.push((s, LineageEdge::Subagent));
        } else if s.forked_from.as_ref().is_some_and(|f| f.session_id == id) {
            kids.push((s, LineageEdge::Fork));
        }
    }
    // Deterministic order: position/creation order within each edge type,
    // then subagents before forks (stable sort preserves the first pass).
    // The `by_id.values()` collection above iterates a `HashMap` in
    // unspecified order, so a final tiebreak on `id` is required — without
    // it, two sessions with equal `position` *and* `created_at` (both
    // plausible: default `position` is 0, and batch-created sessions can
    // share a millisecond) would render in a different order every time.
    kids.sort_by(|(a, _), (b, _)| {
        a.position
            .cmp(&b.position)
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    kids.sort_by_key(|(_, e)| matches!(e, LineageEdge::Fork));

    let total = kids.len();
    let children = if total == 0 {
        Vec::new()
    } else if depth + 1 >= MAX_DEPTH {
        // One more level would exceed the depth cap — collapse this node's
        // children (and everything below them) into a single marker rather
        // than silently truncating one branch and not another.
        vec![LineageChild::More(total)]
    } else {
        let mut out: Vec<LineageChild> = kids
            .iter()
            .take(MAX_SIBLINGS)
            .filter_map(|(s, e)| {
                build_subtree(&s.id, by_id, *e, depth + 1, visited).map(LineageChild::Node)
            })
            .collect();
        if total > MAX_SIBLINGS {
            out.push(LineageChild::More(total - MAX_SIBLINGS));
        }
        out
    };

    Some(LineageNode {
        session_id: id.to_string(),
        edge,
        children,
    })
}

/// Role of one styled text run within a rendered diagram row — the TUI
/// renderer maps each role to a theme style, keeping this module free of
/// ratatui types so the whole layout is unit-testable as plain text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineageSpan {
    /// Diagram wiring: lane bars, branch/merge arrow shafts, connectors.
    Rail,
    /// Session-box border fragments — tagged with the box's session so the
    /// renderer can highlight exactly one box's rectangle (all three of its
    /// rows) when that session is the keyboard selection, without touching
    /// wiring that happens to share those rows.
    Border { session_id: String },
    /// The glyph + word labeling a branch arrow (`⑂ fork` / `▸ subagent`).
    Edge(LineageEdge),
    /// Turn info for one activity window on some node's own timeline —
    /// bounded by that node's creation, a fork child's fork-out /
    /// merge-back points, and "now" (or the node's own terminal point).
    /// The window's numbers ride along so tests can assert boundaries
    /// without parsing the rendered text.
    Segment {
        /// Messages/turns within this window (`SessionSummary::event_count`
        /// / `ForkedFrom::transcript_seq` / `ForkMerge::merged_seq` units —
        /// all the same transcript sequence counter).
        delta_events: u64,
        /// Start of this window, epoch ms.
        start_ms: i64,
        /// End of this window, epoch ms; `None` = still open (measured
        /// against `now_ms` at flatten time).
        end_ms: Option<i64>,
    },
    /// The `•` bullet heading every turn-info line, sitting on the lane.
    SegmentBullet,
    /// Terminal-outcome glyph appended after a node's FINAL turn-info
    /// line: `✓` when the session ended `Done`, `✗` when it `Errored`.
    /// (A fork's merged/discarded outcome is not repeated here — the merge
    /// arrow and the box label's `↩ merged` / `✗ discarded` marker already
    /// carry it.)
    SegmentOutcome { ok: bool },
    /// A node's box label text (status glyph, name, harness, terminal
    /// marker) — carries the session id so the renderer can style it by
    /// that session's live state.
    Node { session_id: String },
    /// "+N more" collapse marker.
    More(usize),
}

/// One styled run of text within a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageSpanRun {
    pub text: String,
    pub role: LineageSpan,
}

/// One renderable line of the lineage diagram.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageRow {
    pub spans: Vec<LineageSpanRun>,
    /// `Some(session id)` when this row is a node's box label row — the
    /// row keyboard selection lands on. Everything else (borders, lane
    /// bars, segments, arrows, "+N more") is not selectable.
    pub node_session_id: Option<String>,
}

impl LineageRow {
    pub fn session_id(&self) -> Option<&str> {
        self.node_session_id.as_deref()
    }

    pub fn is_selectable(&self) -> bool {
        self.node_session_id.is_some()
    }

    /// The row's full text with styling stripped — for tests and debugging.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// A plain character grid the diagram is laid out onto before being cut
/// into styled rows. Cells hold `(char, role)`; unset cells become spaces.
/// A `'\0'` cell marks the continuation column of a double-width character
/// (CJK titles) — it occupies grid space for alignment math but emits no
/// text of its own.
#[derive(Default)]
struct Canvas {
    cells: Vec<Vec<Option<(char, LineageSpan)>>>,
    /// `(row, session id)` for each node's box label row, in paint order.
    node_rows: Vec<(usize, String)>,
}

impl Canvas {
    fn put(&mut self, y: usize, x: usize, text: &str, role: &LineageSpan) {
        if self.cells.len() <= y {
            self.cells.resize_with(y + 1, Vec::new);
        }
        let row = &mut self.cells[y];
        let mut cx = x;
        for ch in text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch)
                .unwrap_or(1)
                .max(1);
            if row.len() <= cx + w - 1 {
                row.resize(cx + w, None);
            }
            row[cx] = Some((ch, role.clone()));
            for pad in 1..w {
                row[cx + pad] = Some(('\0', role.clone()));
            }
            cx += w;
        }
    }

    /// Draw a lane bar only where nothing else has been painted — used to
    /// continue a parent's lane through the rows a child block occupies
    /// without overwriting the branch arrow or turn-info text on that lane.
    fn put_if_empty(&mut self, y: usize, x: usize, ch: char, role: &LineageSpan) {
        if self.cells.len() <= y {
            self.cells.resize_with(y + 1, Vec::new);
        }
        let row = &mut self.cells[y];
        if row.len() <= x {
            row.resize(x + 1, None);
        }
        if row[x].is_none() {
            row[x] = Some((ch, role.clone()));
        }
    }

    fn into_rows(self) -> Vec<LineageRow> {
        let node_by_row: HashMap<usize, String> = self.node_rows.into_iter().collect();
        self.cells
            .into_iter()
            .enumerate()
            .map(|(y, cells)| {
                let mut spans: Vec<LineageSpanRun> = Vec::new();
                let last = cells.iter().rposition(|c| c.is_some());
                if let Some(last) = last {
                    let mut cur_role: Option<LineageSpan> = None;
                    let mut cur_text = String::new();
                    for cell in cells.into_iter().take(last + 1) {
                        let (ch, role) = cell.unwrap_or((' ', LineageSpan::Rail));
                        if cur_role.as_ref() != Some(&role) {
                            if let Some(role) = cur_role.take() {
                                spans.push(LineageSpanRun {
                                    text: std::mem::take(&mut cur_text),
                                    role,
                                });
                            }
                            cur_role = Some(role);
                        }
                        if ch != '\0' {
                            cur_text.push(ch);
                        }
                    }
                    if let Some(role) = cur_role {
                        spans.push(LineageSpanRun {
                            text: cur_text,
                            role,
                        });
                    }
                }
                LineageRow {
                    spans,
                    node_session_id: node_by_row.get(&y).cloned(),
                }
            })
            .collect()
    }
}

/// Indices of the selectable (non-`More`) rows within a flattened row list,
/// in on-screen order — the shared "which rows can the cursor land on"
/// logic behind keyboard navigation. Kept here, next to `flatten`, so both
/// the lineage preview's rendering (`ui.rs::render_lineage_preview`, to
/// highlight the selected row) and its keyboard navigation
/// (`app/lineage_preview.rs`, to move/clamp the selection) share one
/// definition rather than re-deriving it.
pub fn selectable_indices(rows: &[LineageRow]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.is_selectable())
        .map(|(i, _)| i)
        .collect()
}

/// Lay the tree out as a boxed-lane diagram and cut it into renderable
/// rows. `sessions` is the same slice `build_tree` used, passed again here
/// since segment math needs live `SessionSummary` fields (`event_count`,
/// `forked_from`, `merge`) that `LineageNode` deliberately doesn't carry;
/// `now_ms` is the render frame's clock, used to compose open-ended
/// turn-info labels (the diagram is rebuilt from live state every frame,
/// so labels never go stale).
///
/// ### The diagram
///
/// Each session renders as a bordered box; its own timeline is a vertical
/// lane hanging below the box (indented one column from the box's left
/// edge), read top to bottom. A fork child branches off the parent's lane
/// with a labeled arrow into its own box (placed to the right, with its
/// own lane below it), and — when it merged back (`ForkMergeMode::Result`)
/// — returns to the parent's lane with a merge arrow. Turn info renders
/// ON the lanes, a `•` bullet sitting where the bar would be with the
/// text to its right, between the markers that bound each window; the
/// FINAL window appends `✓`/`✗` when the session ended Done/Errored:
///
/// ```text
/// ┌───────────────────────────┐
/// │ ● auth-refactor (claude)  │
/// └───────────────────────────┘
///  │
///  • 12 msgs · 8m12s
///  │
///  │                   ┌─────────────────────────────┐
///  ├─ ⑂ fork ─────────▸│ ● idea A (claude)  ↩ merged │
///  │                   └─────────────────────────────┘
///  │                    │
///  │                    • 2 msgs · 1m05s
///  │                    │
///  • 5 msgs · 3m40s     │
///  │                    │
///  │◂─ ↩ merge ─────────┘
///  │
///  • 3 msgs · 2m00s ✓
/// ```
///
/// ### Chronological order
///
/// Rows follow the node's own timeline: every branch arrow and merge
/// arrow appears at its actual position in event order (fork A, then fork
/// B, then merge A renders exactly those three connectors top to bottom —
/// a merge is never grouped with its fork's block). A fork whose merge
/// comes later keeps its lane column "live": its lane runs down to the
/// merge arrow, later branches stack to its right, and arrows crossing it
/// break around its bar.
///
/// ### Segment boundaries
///
/// The markers carving a node's own lane into windows are all on the SAME
/// counter (`SessionSummary::event_count` == `ForkedFrom::transcript_seq`
/// == `ForkMerge::merged_seq`, the transcript's own sequence counter), so
/// boundaries and deltas are plain arithmetic over data already in memory:
///
/// - `0` (the node's own creation).
/// - Each FORK child's `forked_from.transcript_seq` (subagents don't stamp
///   a parent-timeline position — spec 0014 vs spec 0078 — so a subagent
///   branch arrow never advances the checkpoint; the branch is drawn at
///   its `created_at` position in event order).
/// - Each fork child's `merge.merged_seq`, ONLY when it actually merged —
///   a discard never injects anything into the parent's transcript, so it
///   contributes no checkpoint beyond its own fork-out point.
/// - The node's own current `event_count` as the final checkpoint — except
///   when the node ITSELF has a terminal outcome (it's a fork that has
///   since merged/discarded), in which case its timeline froze at
///   `merge.at_ms` and that's the final window's end instead of "now".
///
/// A childless node still gets exactly one window — its whole life — so
/// every node's activity is visible somewhere. A window with zero messages
/// is skipped (no "0 msgs" line), leaving just the lane bar.
pub fn flatten(root: &LineageNode, sessions: &[SessionSummary], now_ms: i64) -> Vec<LineageRow> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut canvas = Canvas::default();
    let _ = layout_node(&mut canvas, root, &by_id, 1, 0, now_ms);
    canvas.into_rows()
}

/// `"● name (harness)"` box text, plus a terminal-state marker for
/// merged/discarded forks. The name is the session's title (truncated) when
/// it has one; otherwise just the harness stands alone.
fn node_box_label(summary: Option<&SessionSummary>, session_id: &str) -> String {
    let Some(s) = summary else {
        let short: String = session_id.chars().take(8).collect();
        return format!("{short} (gone)");
    };
    let status = status_glyph(s.state);
    let title = s.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let mut label = match title {
        Some(t) => {
            let name: String = if t.chars().count() > 24 {
                t.chars().take(23).chain(std::iter::once('…')).collect()
            } else {
                t.to_string()
            };
            format!("{status} {name} ({})", s.harness)
        }
        None => format!("{status} {}", s.harness),
    };
    match ForkStatus::of(s) {
        ForkStatus::Merged => label.push_str("  ↩ merged"),
        ForkStatus::Discarded => label.push_str("  ✗ discarded"),
        ForkStatus::Open => {}
    }
    label
}

/// Paint one turn-info line: a `•` bullet ON the lane, the info text two
/// columns right of it, and — when `outcome` is set (a node's final
/// window, session ended `Done`/`Errored`) — a trailing `✓`/`✗` glyph.
/// The window's numbers ride along on the Segment span role for tests.
/// Zero-message windows are the caller's job to skip. Returns one past
/// the rightmost column painted.
#[allow(clippy::too_many_arguments)]
fn put_segment(
    c: &mut Canvas,
    y: usize,
    lane: usize,
    delta_events: u64,
    start_ms: i64,
    end_ms: Option<i64>,
    now_ms: i64,
    outcome: Option<bool>,
) -> usize {
    use unicode_width::UnicodeWidthStr;
    c.put(y, lane, "•", &LineageSpan::SegmentBullet);
    let text = segment_label(delta_events, start_ms, end_ms, now_ms);
    let w = UnicodeWidthStr::width(text.as_str());
    c.put(
        y,
        lane + 2,
        &text,
        &LineageSpan::Segment {
            delta_events,
            start_ms,
            end_ms,
        },
    );
    match outcome {
        Some(ok) => {
            c.put(
                y,
                lane + 3 + w,
                if ok { "✓" } else { "✗" },
                &LineageSpan::SegmentOutcome { ok },
            );
            lane + 4 + w
        }
        None => lane + 2 + w,
    }
}

/// `✓`/`✗` for a node's final turn-info line, from its live session state
/// — `None` while it's still going (the window is just the latest one).
fn node_outcome(summary: &SessionSummary) -> Option<bool> {
    match summary.state {
        SessionState::Done => Some(true),
        SessionState::Errored => Some(false),
        _ => None,
    }
}

/// One event on a node's own timeline, used to order the diagram's rows
/// chronologically: a branch (fork-out or subagent spawn) or a fork's
/// merge-back. If session A forks, then session B forks, then A merges,
/// the diagram shows exactly that sequence top to bottom — the merge
/// arrow does NOT get grouped with its fork's block.
enum LaneEvent<'a> {
    Branch {
        at_ms: i64,
        /// Fork-out position on this node's own counter — `None` for
        /// subagents, which never advance the checkpoint (spec 0014 stamps
        /// no parent-timeline position).
        seq: Option<u64>,
        child: &'a LineageNode,
        child_idx: usize,
    },
    MergeBack {
        at_ms: i64,
        seq: u64,
        child_idx: usize,
    },
}

impl LaneEvent<'_> {
    fn sort_key(&self) -> (i64, u8, u64) {
        match self {
            // Branches sort before merges at the same instant so a child's
            // own fork-out always precedes its own merge-back.
            LaneEvent::Branch { at_ms, seq, .. } => (*at_ms, 0, seq.unwrap_or(0)),
            LaneEvent::MergeBack { at_ms, seq, .. } => (*at_ms, 1, *seq),
        }
    }
}

/// A fork child whose lane is still "live" below the current row — it
/// merged back later in the event order, so its lane column must keep
/// running down to the merge arrow and later branch arrows must be laid
/// out to its right (and drawn bridging over it) rather than through it.
struct LiveLane {
    /// The child's lane column (`child_x + 1`).
    lane: usize,
    /// One past the child's rightmost painted column — later boxes stack
    /// beyond this.
    right: usize,
    /// First row past the child's own painted block; the lane is
    /// back-filled from here down to its merge row.
    block_bottom: usize,
}

/// Emit one gap window on the node's own lane: a bar row, the bulleted
/// turn-info line (`• N msgs · elapsed`, bullet sitting on the lane), and
/// a closing bar row. A zero-message window contributes a single bar row
/// instead, keeping the lane continuous without a "0 msgs" line. Returns
/// the new current row.
#[allow(clippy::too_many_arguments)]
fn emit_gap(
    c: &mut Canvas,
    cur: usize,
    lane: usize,
    delta: u64,
    start_ms: i64,
    end_ms: Option<i64>,
    now_ms: i64,
    right: &mut usize,
) -> usize {
    c.put(cur, lane, "│", &LineageSpan::Rail);
    if delta == 0 {
        return cur + 1;
    }
    let seg_right = put_segment(c, cur + 1, lane, delta, start_ms, end_ms, now_ms, None);
    *right = (*right).max(seg_right);
    c.put(cur + 2, lane, "│", &LineageSpan::Rail);
    cur + 3
}

/// Paint `node`'s box at `(x, y)`, its lane and children below it, and
/// return `(bottom, right)`: the first free row past everything painted,
/// and one past the rightmost column painted (so siblings/parents can lay
/// out around this subtree without collisions).
fn layout_node(
    c: &mut Canvas,
    node: &LineageNode,
    by_id: &HashMap<&str, &SessionSummary>,
    x: usize,
    y: usize,
    now_ms: i64,
) -> (usize, usize) {
    use unicode_width::UnicodeWidthStr;

    let summary = by_id.get(node.session_id.as_str()).copied();
    let label = node_box_label(summary, &node.session_id);
    let lw = UnicodeWidthStr::width(label.as_str());
    let border = LineageSpan::Border {
        session_id: node.session_id.clone(),
    };
    c.put(y, x, &format!("┌{}┐", "─".repeat(lw + 2)), &border);
    c.put(y + 1, x, "│ ", &border);
    c.put(
        y + 1,
        x + 2,
        &label,
        &LineageSpan::Node {
            session_id: node.session_id.clone(),
        },
    );
    c.put(y + 1, x + 2 + lw, " │", &border);
    c.put(y + 2, x, &format!("└{}┘", "─".repeat(lw + 2)), &border);
    c.node_rows.push((y + 1, node.session_id.clone()));

    // The node's own lane hangs below the box, indented one column from
    // the box's left edge; each turn-info line puts its `•` bullet ON the
    // lane with the text following to the right.
    let lane = x + 1;
    let mut cur = y + 3;
    let mut right = x + lw + 4;
    let mut max_child_bottom = cur;

    // ── Collect this node's timeline events in chronological order. ──
    let mut events: Vec<LaneEvent> = Vec::new();
    let mut more_markers: Vec<usize> = Vec::new();
    for (child_idx, child) in node.children.iter().enumerate() {
        let cn = match child {
            LineageChild::More(n) => {
                more_markers.push(*n);
                continue;
            }
            LineageChild::Node(cn) => cn,
        };
        let cs = by_id.get(cn.session_id.as_str()).copied();
        match cn.edge {
            LineageEdge::Fork => {
                let forked = cs.and_then(|s| s.forked_from.as_ref());
                events.push(LaneEvent::Branch {
                    at_ms: forked.map(|f| f.at_ms).unwrap_or(0),
                    seq: forked.map(|f| f.transcript_seq),
                    child: cn,
                    child_idx,
                });
                if let Some(m) = cs
                    .and_then(|s| s.merge.as_ref())
                    .filter(|m| m.mode == ForkMergeMode::Result)
                {
                    events.push(LaneEvent::MergeBack {
                        at_ms: m.at_ms,
                        seq: m.merged_seq,
                        child_idx,
                    });
                }
            }
            LineageEdge::Subagent | LineageEdge::Root => {
                events.push(LaneEvent::Branch {
                    at_ms: cs.map(|s| s.created_at.timestamp_millis()).unwrap_or(0),
                    seq: None,
                    child: cn,
                    child_idx,
                });
            }
        }
    }
    events.sort_by_key(|e| e.sort_key());

    // Pre-pass: the widest turn-info label this node will emit — every
    // child box must start beyond it so lane text never runs into a box.
    let max_seg_w = {
        let mut cp = 0u64;
        let mut cp_ms = summary
            .map(|s| s.created_at.timestamp_millis())
            .unwrap_or(0);
        let mut w = 0usize;
        let mut probe = |delta: u64, start: i64, end: Option<i64>| {
            if delta > 0 {
                w = w.max(UnicodeWidthStr::width(
                    segment_label(delta, start, end, now_ms).as_str(),
                ));
            }
        };
        for ev in &events {
            match ev {
                LaneEvent::Branch {
                    seq: Some(seq),
                    at_ms,
                    ..
                } => {
                    probe(seq.saturating_sub(cp), cp_ms, Some(*at_ms));
                    cp = *seq;
                    cp_ms = *at_ms;
                }
                LaneEvent::Branch { .. } => {}
                LaneEvent::MergeBack { seq, at_ms, .. } => {
                    probe(seq.saturating_sub(cp), cp_ms, Some(*at_ms));
                    cp = *seq;
                    cp_ms = *at_ms;
                }
            }
        }
        if let Some(s) = summary {
            probe(
                s.event_count.saturating_sub(cp),
                cp_ms,
                s.merge.as_ref().map(|m| m.at_ms),
            );
        }
        w
    };

    // ── Walk the events top to bottom. ──
    let mut cp_seq: u64 = 0;
    let mut cp_ms: i64 = summary
        .map(|s| s.created_at.timestamp_millis())
        .unwrap_or(0);
    let mut live: HashMap<usize, LiveLane> = HashMap::new();
    let merges_back: HashSet<usize> = events
        .iter()
        .filter_map(|e| match e {
            LaneEvent::MergeBack { child_idx, .. } => Some(*child_idx),
            _ => None,
        })
        .collect();

    for ev in &events {
        match ev {
            LaneEvent::Branch {
                seq,
                child,
                child_idx,
                ..
            } => {
                // Close the window this fork-out ends (subagents don't
                // close one — just a connecting bar row).
                if let Some(seq) = seq {
                    let at_ms = match ev {
                        LaneEvent::Branch { at_ms, .. } => *at_ms,
                        _ => unreachable!(),
                    };
                    cur = emit_gap(
                        c,
                        cur,
                        lane,
                        seq.saturating_sub(cp_seq),
                        cp_ms,
                        Some(at_ms),
                        now_ms,
                        &mut right,
                    );
                    cp_seq = *seq;
                    cp_ms = at_ms;
                } else {
                    c.put(cur, lane, "│", &LineageSpan::Rail);
                    cur += 1;
                }

                let edge_word = match child.edge {
                    LineageEdge::Fork => "⑂ fork",
                    LineageEdge::Subagent => "▸ subagent",
                    LineageEdge::Root => "",
                };
                let ew = UnicodeWidthStr::width(edge_word);
                // Column: past the arrow's own minimum, past the widest
                // turn-info label, and past every still-live sibling lane.
                // Bullet (1) + gap (1) + widest label + 2 blank columns.
                let mut child_x = (lane + ew + 6).max(lane + max_seg_w + 4);
                for lane_state in live.values() {
                    child_x = child_x.max(lane_state.right + 2);
                }

                let (child_bottom, child_right) =
                    layout_node(c, child, by_id, child_x, cur, now_ms);
                max_child_bottom = max_child_bottom.max(child_bottom);
                right = right.max(child_right);

                // Branch arrow into the child's box label row. Live sibling
                // lanes crossed by the shaft get their bar painted first so
                // the dashes (put_if_empty) break around them — the lane
                // visibly passes in front of the arrow.
                let ay = cur + 1;
                for lane_state in live.values() {
                    if lane_state.block_bottom <= ay {
                        c.put_if_empty(ay, lane_state.lane, '│', &LineageSpan::Rail);
                    }
                }
                c.put(ay, lane, "├─", &LineageSpan::Rail);
                c.put(ay, lane + 3, edge_word, &LineageSpan::Edge(child.edge));
                let dash_from = lane + 3 + ew + 1;
                for dx in dash_from..child_x.saturating_sub(1) {
                    c.put_if_empty(ay, dx, '─', &LineageSpan::Rail);
                }
                c.put(ay, child_x - 1, "▸", &LineageSpan::Rail);

                if merges_back.contains(child_idx) {
                    live.insert(
                        *child_idx,
                        LiveLane {
                            lane: child_x + 1,
                            right: child_right,
                            block_bottom: child_bottom,
                        },
                    );
                }
                cur = child_bottom;
            }
            LaneEvent::MergeBack {
                seq,
                at_ms,
                child_idx,
            } => {
                cur = emit_gap(
                    c,
                    cur,
                    lane,
                    seq.saturating_sub(cp_seq),
                    cp_ms,
                    Some(*at_ms),
                    now_ms,
                    &mut right,
                );
                cp_seq = *seq;
                cp_ms = *at_ms;

                let Some(lane_state) = live.remove(child_idx) else {
                    continue; // merge without a rendered branch: nothing to draw
                };
                // Extend the child's lane down from its block to the merge
                // arrow row, then draw the arrow flowing child → parent.
                for yy in lane_state.block_bottom..cur {
                    c.put_if_empty(yy, lane_state.lane, '│', &LineageSpan::Rail);
                }
                for other in live.values() {
                    if other.block_bottom <= cur && other.lane < lane_state.lane {
                        c.put_if_empty(cur, other.lane, '│', &LineageSpan::Rail);
                    }
                }
                c.put(cur, lane, "│", &LineageSpan::Rail);
                let word = "◂─ ↩ merge ";
                c.put(cur, lane + 1, word, &LineageSpan::Rail);
                let from = lane + 1 + UnicodeWidthStr::width(word);
                for dx in from..lane_state.lane {
                    c.put_if_empty(cur, dx, '─', &LineageSpan::Rail);
                }
                c.put(cur, lane_state.lane, "┘", &LineageSpan::Rail);
                right = right.max(lane_state.lane + 1);
                cur += 1;
            }
        }
    }

    for n in more_markers {
        c.put(cur, lane, "├─ ", &LineageSpan::Rail);
        c.put(cur, lane + 3, &format!("+{n} more"), &LineageSpan::More(n));
        cur += 1;
    }

    // Trailing window: last checkpoint → now, or → this node's own
    // terminal point if it has one (it's a fork that has merged/discarded).
    // Being the node's FINAL window, it carries the terminal-outcome glyph
    // when the session has ended (`✓` Done / `✗` Errored).
    if let Some(s) = summary {
        let delta = s.event_count.saturating_sub(cp_seq);
        if delta > 0 {
            let end = s.merge.as_ref().map(|m| m.at_ms);
            c.put(cur, lane, "│", &LineageSpan::Rail);
            let seg_right =
                put_segment(c, cur + 1, lane, delta, cp_ms, end, now_ms, node_outcome(s));
            right = right.max(seg_right);
            cur += 2;
        }
    }

    // The node's own lane runs unbroken from its box down to its last
    // painted row — through child-block rows where it had no content of
    // its own (put_if_empty leaves everything already painted alone).
    for yy in (y + 3)..cur {
        c.put_if_empty(yy, lane, '│', &LineageSpan::Rail);
    }

    (cur.max(max_child_bottom), right)
}

/// Status glyph for a node — reuses [`SessionState::glyph`], the same
/// vocabulary the session list and `/tasks` popup already use, rather than
/// inventing a parallel icon set.
pub fn status_glyph(state: SessionState) -> &'static str {
    state.glyph()
}

/// Compact elapsed-time label (`"3s"`, `"12m34s"`) from `since_ms` (epoch
/// ms) to `now_ms`.
pub fn format_elapsed_ms(since_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(since_ms).max(0) / 1000;
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Renderable text for one activity-segment row: `"N msg(s) · elapsed"`.
/// `end_ms` is the segment's own end when known, else `now_ms` (the render
/// frame's live clock) — same split `render_lineage_row` used to take
/// `now_ms` for per-node stats before those moved to segments. Cost is
/// deliberately not shown here (unlike the old per-node stats label): it's
/// a single cumulative total on `SessionSummary`, with no per-checkpoint
/// snapshot the way `event_count` has via `transcript_seq`/`merged_seq`, so
/// there's no correct way to attribute it to one window rather than another.
pub fn segment_label(delta_events: u64, start_ms: i64, end_ms: Option<i64>, now_ms: i64) -> String {
    let elapsed = format_elapsed_ms(start_ms, end_ms.unwrap_or(now_ms));
    let unit = if delta_events == 1 { "msg" } else { "msgs" };
    format!("{delta_events} {unit} \u{00b7} {elapsed}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{ForkMerge, ForkedFrom};
    use chrono::{TimeZone, Utc};

    fn base(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            harness: "smith".into(),
            cwd: "/tmp".into(),
            title: None,
            state: SessionState::Running,
            created_at: Utc.timestamp_opt(0, 0).unwrap(),
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
            kind: SessionKind::User,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }

    fn forked_from(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.forked_from = Some(ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq: 0,
            at_ms: 0,
        });
        s
    }

    fn subagent_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.kind = SessionKind::Subagent;
        s.parent_session_id = Some(parent.to_string());
        s
    }

    /// Like `forked_from`, but with explicit `transcript_seq`/`at_ms` —
    /// needed for segment-boundary tests, where `forked_from`'s always-zero
    /// defaults would collapse every window to zero length.
    fn forked_from_at(
        mut s: SessionSummary,
        parent: &str,
        transcript_seq: u64,
        at_ms: i64,
    ) -> SessionSummary {
        s.forked_from = Some(ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq,
            at_ms,
        });
        s
    }

    fn merged_at(
        mut s: SessionSummary,
        mode: ForkMergeMode,
        merged_seq: u64,
        at_ms: i64,
    ) -> SessionSummary {
        s.merge = Some(ForkMerge {
            mode,
            at_ms,
            merged_seq,
        });
        s
    }

    fn with_created_at_ms(mut s: SessionSummary, ms: i64) -> SessionSummary {
        s.created_at = Utc.timestamp_millis_opt(ms).unwrap();
        s
    }

    fn with_event_count(mut s: SessionSummary, n: u64) -> SessionSummary {
        s.event_count = n;
        s
    }

    /// Every turn-info window's `delta_events`, in on-screen (row-major)
    /// order — the shared assertion helper for the segment-boundary tests
    /// below.
    fn segment_deltas(rows: &[LineageRow]) -> Vec<u64> {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .filter_map(|s| match &s.role {
                LineageSpan::Segment { delta_events, .. } => Some(*delta_events),
                _ => None,
            })
            .collect()
    }

    /// Same, but the full `(delta, start, end)` triples.
    fn segments(rows: &[LineageRow]) -> Vec<(u64, i64, Option<i64>)> {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .filter_map(|s| match &s.role {
                LineageSpan::Segment {
                    delta_events,
                    start_ms,
                    end_ms,
                } => Some((*delta_events, *start_ms, *end_ms)),
                _ => None,
            })
            .collect()
    }

    /// The diagram's plain-text lines (right-trimmed) — for shape asserts.
    fn diagram_text(rows: &[LineageRow]) -> Vec<String> {
        rows.iter()
            .map(|r| r.text().trim_end().to_string())
            .collect()
    }

    #[test]
    fn single_session_is_a_lone_root() {
        let sessions = vec![base("a")];
        let tree = build_tree("a", &sessions).expect("tree");
        assert_eq!(tree.session_id, "a");
        assert_eq!(tree.edge, LineageEdge::Root);
        assert!(tree.children.is_empty());
    }

    #[test]
    fn unknown_focus_session_returns_none() {
        let sessions = vec![base("a")];
        assert!(build_tree("ghost", &sessions).is_none());
    }

    #[test]
    fn fork_and_subagent_children_coexist_with_distinct_edges() {
        let sessions = vec![
            base("a"),
            forked_from(base("a-fork"), "a"),
            subagent_of(base("a-sub"), "a"),
        ];
        let tree = build_tree("a", &sessions).expect("tree");
        assert_eq!(tree.children.len(), 2);
        // Subagents sort before forks (see build_subtree).
        let LineageChild::Node(first) = &tree.children[0] else {
            panic!("expected node")
        };
        assert_eq!(first.session_id, "a-sub");
        assert_eq!(first.edge, LineageEdge::Subagent);
        let LineageChild::Node(second) = &tree.children[1] else {
            panic!("expected node")
        };
        assert_eq!(second.session_id, "a-fork");
        assert_eq!(second.edge, LineageEdge::Fork);
    }

    #[test]
    fn opening_the_view_from_any_descendant_finds_the_same_root() {
        let sessions = vec![
            base("a"),
            forked_from(base("b"), "a"),
            forked_from(base("c"), "b"),
        ];
        for focus in ["a", "b", "c"] {
            let tree = build_tree(focus, &sessions).expect("tree");
            assert_eq!(
                tree.session_id, "a",
                "focus {focus} should resolve to root a"
            );
        }
    }

    #[test]
    fn recursive_fork_of_a_fork_nests_boxes_rightward() {
        let sessions = vec![
            base("a"),
            forked_from(base("b"), "a"),
            forked_from(base("c"), "b"),
        ];
        let tree = build_tree("a", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        // Each nesting level's box starts strictly further right — measured
        // as the column where the node label span begins on that node's row.
        let label_col = |id: &str| {
            let row = rows
                .iter()
                .find(|r| r.session_id() == Some(id))
                .unwrap_or_else(|| panic!("{id} row"));
            let mut col = 0usize;
            for span in &row.spans {
                if matches!(&span.role, LineageSpan::Node { .. }) {
                    return col;
                }
                col += span.text.chars().count();
            }
            panic!("{id} has no node span");
        };
        assert!(label_col("a") < label_col("b"));
        assert!(label_col("b") < label_col("c"));
    }

    #[test]
    fn breadth_beyond_cap_collapses_into_a_more_marker() {
        let mut sessions = vec![base("root")];
        for i in 0..(MAX_SIBLINGS + 5) {
            sessions.push(forked_from(base(&format!("f{i}")), "root"));
        }
        let tree = build_tree("root", &sessions).unwrap();
        assert_eq!(tree.children.len(), MAX_SIBLINGS + 1); // +1 for the More marker
        let last = tree.children.last().unwrap();
        assert_eq!(*last, LineageChild::More(5));
    }

    #[test]
    fn depth_beyond_cap_collapses_into_a_more_marker() {
        // A straight-line chain deeper than MAX_DEPTH.
        let mut sessions = vec![base("s0")];
        for i in 1..(MAX_DEPTH + 3) {
            sessions.push(forked_from(base(&format!("s{i}")), &format!("s{}", i - 1)));
        }
        let tree = build_tree("s0", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        // Depths 0..MAX_DEPTH-1 render as real nodes; beyond that collapses.
        assert!(rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .any(|s| matches!(s.role, LineageSpan::More(_))));
        assert!(
            selectable_indices(&rows).len() < MAX_DEPTH + 3,
            "the collapsed tail must not materialize as selectable node rows"
        );
    }

    #[test]
    fn fork_status_reflects_merge_outcome() {
        let mut open = forked_from(base("f"), "root");
        assert_eq!(ForkStatus::of(&open), ForkStatus::Open);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Result,
            at_ms: 0,
            merged_seq: 0,
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Merged);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Discard,
            at_ms: 0,
            merged_seq: 0,
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Discarded);
    }

    #[test]
    fn diagram_matches_the_concept_layout_for_a_single_merged_fork() {
        // The canonical scenario from the concept sketch: a parent whose
        // fork merged back. Locks in the full shape — boxes, lanes (indented
        // one column from the box edge, turn info outdented two from the
        // lane), labeled branch/merge arrows, and chronological row order
        // (fork-out, the fork's life, the parent's while-fork-was-out
        // window, merge-back, trailing).
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 12, 300_000), 300_000),
                2,
            ),
            ForkMergeMode::Result,
            15,
            500_000,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 800_000);
        let g = status_glyph(SessionState::Running);
        assert_eq!(
            diagram_text(&rows),
            vec![
                " ┌─────────┐".to_string(),
                format!(" │ {g} smith │"),
                " └─────────┘".to_string(),
                "  │".to_string(),
                "  • 12 msgs · 5m00s".to_string(),
                "  │".to_string(),
                "  │                  ┌───────────────────┐".to_string(),
                format!("  ├─ ⑂ fork ────────▸│ {g} smith  ↩ merged │"),
                "  │                  └───────────────────┘".to_string(),
                "  │                   │".to_string(),
                "  │                   • 2 msgs · 3m20s".to_string(),
                "  │                   │".to_string(),
                "  • 3 msgs · 3m20s    │".to_string(),
                "  │                   │".to_string(),
                "  │◂─ ↩ merge ────────┘".to_string(),
                "  │".to_string(),
                "  • 5 msgs · 5m00s".to_string(),
            ]
        );
        // The two box label rows are the (only) selectable rows, in
        // parent-then-child order.
        let ids: Vec<_> = selectable_indices(&rows)
            .into_iter()
            .map(|i| rows[i].session_id().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["root".to_string(), "f".to_string()]);
    }

    #[test]
    fn final_turn_info_carries_the_terminal_outcome_glyph() {
        // A node's LAST turn-info line appends `✓` when the session ended
        // Done and `✗` when it Errored; mid-timeline windows and still-live
        // sessions keep the plain `•` bullet only.
        let mut done = with_event_count(with_created_at_ms(base("done"), 0), 3);
        done.state = SessionState::Done;
        let rows = flatten(
            &build_tree("done", &[done.clone()]).unwrap(),
            &[done],
            9_000,
        );
        let text = diagram_text(&rows).join("\n");
        assert!(text.contains("• 3 msgs"), "{text}");
        assert!(
            rows.iter()
                .flat_map(|r| r.spans.iter())
                .any(
                    |s| matches!(s.role, LineageSpan::SegmentOutcome { ok: true }) && s.text == "✓"
                ),
            "{text}"
        );

        let mut errored = with_event_count(with_created_at_ms(base("err"), 0), 3);
        errored.state = SessionState::Errored;
        let rows = flatten(
            &build_tree("err", &[errored.clone()]).unwrap(),
            &[errored],
            9_000,
        );
        assert!(rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .any(|s| matches!(s.role, LineageSpan::SegmentOutcome { ok: false }) && s.text == "✗"));

        // Still running: no outcome glyph anywhere.
        let live = with_event_count(with_created_at_ms(base("live"), 0), 3);
        let rows = flatten(
            &build_tree("live", &[live.clone()]).unwrap(),
            &[live],
            9_000,
        );
        assert!(!rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .any(|s| matches!(s.role, LineageSpan::SegmentOutcome { .. })));
    }

    #[test]
    fn mid_timeline_windows_never_carry_an_outcome_glyph() {
        // A Done parent with a fork: only the parent's FINAL window (and
        // the fork's, if it ended) gets the glyph — the pre-fork window
        // stays a plain bullet even though the session is Done overall.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        root.state = SessionState::Done;
        let fork = with_event_count(
            with_created_at_ms(forked_from_at(base("f"), "root", 12, 500), 500),
            2,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        let outcome_count = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .filter(|s| matches!(s.role, LineageSpan::SegmentOutcome { .. }))
            .count();
        assert_eq!(
            outcome_count, 1,
            "only root's trailing window carries the ✓ — not its pre-fork \
             window, and not the still-open fork's"
        );
        // And it's on the LAST turn-info row.
        let last_seg_row = rows
            .iter()
            .rposition(|r| {
                r.spans
                    .iter()
                    .any(|s| matches!(s.role, LineageSpan::Segment { .. }))
            })
            .unwrap();
        assert!(rows[last_seg_row]
            .spans
            .iter()
            .any(|s| matches!(s.role, LineageSpan::SegmentOutcome { ok: true })));
    }

    #[test]
    fn events_render_in_chronological_order_fork_a_fork_b_merge_a() {
        // Fork A, then fork B, then A merges back — the three connectors
        // must appear in exactly that order top to bottom (a merge is not
        // grouped with its fork's block), and B, branching while A's lane
        // is still live, stacks to A's right with the arrow bridging over
        // A's lane rather than through it.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 30);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                4,
            ),
            ForkMergeMode::Result,
            12,
            5_000,
        );
        let b = with_event_count(
            with_created_at_ms(forked_from_at(base("b"), "root", 8, 3_000), 3_000),
            2,
        );
        let sessions = vec![root, a, b];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        let text = diagram_text(&rows);

        let fork_rows: Vec<usize> = text
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains("⑂ fork"))
            .map(|(i, _)| i)
            .collect();
        // "◂─ ↩ merge" is the arrow; a bare "↩ merge" would also match the
        // merged box's own "↩ merged" label suffix.
        let merge_row = text
            .iter()
            .position(|l| l.contains("◂─ ↩ merge"))
            .expect("merge arrow row");
        assert_eq!(fork_rows.len(), 2, "{text:#?}");
        assert!(
            fork_rows[0] < fork_rows[1] && fork_rows[1] < merge_row,
            "expected fork-A, fork-B, merge-A top to bottom; got forks at \
             {fork_rows:?}, merge at {merge_row}:\n{}",
            text.join("\n")
        );

        // B's box sits right of A's (A's lane was still live when B
        // branched), and B's branch arrow bridges over A's lane — the bar
        // survives inside the arrow's shaft on that row.
        let a_row = rows
            .iter()
            .position(|r| r.session_id() == Some("a"))
            .expect("a's box row");
        let b_row = rows
            .iter()
            .position(|r| r.session_id() == Some("b"))
            .expect("b's box row");
        let label_col = |ri: usize| {
            let mut col = 0usize;
            for span in &rows[ri].spans {
                if matches!(&span.role, LineageSpan::Node { .. }) {
                    return col;
                }
                col += span.text.chars().count();
            }
            unreachable!()
        };
        assert!(label_col(b_row) > label_col(a_row), "{}", text.join("\n"));
        let b_arrow_line = &text[fork_rows[1]];
        assert!(
            b_arrow_line.contains("─│") || b_arrow_line.contains("│─"),
            "B's arrow shaft must bridge over A's live lane bar: {b_arrow_line}"
        );

        // Segment order is chronological too: pre-A (5), A's own life (4),
        // root while only A was out (3 = seq 8 - seq 5, closed by B's
        // fork-out), B's own life (2), root from B's fork-out to A's
        // merge-back (4 = seq 12 - seq 8), trailing (18 = 30 - 12).
        assert_eq!(segment_deltas(&rows), vec![5, 4, 3, 2, 4, 18]);
    }

    #[test]
    fn discarded_fork_gets_no_merge_arrow_and_a_struck_marker() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 10);
        let fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 4, 1_000), 1_000),
                3,
            ),
            ForkMergeMode::Discard,
            999, // deliberately poisoned: a discard must never be read as a checkpoint
            2_000,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        let text = diagram_text(&rows).join("\n");
        assert!(
            !text.contains("◂─"),
            "a discarded fork must not draw a merge-back arrow:\n{text}"
        );
        assert!(text.contains("✗ discarded"), "{text}");
        // Root: 4 before the fork, then its trailing window counts from the
        // FORK-OUT point (4), not the poisoned discard seq: 10 - 4 = 6. The
        // fork's own life (3) sits in between.
        assert_eq!(segment_deltas(&rows), vec![4, 3, 6]);
    }

    #[test]
    fn segment_label_reports_message_count_and_elapsed() {
        let label = segment_label(42, 0, Some(65_000), 999_999);
        assert!(label.contains("42 msgs"));
        assert!(label.contains("1m05s"));
    }

    #[test]
    fn segment_label_singular_for_one_message() {
        let label = segment_label(1, 0, Some(1_000), 999_999);
        assert!(label.contains("1 msg "), "expected singular 'msg': {label}");
        assert!(!label.contains("msgs"));
    }

    #[test]
    fn segment_label_falls_back_to_now_when_end_is_open() {
        // An open-ended segment (`end_ms: None`) measures against the live
        // render-time clock (`now_ms`), not a baked-in end.
        let label = segment_label(3, 0, None, 5_000);
        assert!(
            label.contains("5s"),
            "expected elapsed against now_ms: {label}"
        );
    }

    #[test]
    fn has_lineage_is_false_for_an_ordinary_session() {
        let sessions = vec![base("a"), base("b")];
        assert!(!has_lineage("a", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_fork_itself() {
        let sessions = vec![base("root"), forked_from(base("f"), "root")];
        assert!(has_lineage("f", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_session_with_a_fork_descendant() {
        let sessions = vec![base("root"), forked_from(base("f"), "root")];
        assert!(has_lineage("root", &sessions));
    }

    #[test]
    fn has_lineage_is_true_for_a_session_with_a_subagent_descendant() {
        let sessions = vec![base("root"), subagent_of(base("sub"), "root")];
        assert!(has_lineage("root", &sessions));
    }

    #[test]
    fn has_lineage_is_false_for_an_unknown_session_id() {
        let sessions = vec![base("root")];
        assert!(!has_lineage("ghost", &sessions));
    }

    #[test]
    fn selectable_indices_skips_more_markers() {
        let mut sessions = vec![base("root")];
        for i in 0..(MAX_SIBLINGS + 2) {
            sessions.push(forked_from(base(&format!("f{i}")), "root"));
        }
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        let selectable = selectable_indices(&rows);
        assert_eq!(
            selectable.len(),
            MAX_SIBLINGS + 1,
            "the collapsed +N more row must not count as selectable"
        );
        for idx in selectable {
            assert!(rows[idx].is_selectable());
        }
    }

    #[test]
    fn leaf_node_gets_a_single_trailing_segment() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 9);
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        assert_eq!(
            segments(&rows),
            vec![(9, 0, None)],
            "a childless node gets one segment covering its whole life, \
             open-ended (its end is \"now\" at render time, not baked in)"
        );
    }

    #[test]
    fn leaf_forks_trailing_segment_ends_at_its_own_merge_not_now() {
        // A fork that has itself merged/discarded froze at that instant —
        // its own trailing segment must end there, not keep growing against
        // a live "now" the way a still-open node's does.
        let fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 5, 1_000), 1_000),
                7,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let sessions = vec![base("root"), fork];
        // `build_tree` walks up to the topmost ancestor — here that's
        // "root", with "f" as its child — so "f"'s own leaf segment is the
        // SECOND segment row (root's own "before f forked" segment comes
        // first); find it by its distinctive delta rather than assuming
        // position.
        let tree = build_tree("f", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 999_999);
        let seg = segments(&rows)
            .into_iter()
            .find(|(d, _, _)| *d == 7)
            .expect("f's own leaf segment (delta_events = f.event_count = 7)");
        assert_eq!(seg, (7, 1_000, Some(3_000)));
    }

    #[test]
    fn single_open_fork_produces_a_pre_fork_and_a_fork_own_segment() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let fork = with_event_count(
            with_created_at_ms(forked_from_at(base("f"), "root", 12, 500), 500),
            2,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        // root: [0, 12) before the fork, then [12, 20) since the fork (still
        // open, so it's a trailing "to now" segment AFTER the fork's block —
        // an open fork has no merge-back point to pin a side-by-side window
        // to); f: its own [0, 2) life, still open too.
        assert_eq!(segment_deltas(&rows), vec![12, 2, 8]);
    }

    #[test]
    fn multiple_forks_mixed_merged_discarded_open_produce_the_expected_segment_sequence() {
        // root -> A (merged) -> B (discarded) -> C (still open), with root
        // continuing to accrue its own messages between each.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 30);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                7,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let b = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("b"), "root", 15, 4_000), 4_000),
                3,
            ),
            ForkMergeMode::Discard,
            // A discard's own `merged_seq`/`at_ms` must NOT move the
            // parent's checkpoint — deliberately set to values that would
            // fail the assertions below if the implementation used them.
            999,
            5_000,
        );
        let c = with_event_count(
            with_created_at_ms(forked_from_at(base("c"), "root", 20, 6_000), 6_000),
            2,
        );
        let sessions = vec![root, a, b, c];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        assert_eq!(
            segment_deltas(&rows),
            vec![
                5, // root, before A forked: [0, 5)
                7, // A's own whole life (its block renders right after its
                // branch arrow)
                5, // root, while A was out ([5, 10)) — emitted at A's
                // merge-back event, chronologically after A's block and
                // before the merge arrow
                5, // root, between A merging back (seq 10) and B forking (seq 15)
                3, // B's own whole life
                5, // root, between B forking (seq 15, a discard doesn't move the
                // checkpoint past it) and C forking (seq 20)
                2,  // C's own whole life
                10, // root, since C forked (seq 20) to root's current event_count (30)
            ]
        );
    }

    #[test]
    fn a_merge_boundary_with_zero_gap_is_skipped_not_rendered_as_zero() {
        // A fork whose merge lands exactly where the next fork branches off
        // (root did nothing of its own in between) must not render a "0
        // msgs" line.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 12);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                4,
            ),
            ForkMergeMode::Result,
            8,
            2_000,
        );
        // b forks out exactly at seq 8 — the same point a merged back.
        let b = with_event_count(
            with_created_at_ms(forked_from_at(base("b"), "root", 8, 2_000), 2_000),
            1,
        );
        let sessions = vec![root, a, b];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        assert_eq!(
            segment_deltas(&rows),
            vec![
                5, // root before a forked
                4, // a's own life
                3, // root while a was out ([5, 8)) — b forks at the very
                // instant a merges, and branches sort before merges on a
                // time tie, so this window closes at b's BRANCH event
                // (seq 8, same boundary)
                1, // b's own life
                // no zero-length window at a's merge-back (seq 8 → seq 8)
                4, // root since the seq-8 boundary to now (event_count 12)
            ]
        );
    }

    #[test]
    fn subagent_children_do_not_split_the_parent_timeline() {
        // A node with only subagent children (no forks) gets exactly one
        // trailing segment for its whole life, positioned after the
        // subagent's own subtree — subagents don't stamp a parent-timeline
        // checkpoint the way forks do (spec 0014 has no `transcript_seq`).
        let root = with_event_count(with_created_at_ms(base("root"), 0), 9);
        let sub = with_event_count(with_created_at_ms(subagent_of(base("s"), "root"), 500), 2);
        let sessions = vec![root, sub];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        assert_eq!(
            segment_deltas(&rows),
            vec![2, 9],
            "s's own leaf segment, then root's whole-life segment (unsplit by the subagent)"
        );
        // And root's segment must come after s's entire subtree in render
        // order (it's the last row), and the branch arrow must be labeled
        // as a subagent edge, not a fork.
        assert!(rows.last().unwrap().spans.iter().any(|s| matches!(
            s.role,
            LineageSpan::Segment {
                delta_events: 9,
                ..
            }
        )));
        assert!(rows.iter().flat_map(|r| r.spans.iter()).any(|s| matches!(
            s.role,
            LineageSpan::Edge(LineageEdge::Subagent)
        ) && s.text == "▸ subagent"));
    }

    #[test]
    fn segment_rows_are_never_selectable() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 5);
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        for idx in selectable_indices(&rows) {
            assert!(!rows[idx]
                .spans
                .iter()
                .any(|s| matches!(s.role, LineageSpan::Segment { .. })));
        }
        assert!(
            rows.iter()
                .flat_map(|r| r.spans.iter())
                .any(|s| matches!(s.role, LineageSpan::Segment { .. })),
            "sanity: this tree does have a turn-info row"
        );
    }

    #[test]
    fn wide_characters_in_titles_keep_box_borders_aligned() {
        // A CJK title occupies two columns per character — the box's right
        // border and closing corner must land at the same display column on
        // all three box rows, or the diagram shears.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 1);
        root.title = Some("한글 제목".to_string());
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 0);
        let text = diagram_text(&rows);
        let width = |s: &str| {
            s.chars()
                .map(|c| {
                    unicode_width::UnicodeWidthChar::width(c)
                        .unwrap_or(1)
                        .max(1)
                })
                .sum::<usize>()
        };
        assert_eq!(width(&text[0]), width(&text[1]), "{text:?}");
        assert_eq!(width(&text[1]), width(&text[2]), "{text:?}");
        assert!(text[1].contains("한글 제목"), "{text:?}");
    }
}
