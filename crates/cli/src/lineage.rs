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
///  • 5 msgs · 3m40s     • 2 msgs · 1m05s
///  │                    │
///  │◂─ ↩ merge ─────────┘
///  │
///  • 3 msgs · 2m00s ✓
/// ```
///
/// ### One global timeline
///
/// Rows are allocated from ONE time-ordered queue of events across the
/// whole tree — every fork-out, subagent spawn, merge-back, and lane end
/// (a session going Done/Errored, a fork being discarded, or "now" for
/// live sessions) gets its rows at its actual position in global time
/// order. Fork A, then fork B, then merge A renders exactly those three
/// connectors top to bottom, no matter which lane they belong to. A turn
/// -info window renders at the row where its CLOSING event lands, on its
/// own lane — so windows that close at the same instant (a merged fork's
/// life and its parent's while-it-was-out window both close at the merge;
/// several live lanes all "close" at now) share one row side by side,
/// like the concept sketch. A lane whose end comes later stays live: its
/// column keeps running down to its closing arrow/turn-info, later boxes
/// stack to its right, and arrows crossing it break around its bar.
///
/// ### Segment boundaries
///
/// The markers carving a lane into windows are all on the SAME counter
/// (`SessionSummary::event_count` == `ForkedFrom::transcript_seq` ==
/// `ForkMerge::merged_seq`, the transcript's own sequence counter), so
/// boundaries and deltas are plain arithmetic over data already in memory:
///
/// - `0` (the lane's own creation).
/// - Each FORK child's `forked_from.transcript_seq` (subagents don't stamp
///   a parent-timeline position — spec 0014 vs spec 0078 — so a subagent
///   branch arrow never advances the checkpoint; the branch is drawn at
///   its `created_at` position in event order).
/// - Each fork child's `merge.merged_seq`, ONLY when it actually merged —
///   a discard never injects anything into the parent's transcript, so it
///   contributes no checkpoint beyond its own fork-out point.
/// - The lane's own current `event_count` as the final checkpoint, closing
///   at the lane's end event: its merge-back, its discard, the moment its
///   session went Done/Errored (`last_event_at`), or "now" while live.
///
/// A childless node still gets exactly one window — its whole life — so
/// every node's activity is visible somewhere. A window with zero messages
/// is skipped (no "0 msgs" line), leaving just the lane bar.
pub fn flatten(root: &LineageNode, sessions: &[SessionSummary], now_ms: i64) -> Vec<LineageRow> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut canvas = Canvas::default();
    layout_tree(&mut canvas, root, &by_id, now_ms);
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
/// One session's lane in the global-timeline layout: its box, its lane
/// column, its timeline facts (branch/merge/end times, all clamped so a
/// child never precedes its parent), and the running checkpoint state used
/// to carve its own timeline into windows as the global event walk passes
/// through it.
struct Lane<'a> {
    node: &'a LineageNode,
    summary: Option<&'a SessionSummary>,
    parent: Option<usize>,
    /// When this lane's box appears: fork-out time for forks, creation
    /// time otherwise.
    box_ms: i64,
    /// `forked_from.transcript_seq` — fork lanes only.
    fork_seq: Option<u64>,
    /// `(at_ms, merged_seq)` for forks that merged back (`Result`) — such
    /// a lane ends at its merge arrow instead of an `End` event.
    merge: Option<(i64, u64)>,
    /// `(at_ms, label end, outcome)` for every other lane's end: a
    /// discarded fork (at discard time), a session that went Done/Errored
    /// (at `last_event_at`), or a live session (at "now", open-ended).
    end: Option<(i64, Option<i64>, Option<bool>)>,
    /// Collapsed "+N more" markers among this lane's children.
    more: Vec<usize>,
    /// Widest turn-info label this lane can emit (outcome glyph included)
    /// — later boxes must clear it so lane text never runs into a box.
    max_seg_w: usize,
    label: String,
    // Running state, filled in as the global walk proceeds.
    cp_seq: u64,
    cp_ms: i64,
    lane_col: usize,
    box_bottom: usize,
    /// Last row this lane's content reaches (inclusive) — the end-fill
    /// draws its bar from `box_bottom` down to here.
    last_row: usize,
    placed: bool,
    ended: bool,
}

/// Event kinds on the global timeline, in tie-break order: a box appears
/// (fork-out/subagent spawn/root creation), a fork merges back, a lane
/// ends. Branches sort before merges at the same instant so e.g. a fork
/// branching at the very moment a sibling merges renders above that merge.
const EV_BOX: u8 = 0;
const EV_MERGE: u8 = 1;
const EV_END: u8 = 2;

/// Flatten the tree into `lanes` (DFS order, parents before children) and
/// return the new lane's index.
fn collect_lanes<'a>(
    node: &'a LineageNode,
    by_id: &HashMap<&str, &'a SessionSummary>,
    parent: Option<usize>,
    now_ms: i64,
    lanes: &mut Vec<Lane<'a>>,
) -> usize {
    let summary = by_id.get(node.session_id.as_str()).copied();
    let parent_box_ms = parent.map(|p| lanes[p].box_ms).unwrap_or(i64::MIN);
    let forked = if node.edge == LineageEdge::Fork {
        summary.and_then(|s| s.forked_from.as_ref())
    } else {
        None
    };
    let created = summary
        .map(|s| s.created_at.timestamp_millis())
        .unwrap_or(0);
    // Clamp to the parent's box time so clock skew can never place a
    // child's box above its parent's.
    let box_ms = forked
        .map(|f| f.at_ms)
        .unwrap_or(created)
        .max(parent_box_ms);
    let merge_rec = summary.and_then(|s| s.merge.as_ref());
    let merge = if forked.is_some() {
        merge_rec
            .filter(|m| m.mode == ForkMergeMode::Result)
            .map(|m| (m.at_ms.max(box_ms), m.merged_seq))
    } else {
        None
    };
    let end = if merge.is_some() {
        None
    } else if let Some(m) = merge_rec.filter(|m| m.mode == ForkMergeMode::Discard) {
        // A discarded fork's lane ends at the discard, its final window
        // frozen there. No outcome glyph: the box label's "✗ discarded"
        // already carries the outcome.
        let t = m.at_ms.max(box_ms);
        Some((t, Some(m.at_ms), node_outcome_of(summary)))
    } else if let Some(ok) = node_outcome_of(summary) {
        // Session reached a terminal state: the lane ends when its last
        // event landed, and the final window carries ✓/✗.
        let t = summary
            .and_then(|s| s.last_event_at.map(|d| d.timestamp_millis()))
            .unwrap_or(now_ms)
            .max(box_ms);
        Some((t, Some(t), Some(ok)))
    } else {
        // Live: the lane runs to "now", its final window open-ended.
        Some((now_ms.max(box_ms), None, None))
    };
    let idx = lanes.len();
    lanes.push(Lane {
        node,
        summary,
        parent,
        box_ms,
        fork_seq: forked.map(|f| f.transcript_seq),
        merge,
        end,
        more: Vec::new(),
        max_seg_w: 0,
        label: node_box_label(summary, &node.session_id),
        cp_seq: 0,
        cp_ms: box_ms,
        lane_col: 0,
        box_bottom: 0,
        last_row: 0,
        placed: false,
        ended: false,
    });
    for child in &node.children {
        match child {
            LineageChild::More(n) => lanes[idx].more.push(*n),
            LineageChild::Node(cn) => {
                collect_lanes(cn, by_id, Some(idx), now_ms, lanes);
            }
        }
    }
    idx
}

fn node_outcome_of(summary: Option<&SessionSummary>) -> Option<bool> {
    summary.and_then(node_outcome)
}

/// Build the global event queue: `(t, kind, lane)` sorted by time, then
/// kind, then lane index (DFS order — parents before children on full
/// ties).
fn build_events(lanes: &[Lane]) -> Vec<(i64, u8, usize)> {
    let mut events: Vec<(i64, u8, usize)> = Vec::new();
    for (i, lane) in lanes.iter().enumerate() {
        events.push((lane.box_ms, EV_BOX, i));
        if let Some((at, _)) = lane.merge {
            events.push((at, EV_MERGE, i));
        }
        if let Some((at, _, _)) = lane.end {
            events.push((at, EV_END, i));
        }
    }
    events.sort_unstable();
    events
}

/// Dry-run the event walk once to find each lane's widest turn-info label
/// (outcome glyph included) — needed before layout so box columns can be
/// allocated clear of every label a lane will ever emit. Resets each
/// lane's checkpoint state afterwards for the real walk.
fn compute_max_seg_widths(lanes: &mut [Lane], events: &[(i64, u8, usize)], now_ms: i64) {
    use unicode_width::UnicodeWidthStr;
    fn probe(lane: &mut Lane, delta: u64, start: i64, end: Option<i64>, extra: usize, now: i64) {
        if delta > 0 {
            let w = UnicodeWidthStr::width(segment_label(delta, start, end, now).as_str());
            lane.max_seg_w = lane.max_seg_w.max(w + extra);
        }
    }
    for &(_, kind, i) in events {
        match kind {
            EV_BOX => {
                let (Some(seq), Some(p)) = (lanes[i].fork_seq, lanes[i].parent) else {
                    continue;
                };
                let box_ms = lanes[i].box_ms;
                let d = seq.saturating_sub(lanes[p].cp_seq);
                let cp_ms = lanes[p].cp_ms;
                probe(&mut lanes[p], d, cp_ms, Some(box_ms), 0, now_ms);
                lanes[p].cp_seq = seq;
                lanes[p].cp_ms = box_ms;
            }
            EV_MERGE => {
                let (at, mseq) = lanes[i].merge.expect("merge event has merge data");
                if let Some(p) = lanes[i].parent {
                    let d = mseq.saturating_sub(lanes[p].cp_seq);
                    let cp_ms = lanes[p].cp_ms;
                    probe(&mut lanes[p], d, cp_ms, Some(at), 0, now_ms);
                    lanes[p].cp_seq = mseq;
                    lanes[p].cp_ms = at;
                }
                let df = lanes[i]
                    .summary
                    .map(|s| s.event_count.saturating_sub(lanes[i].cp_seq))
                    .unwrap_or(0);
                let cp_ms = lanes[i].cp_ms;
                let extra = if node_outcome_of(lanes[i].summary).is_some() {
                    2
                } else {
                    0
                };
                probe(&mut lanes[i], df, cp_ms, Some(at), extra, now_ms);
            }
            EV_END => {
                let (_, label_end, outcome) = lanes[i].end.expect("end event has end data");
                let d = lanes[i]
                    .summary
                    .map(|s| s.event_count.saturating_sub(lanes[i].cp_seq))
                    .unwrap_or(0);
                let cp_ms = lanes[i].cp_ms;
                let extra = if outcome.is_some() { 2 } else { 0 };
                probe(&mut lanes[i], d, cp_ms, label_end, extra, now_ms);
            }
            _ => unreachable!(),
        }
    }
    for lane in lanes.iter_mut() {
        lane.cp_seq = 0;
        lane.cp_ms = lane.box_ms;
    }
}

/// Paint one session box with its top-left at `(x, y)` and register its
/// label row as selectable.
fn draw_box(c: &mut Canvas, lane: &Lane, x: usize, y: usize) {
    use unicode_width::UnicodeWidthStr;
    let lw = UnicodeWidthStr::width(lane.label.as_str());
    let border = LineageSpan::Border {
        session_id: lane.node.session_id.clone(),
    };
    c.put(y, x, &format!("┌{}┐", "─".repeat(lw + 2)), &border);
    c.put(y + 1, x, "│ ", &border);
    c.put(
        y + 1,
        x + 2,
        &lane.label,
        &LineageSpan::Node {
            session_id: lane.node.session_id.clone(),
        },
    );
    c.put(y + 1, x + 2 + lw, " │", &border);
    c.put(y + 2, x, &format!("└{}┘", "─".repeat(lw + 2)), &border);
    c.node_rows.push((y + 1, lane.node.session_id.clone()));
}

/// Draw the diagram from one global, time-ordered event queue — every
/// box, merge arrow, and lane end lands at its position in event-time
/// order regardless of which lane it belongs to (see `flatten`'s "One
/// global timeline" notes).
fn layout_tree(
    c: &mut Canvas,
    root: &LineageNode,
    by_id: &HashMap<&str, &SessionSummary>,
    now_ms: i64,
) {
    use unicode_width::UnicodeWidthStr;

    let mut lanes: Vec<Lane> = Vec::new();
    collect_lanes(root, by_id, None, now_ms, &mut lanes);
    let events = build_events(&lanes);
    compute_max_seg_widths(&mut lanes, &events, now_ms);

    let mut cur = 0usize;
    let mut gi = 0usize;
    while gi < events.len() {
        let (t, kind, i) = events[gi];
        match kind {
            EV_BOX => {
                let Some(p) = lanes[i].parent else {
                    // The root's box tops the diagram; its lane state
                    // starts right below.
                    draw_box(c, &lanes[i], 1, cur);
                    lanes[i].lane_col = 2;
                    lanes[i].box_bottom = cur + 3;
                    lanes[i].last_row = cur + 2;
                    lanes[i].placed = true;
                    cur += 3;
                    gi += 1;
                    continue;
                };
                // A fork-out closes a window on the parent's lane;
                // subagent spawns just get a spacer row.
                cur += 1;
                if let Some(seq) = lanes[i].fork_seq {
                    let d = seq.saturating_sub(lanes[p].cp_seq);
                    if d > 0 {
                        put_segment(
                            c,
                            cur,
                            lanes[p].lane_col,
                            d,
                            lanes[p].cp_ms,
                            Some(lanes[i].box_ms),
                            now_ms,
                            None,
                        );
                        cur += 2;
                    }
                    lanes[p].cp_seq = seq;
                    lanes[p].cp_ms = lanes[i].box_ms;
                }
                let edge_word = match lanes[i].node.edge {
                    LineageEdge::Fork => "⑂ fork",
                    LineageEdge::Subagent => "▸ subagent",
                    LineageEdge::Root => "",
                };
                let ew = UnicodeWidthStr::width(edge_word);
                // Column: past the arrow's own minimum, and past every
                // live lane's widest turn-info reach (bullet + gap + label
                // + 2 blank columns) so nothing this box could share a row
                // with ever runs into it.
                let mut x = lanes[p].lane_col + ew + 6;
                for lane in lanes.iter().filter(|l| l.placed && !l.ended) {
                    x = x.max(lane.lane_col + lane.max_seg_w + 4);
                }
                draw_box(c, &lanes[i], x, cur);
                // Branch arrow into the box's label row, bridging over any
                // live lane it crosses (their bar is painted first; the
                // dashes fill only empty cells, so the lane visibly passes
                // in front of the shaft).
                let ay = cur + 1;
                let plane = lanes[p].lane_col;
                for lane in lanes.iter().filter(|l| l.placed && !l.ended) {
                    if lane.lane_col > plane && lane.lane_col < x && lane.box_bottom <= ay {
                        c.put_if_empty(ay, lane.lane_col, '│', &LineageSpan::Rail);
                    }
                }
                c.put(ay, plane, "├─", &LineageSpan::Rail);
                c.put(
                    ay,
                    plane + 3,
                    edge_word,
                    &LineageSpan::Edge(lanes[i].node.edge),
                );
                for dx in (plane + 3 + ew + 1)..x.saturating_sub(1) {
                    c.put_if_empty(ay, dx, '─', &LineageSpan::Rail);
                }
                c.put(ay, x - 1, "▸", &LineageSpan::Rail);
                lanes[p].last_row = lanes[p].last_row.max(ay);
                lanes[i].lane_col = x + 1;
                lanes[i].box_bottom = cur + 3;
                lanes[i].last_row = cur + 2;
                lanes[i].placed = true;
                cur += 3;
                gi += 1;
            }
            EV_MERGE => {
                let (at, mseq) = lanes[i].merge.expect("merge event has merge data");
                let p = lanes[i].parent.expect("a merging fork has a parent");
                let dp = mseq.saturating_sub(lanes[p].cp_seq);
                let df = lanes[i]
                    .summary
                    .map(|s| s.event_count.saturating_sub(lanes[i].cp_seq))
                    .unwrap_or(0);
                cur += 1;
                if dp > 0 || df > 0 {
                    // Both windows close at this same instant — they share
                    // one row, each on its own lane (the concept sketch's
                    // side-by-side "(turn info)" pair).
                    if dp > 0 {
                        put_segment(
                            c,
                            cur,
                            lanes[p].lane_col,
                            dp,
                            lanes[p].cp_ms,
                            Some(at),
                            now_ms,
                            None,
                        );
                    }
                    if df > 0 {
                        put_segment(
                            c,
                            cur,
                            lanes[i].lane_col,
                            df,
                            lanes[i].cp_ms,
                            Some(at),
                            now_ms,
                            node_outcome_of(lanes[i].summary),
                        );
                    }
                    cur += 2;
                }
                for n in std::mem::take(&mut lanes[i].more) {
                    c.put(cur, lanes[i].lane_col, "├─ ", &LineageSpan::Rail);
                    c.put(
                        cur,
                        lanes[i].lane_col + 3,
                        &format!("+{n} more"),
                        &LineageSpan::More(n),
                    );
                    cur += 1;
                }
                // Merge arrow flowing child → parent, bridging any live
                // lane strictly between them.
                let plane = lanes[p].lane_col;
                let flane = lanes[i].lane_col;
                for lane in lanes.iter().filter(|l| l.placed && !l.ended) {
                    if lane.lane_col > plane && lane.lane_col < flane && lane.box_bottom <= cur {
                        c.put_if_empty(cur, lane.lane_col, '│', &LineageSpan::Rail);
                    }
                }
                c.put(cur, plane, "│", &LineageSpan::Rail);
                let word = "◂─ ↩ merge ";
                c.put(cur, plane + 1, word, &LineageSpan::Rail);
                for dx in (plane + 1 + UnicodeWidthStr::width(word))..flane {
                    c.put_if_empty(cur, dx, '─', &LineageSpan::Rail);
                }
                c.put(cur, flane, "┘", &LineageSpan::Rail);
                lanes[p].cp_seq = mseq;
                lanes[p].cp_ms = at;
                lanes[p].last_row = lanes[p].last_row.max(cur);
                lanes[i].last_row = cur;
                lanes[i].ended = true;
                cur += 1;
                gi += 1;
            }
            EV_END => {
                // All lanes ending at the same instant share one turn-info
                // row (several live lanes all "end" at now).
                let mut group = vec![i];
                while gi + 1 < events.len() && events[gi + 1].0 == t && events[gi + 1].1 == EV_END {
                    gi += 1;
                    group.push(events[gi].2);
                }
                for &j in &group {
                    for n in std::mem::take(&mut lanes[j].more) {
                        c.put(cur, lanes[j].lane_col, "├─ ", &LineageSpan::Rail);
                        c.put(
                            cur,
                            lanes[j].lane_col + 3,
                            &format!("+{n} more"),
                            &LineageSpan::More(n),
                        );
                        lanes[j].last_row = lanes[j].last_row.max(cur);
                        cur += 1;
                    }
                }
                let closing: Vec<(usize, u64)> = group
                    .iter()
                    .filter_map(|&j| {
                        let d = lanes[j]
                            .summary
                            .map(|s| s.event_count.saturating_sub(lanes[j].cp_seq))
                            .unwrap_or(0);
                        (d > 0).then_some((j, d))
                    })
                    .collect();
                if !closing.is_empty() {
                    cur += 1;
                    for &(j, d) in &closing {
                        let (_, label_end, outcome) = lanes[j].end.expect("end event has end data");
                        put_segment(
                            c,
                            cur,
                            lanes[j].lane_col,
                            d,
                            lanes[j].cp_ms,
                            label_end,
                            now_ms,
                            outcome,
                        );
                        lanes[j].last_row = cur;
                    }
                    cur += 1;
                }
                for &j in &group {
                    lanes[j].ended = true;
                }
                gi += 1;
            }
            _ => unreachable!(),
        }
    }

    // End-fill: every lane's bar runs unbroken from its box down to its
    // last content row, through rows other lanes' events occupied
    // (put_if_empty leaves everything already painted alone). A lane with
    // nothing below its box has `last_row < box_bottom` — an empty range.
    for lane in &lanes {
        if !lane.placed {
            continue;
        }
        for yy in lane.box_bottom..=lane.last_row {
            c.put_if_empty(yy, lane.lane_col, '│', &LineageSpan::Rail);
        }
    }
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
                // Both windows close at the merge instant, so they share
                // one row side by side — the concept sketch's paired
                // "(turn info)  (turn info)".
                "  • 3 msgs · 3m20s    • 2 msgs · 3m20s".to_string(),
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
    fn terminal_state_lands_at_its_time_position_between_other_events() {
        // A lane reaching its terminal state is itself a timeline event: a
        // subagent that finished (Done at t=2000) BEFORE a later fork
        // branched (t=5000) must show its final ✓-marked turn info ABOVE
        // that fork's arrow — not grouped at the bottom of the diagram.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 10);
        let mut sub = with_event_count(with_created_at_ms(subagent_of(base("s"), "root"), 500), 3);
        sub.state = SessionState::Done;
        sub.last_event_at = Some(Utc.timestamp_millis_opt(2_000).unwrap());
        let fork = with_event_count(
            with_created_at_ms(forked_from_at(base("f"), "root", 6, 5_000), 5_000),
            2,
        );
        let sessions = vec![root, sub, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);

        // s finished (3 msgs, ✓) at t=2000; root's pre-fork window (6)
        // closes at f's fork-out (t=5000); root's trailing (4) and f's
        // life (2) close at "now".
        assert_eq!(segment_deltas(&rows), vec![3, 6, 4, 2]);
        let sub_done_row = rows
            .iter()
            .position(|r| {
                r.spans
                    .iter()
                    .any(|s| matches!(s.role, LineageSpan::SegmentOutcome { ok: true }))
            })
            .expect("s's ✓ row");
        let fork_arrow_row = rows
            .iter()
            .position(|r| r.text().contains("⑂ fork"))
            .expect("f's branch arrow row");
        assert!(
            sub_done_row < fork_arrow_row,
            "s went Done (t=2000) before f forked (t=5000), so its ✓ row \
             must render above the fork arrow:\n{}",
            diagram_text(&rows).join("\n")
        );
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

        // Segment order is chronological: pre-A (5), root while only A was
        // out (3 = seq 8 - seq 5, closed by B's fork-out), then A's
        // merge-back row where root's [8, 12) window (4) and A's own life
        // (4) close together side by side, then the final "now" row where
        // root's trailing (18 = 30 - 12) and B's life (2) close together.
        assert_eq!(segment_deltas(&rows), vec![5, 3, 4, 4, 18, 2]);
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
        // root: [0, 12) before the fork, then [12, 20) since — both lanes
        // are live, so both trailing windows close at "now" and share the
        // diagram's final row (root's lane left of f's).
        assert_eq!(segment_deltas(&rows), vec![12, 8, 2]);
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
                5, // root, while A was out ([5, 10)) — closes at A's merge…
                7, // …sharing that row with A's own whole life (row-major:
                // root's lane is left of A's)
                5, // root, between A merging back (seq 10) and B forking (seq 15)
                3, // B's own whole life — closes at its DISCARD time (t=5000),
                // which lands between B's fork-out and C's fork-out in
                // global event order
                5, // root, between B forking (seq 15, a discard doesn't move the
                // checkpoint past it) and C forking (seq 20)
                10, // root, since C forked (seq 20) to now (event_count 30)…
                2,  // …sharing the final "now" row with C's own whole life
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
                3, // root while a was out ([5, 8)) — b forks at the very
                // instant a merges, and branches sort before merges on a
                // time tie, so this window closes at b's BRANCH event
                // (seq 8, same boundary)
                4, // a's own life, closing at its merge arrow
                // no zero-length window at a's merge-back (seq 8 → seq 8)
                4, // root since the seq-8 boundary to now (event_count 12)…
                1, // …sharing the final "now" row with b's own life
            ]
        );
    }

    #[test]
    fn subagent_children_do_not_split_the_parent_timeline() {
        // A node with only subagent children (no forks) gets exactly one
        // window for its whole life — subagents don't stamp a
        // parent-timeline checkpoint the way forks do (spec 0014 has no
        // `transcript_seq`).
        let root = with_event_count(with_created_at_ms(base("root"), 0), 9);
        let sub = with_event_count(with_created_at_ms(subagent_of(base("s"), "root"), 500), 2);
        let sessions = vec![root, sub];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        assert_eq!(
            segment_deltas(&rows),
            vec![9, 2],
            "both lanes are live, so root's whole-life window (unsplit by \
             the subagent) and s's share the final \"now\" row — root's \
             lane is the left one"
        );
        // Both windows sit on the diagram's last row, and the branch arrow
        // must be labeled as a subagent edge, not a fork.
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
