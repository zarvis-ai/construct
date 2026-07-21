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

use construct_protocol::{ForkMergeMode, SessionKind, SessionState, SessionSummary, TokenTally};

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
    /// This node's `count` subagent children as a collapsible group
    /// (spec 0081): sessions spawn a LOT of (native) subagents, and the
    /// lineage a user manages by hand is the fork structure — so subagent
    /// children collapse into one "▸ N subagents · M running" toggle row
    /// by default, expanding (▾, children materialized after this marker)
    /// per parent on click. Only present when the tree was built with
    /// expansion tracking ([`build_tree_with_expansions`]).
    Subagents {
        count: usize,
        running: usize,
        expanded: bool,
    },
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
/// sidebar lineage section (spec 0081) on ordinary
/// sessions that have nothing to show — cheaper than [`build_tree`] since it
/// doesn't walk to the root or materialize the full tree, just answers
/// yes/no for `session_id` itself.
pub fn has_lineage(session_id: &str, sessions: &[SessionSummary]) -> bool {
    sessions.iter().any(|s| {
        if s.id == session_id {
            // Its own upward links count too: a subagent (or a fork) sits in
            // its parent's tree even when nothing points down at it yet.
            s.forked_from.is_some()
                || (matches!(s.kind, SessionKind::Subagent) && s.parent_session_id.is_some())
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
    build_tree_with_expansions(focus_id, sessions, None)
}

/// Like [`build_tree`], but with subagent-group expansion tracking
/// (spec 0081): `expanded` names the parents whose subagent children
/// materialize; every other node's subagent children collapse into a
/// [`LineageChild::Subagents`] toggle marker. `None` disables the feature
/// entirely (everything materializes, no markers — [`build_tree`]'s
/// behavior). The focus session's own ancestor chain is always expanded,
/// so the session the section is showing can never be hidden inside a
/// collapsed group.
pub fn build_tree_with_expansions(
    focus_id: &str,
    sessions: &[SessionSummary],
    expanded: Option<&HashSet<String>>,
) -> Option<LineageNode> {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    by_id.get(focus_id)?;
    let root_id = root_of(focus_id, &by_id);
    let effective: Option<HashSet<String>> = expanded.map(|set| {
        let mut e = set.clone();
        let mut cur = focus_id.to_string();
        let mut seen = HashSet::new();
        while seen.insert(cur.clone()) {
            let Some(s) = by_id.get(cur.as_str()) else {
                break;
            };
            if matches!(s.kind, SessionKind::Subagent) {
                if let Some(pid) = s.parent_session_id.as_deref() {
                    e.insert(pid.to_string());
                }
            }
            match parent_of(s).filter(|p| by_id.contains_key(p)) {
                Some(p) => cur = p.to_string(),
                None => break,
            }
        }
        e
    });
    let mut visited = HashSet::new();
    build_subtree(
        &root_id,
        &by_id,
        LineageEdge::Root,
        0,
        &mut visited,
        effective.as_ref(),
    )
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
    expanded: Option<&HashSet<String>>,
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
        // Subagent grouping (spec 0081): with expansion tracking on, this
        // node's subagent children sit behind a toggle marker — collapsed
        // (not materialized) unless the node is in the expanded set. Forks
        // always materialize: they're the structure the user built by hand.
        let sub_total = kids
            .iter()
            .filter(|(_, e)| matches!(e, LineageEdge::Subagent))
            .count();
        let sub_expanded = expanded.map_or(true, |set| set.contains(id));
        let mut out: Vec<LineageChild> = Vec::new();
        if expanded.is_some() && sub_total > 0 {
            let running = kids
                .iter()
                .filter(|(s, e)| {
                    matches!(e, LineageEdge::Subagent) && matches!(s.state, SessionState::Running)
                })
                .count();
            out.push(LineageChild::Subagents {
                count: sub_total,
                running,
                expanded: sub_expanded,
            });
        }
        let materialized: Vec<&(&SessionSummary, LineageEdge)> = kids
            .iter()
            .filter(|(_, e)| sub_expanded || !matches!(e, LineageEdge::Subagent))
            .collect();
        let m_total = materialized.len();
        out.extend(materialized.iter().take(MAX_SIBLINGS).filter_map(|(s, e)| {
            build_subtree(&s.id, by_id, *e, depth + 1, visited, expanded).map(LineageChild::Node)
        }));
        if m_total > MAX_SIBLINGS {
            out.push(LineageChild::More(m_total - MAX_SIBLINGS));
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
    /// The glyph labeling a branch arrow (`⑂` / `▸`) — tagged with the
    /// branching session so hover/selection lights it with the rest of
    /// that session's timeline.
    Edge {
        kind: LineageEdge,
        session_id: String,
    },
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
        /// Token consumption within this window (spec 0103) — the delta
        /// between the lane's boundary stamps. All-zero when untracked;
        /// the label then falls back to the message count, and hover has
        /// no token detail to show.
        tokens: TokenTally,
        /// Start of this window, epoch ms.
        start_ms: i64,
        /// End of this window, epoch ms; `None` = still open (measured
        /// against `now_ms` at flatten time).
        end_ms: Option<i64>,
        /// The lane this window belongs to.
        session_id: String,
    },
    /// The `•` bullet heading a mid-timeline turn-info line, sitting on
    /// the lane — tagged with the lane's session so hover/selection can
    /// light a session's whole timeline.
    SegmentBullet { session_id: String },
    /// Terminal-outcome glyph heading a lane's FINAL turn-info line in
    /// place of the bullet: `✓` when the lane ended well (fork merged,
    /// session `Done`), `✗` when it dead-ended (fork discarded, session
    /// `Errored`).
    SegmentOutcome { ok: bool, session_id: String },
    /// A node's status glyph — the ONLY label part styled by live session
    /// state, mirroring the session list (name text stays the default
    /// text color; only the check mark goes blue when Done, etc.).
    NodeStatus { session_id: String },
    /// A node's box label text (name, harness, terminal marker) — default
    /// text color; carries the session id for selection/hover.
    Node { session_id: String },
    /// "+N more" collapse marker.
    More(usize),
    /// The collapsed/expanded subagent-group toggle row for `session_id`'s
    /// node: "▸ N subagents · M running" / "▾ N subagents". Click toggles
    /// the group.
    SubagentsToggle { session_id: String, expanded: bool },
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

/// One session box's bounds within the diagram, in canvas (row/column)
/// coordinates — the renderer maps these to screen cells for mouse
/// hit-testing (hover highlights the border, click jumps to the session).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineageBoxBounds {
    pub session_id: String,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Maximum box interior width, in columns — longer labels wrap onto
/// additional box rows.
pub const MAX_BOX_CONTENT_W: usize = 28;
/// Maximum wrapped label rows per box — content past this is ellipsized.
pub const MAX_BOX_LINES: usize = 2;

/// How the lineage section draws the tree — toggled from the section's
/// top border.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineageViewMode {
    /// Boxed-lane diagram: each session a bordered box with its own
    /// vertical timeline lane below it (the concept-sketch layout).
    #[default]
    Boxes,
    /// git-graph-style compact rails: one 2-column rail per session,
    /// events as one-line entries with connectors curving between rails,
    /// all text in a single left-aligned column right of the rails.
    Rails,
}

impl LineageViewMode {
    pub fn toggled(self) -> Self {
        match self {
            LineageViewMode::Boxes => LineageViewMode::Rails,
            LineageViewMode::Rails => LineageViewMode::Boxes,
        }
    }

    /// Short label for the top-border toggle button.
    pub fn label(self) -> &'static str {
        match self {
            LineageViewMode::Boxes => "lineage",
            LineageViewMode::Rails => "lineage (compact)",
        }
    }

    /// A one-word name for this mode — used where the surrounding UI
    /// already says "lineage" (the sidebar section header), so the full
    /// [`Self::label`] would be redundant.
    pub fn short_label(self) -> &'static str {
        match self {
            LineageViewMode::Boxes => "full",
            LineageViewMode::Rails => "compact",
        }
    }
}

impl LineageSpan {
    /// The session a span belongs to, when it has one — plain rail filler
    /// and `+N more` markers don't. Drives hover/click hit-testing: any
    /// owned cell (box border, lane bar, branch glyph, turn-info text)
    /// highlights and jumps to its session.
    pub fn owner(&self) -> Option<&str> {
        match self {
            // The subagents toggle is deliberately owner-less: clicking it
            // must toggle the group, never jump to the parent session.
            LineageSpan::Rail | LineageSpan::More(_) | LineageSpan::SubagentsToggle { .. } => None,
            LineageSpan::Border { session_id }
            | LineageSpan::Edge { session_id, .. }
            | LineageSpan::Segment { session_id, .. }
            | LineageSpan::SegmentBullet { session_id }
            | LineageSpan::SegmentOutcome { session_id, .. }
            | LineageSpan::NodeStatus { session_id }
            | LineageSpan::Node { session_id } => Some(session_id),
        }
    }
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
    /// Every box's bounds, for mouse hit-testing.
    boxes: Vec<LineageBoxBounds>,
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
/// the lineage section's rendering (`ui.rs::render_lineage_section`, to
/// highlight the selected row) and its keyboard navigation
/// (`app/lineage_section.rs`, to move/clamp the selection) share one
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
/// ON the lanes, a marker sitting where the bar would be with the text to
/// its right: `•` mid-timeline, and on each lane's FINAL window a
/// terminal-outcome glyph instead — `✓` (merged / Done) or `✗`
/// (discarded / Errored). Rows pack tight: no blank spacer rows.
///
/// ```text
/// ┌───────────────────────────┐
/// │ ● auth-refactor (claude)  │
/// └───────────────────────────┘
///  • 12 msgs · 8m12s
///  │                   ┌───────────────────┐
///  ├─ ⑂ fork ─────────▸│ ● idea A (claude) │
///  │                   └───────────────────┘
///  • 5 msgs · 3m40s     ✓ 2 msgs · 1m05s
///  │◂─ ↩ merge ─────────┘
///  • 3 msgs · 2m00s
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
/// like the concept sketch.
///
/// Lane columns nest by CLOSE time (`assign_offsets`): overlapping
/// sibling lanes stack outward with the later-terminating one further
/// out, so an inner lane's merge/end never crosses an outer lane that's
/// still running; sequential siblings reuse the inner slot. A lane stays
/// live until its closing row — arrows crossing a live lane break around
/// its bar.
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
    flatten_with_boxes(root, sessions, now_ms).0
}

/// [`flatten`], plus every box's canvas bounds for mouse hit-testing.
pub fn flatten_with_boxes(
    root: &LineageNode,
    sessions: &[SessionSummary],
    now_ms: i64,
) -> (Vec<LineageRow>, Vec<LineageBoxBounds>) {
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut canvas = Canvas::default();
    layout_tree(&mut canvas, root, &by_id, now_ms);
    let boxes = std::mem::take(&mut canvas.boxes);
    (canvas.into_rows(), boxes)
}

/// Whether `lane`'s incoming edge is a fork synthesized automatically by a
/// harness-native context reset (`ForkedFrom::is_reset_snapshot`, spec
/// 0085) rather than a user picking a harness and forking on purpose.
/// `false` for anything without a `SessionSummary` (deleted mid-render) or
/// without `forked_from` at all (root/subagent edges).
fn is_reset_snapshot_edge(lane: &Lane) -> bool {
    lane.summary
        .and_then(|s| s.forked_from.as_ref())
        .is_some_and(|f| f.is_reset_snapshot)
}

/// `"● name (harness)"` box text. The name is the session's full title
/// when it has one; otherwise just the harness stands alone. A merged
/// fork gets NO marker here — the merge arrow and the `✓` on its final
/// turn-info line already carry that outcome. A discarded fork keeps its
/// `✗ discarded` marker since a discard draws no arrow.
fn node_box_label(summary: Option<&SessionSummary>, session_id: &str) -> String {
    let Some(s) = summary else {
        let short: String = session_id.chars().take(8).collect();
        return format!("{short} (gone)");
    };
    let status = status_glyph(s.state);
    let title = s.title.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let mut label = match title {
        Some(t) => format!("{status} {t} ({})", s.harness),
        None => format!("{status} {}", s.harness),
    };
    if ForkStatus::of(s) == ForkStatus::Discarded {
        label.push_str("  ✗ discarded");
    }
    label
}

/// Greedy word-wrap of a box label to [`MAX_BOX_CONTENT_W`] columns and at
/// most [`MAX_BOX_LINES`] lines; content past the last line is ellipsized.
/// A word wider than a whole line hard-breaks.
fn wrap_box_label(label: &str) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    let mut truncated = false;
    'words: for word in label.split_whitespace() {
        let ww = UnicodeWidthStr::width(word);
        let sep = usize::from(!cur.is_empty());
        if cur_w + sep + ww <= MAX_BOX_CONTENT_W {
            if sep == 1 {
                cur.push(' ');
            }
            cur.push_str(word);
            cur_w += sep + ww;
            continue;
        }
        // Word doesn't fit on the current line.
        if !cur.is_empty() {
            if lines.len() + 1 == MAX_BOX_LINES {
                truncated = true;
                break 'words;
            }
            lines.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if ww <= MAX_BOX_CONTENT_W {
            cur.push_str(word);
            cur_w = ww;
            continue;
        }
        // Hard-break an overlong word.
        for ch in word.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            if cur_w + cw > MAX_BOX_CONTENT_W {
                if lines.len() + 1 == MAX_BOX_LINES {
                    truncated = true;
                    break 'words;
                }
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            cur.push(ch);
            cur_w += cw;
        }
    }
    if !cur.is_empty() || lines.is_empty() {
        lines.push(cur);
    }
    if truncated {
        // Make room for the ellipsis on the final kept line.
        let last = lines.last_mut().expect("at least one line");
        while UnicodeWidthStr::width(last.as_str()) + 1 > MAX_BOX_CONTENT_W {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// Paint one turn-info line: a marker ON the lane — `•` mid-timeline, or
/// `✓`/`✗` when `outcome` is set (this is the lane's FINAL window and its
/// terminal event was a merge/completion vs a discard/error) — with the
/// info text two columns right of it. The window's numbers ride along on
/// the Segment span role for tests. Zero-message windows are the caller's
/// job to skip. Returns one past the rightmost column painted.
#[allow(clippy::too_many_arguments)]
fn put_segment(
    c: &mut Canvas,
    y: usize,
    lane: usize,
    delta_events: u64,
    tokens: TokenTally,
    start_ms: i64,
    end_ms: Option<i64>,
    now_ms: i64,
    outcome: Option<bool>,
    owner: &str,
    busy_ms: Option<u64>,
) -> usize {
    use unicode_width::UnicodeWidthStr;
    match outcome {
        Some(ok) => c.put(
            y,
            lane,
            if ok { "✓" } else { "✗" },
            &LineageSpan::SegmentOutcome {
                ok,
                session_id: owner.to_string(),
            },
        ),
        None => c.put(
            y,
            lane,
            "•",
            &LineageSpan::SegmentBullet {
                session_id: owner.to_string(),
            },
        ),
    }
    // Compute time when the daemon has tracked it (the sum of the turns'
    // Running spans in this window); wall-clock span as the legacy
    // fallback for records without busy data.
    let text = match busy_ms {
        Some(b) => segment_label_busy(delta_events, tokens, b),
        None => segment_label(delta_events, tokens, start_ms, end_ms, now_ms),
    };
    let w = UnicodeWidthStr::width(text.as_str());
    c.put(
        y,
        lane + 2,
        &text,
        &LineageSpan::Segment {
            delta_events,
            tokens,
            start_ms,
            end_ms,
            session_id: owner.to_string(),
        },
    );
    lane + 2 + w
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
    /// `forked_from.parent_busy_ms` — the parent's compute time at
    /// fork-out (0 on records predating the field).
    fork_busy: u64,
    /// The parent's `message_count` at fork time (`ForkedFrom::
    /// parent_message_count`); 0 when untracked (legacy records).
    fork_msgs: u64,
    /// The parent's token tally at fork time (`ForkedFrom::parent_tokens`);
    /// all-zero when untracked (legacy records).
    fork_tokens: TokenTally,
    /// `(at_ms, merged_seq, merged_busy_ms, merged_message_count,
    /// merged_tokens)` for forks that merged back (`Result`) — such a lane
    /// ends at its merge arrow instead of an `End` event.
    merge: Option<(i64, u64, u64, u64, TokenTally)>,
    /// `(at_ms, label end, outcome)` for every other lane's end: a
    /// discarded fork (at discard time), a session that went Done/Errored
    /// (at `last_event_at`), or a live session (at "now", open-ended).
    end: Option<(i64, Option<i64>, Option<bool>)>,
    /// When this lane's terminal event lands: its merge time, or its end
    /// time — the interval `[box_ms, close_ms]` is what column nesting
    /// orders by (later-closing lanes sit further out).
    close_ms: i64,
    /// Collapsed "+N more" markers among this lane's children.
    more: Vec<usize>,
    /// This node's subagent-group toggle marker `(count, running,
    /// expanded)`, rendered as its own row right under the node.
    subagent_marker: Option<(usize, usize, bool)>,
    /// Wrapped box label lines (see `wrap_box_label`).
    label_lines: Vec<String>,
    /// This lane's own total compute time as of "now" (frozen naturally
    /// for terminal sessions — busy stops accumulating on the terminal
    /// transition). `0` means no busy data (legacy records): windows fall
    /// back to wall-clock spans.
    busy_total: u64,
    /// The session's lifetime chat-message tally (`SessionSummary::
    /// message_count`); 0 when untracked (legacy records).
    msgs_total: u64,
    /// The session's lifetime token tally (`SessionSummary::tokens`);
    /// all-zero when untracked (legacy records).
    tokens_total: TokenTally,
    // Running state, filled in as the global walk proceeds.
    cp_seq: u64,
    cp_ms: i64,
    cp_busy: u64,
    /// Message-count checkpoint mirroring `cp_seq`.
    cp_msgs: u64,
    /// Token-tally checkpoint mirroring `cp_seq`.
    cp_tokens: TokenTally,
    x_abs: usize,
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
            .map(|m| {
                (
                    m.at_ms.max(box_ms),
                    m.merged_seq,
                    m.merged_busy_ms,
                    m.merged_message_count,
                    m.merged_tokens,
                )
            })
    } else {
        None
    };
    let end = if merge.is_some() {
        None
    } else if let Some(m) = merge_rec.filter(|m| m.mode == ForkMergeMode::Discard) {
        // A discarded fork's lane dead-ends at the discard, its final
        // window frozen there and marked `✗`.
        let t = m.at_ms.max(box_ms);
        Some((t, Some(m.at_ms), Some(false)))
    } else if let Some(ok) = node_outcome_of(summary) {
        // Session reached a terminal state: the lane ends when its last
        // event landed, and the final window carries ✓/✗. Without a
        // recorded last event (native subagents can exit without one),
        // fall back to the session's own start — a closed session's lane
        // must never keep running toward "now".
        let t = summary
            .and_then(|s| s.last_event_at.map(|d| d.timestamp_millis()))
            .unwrap_or(box_ms)
            .max(box_ms);
        Some((t, Some(t), Some(ok)))
    } else {
        // Live: the lane's row position runs to "now", but its final
        // window's elapsed measures only up to the session's last
        // activity (`last_event_at`) — execution time for the turns, not
        // idle wall-clock ticking toward now.
        let label_end = summary.and_then(|s| s.last_event_at.map(|d| d.timestamp_millis()));
        Some((now_ms.max(box_ms), label_end, None))
    };
    let close_ms = merge
        .map(|(at, _, _, _, _)| at)
        .or(end.map(|(at, _, _)| at))
        .unwrap();
    let idx = lanes.len();
    lanes.push(Lane {
        node,
        summary,
        parent,
        box_ms,
        fork_seq: forked.map(|f| f.transcript_seq),
        fork_busy: forked.map(|f| f.parent_busy_ms).unwrap_or(0),
        fork_msgs: forked.map(|f| f.parent_message_count).unwrap_or(0),
        fork_tokens: forked.map(|f| f.parent_tokens).unwrap_or_default(),
        merge,
        end,
        close_ms,
        more: Vec::new(),
        subagent_marker: None,
        label_lines: wrap_box_label(&node_box_label(summary, &node.session_id)),
        busy_total: summary.map(|s| s.busy_ms_at(now_ms)).unwrap_or(0),
        msgs_total: summary.map(|s| s.message_count).unwrap_or(0),
        tokens_total: summary.map(|s| s.tokens).unwrap_or_default(),
        cp_seq: 0,
        cp_ms: box_ms,
        cp_busy: 0,
        cp_msgs: 0,
        cp_tokens: TokenTally::default(),
        x_abs: 0,
        lane_col: 0,
        box_bottom: 0,
        last_row: 0,
        placed: false,
        ended: false,
    });
    for child in &node.children {
        match child {
            LineageChild::More(n) => lanes[idx].more.push(*n),
            LineageChild::Subagents {
                count,
                running,
                expanded,
            } => {
                lanes[idx].subagent_marker = Some((*count, *running, *expanded));
            }
            LineageChild::Node(cn) => {
                collect_lanes(cn, by_id, Some(idx), now_ms, lanes);
            }
        }
    }
    idx
}

/// The subagent-group toggle row's text: `▸ N subagents · M running`
/// collapsed (running part only when M > 0), `▾ N subagents` expanded.
fn subagent_marker_text(count: usize, running: usize, expanded: bool) -> String {
    let noun = if count == 1 { "subagent" } else { "subagents" };
    if expanded {
        format!("▾ {count} {noun}")
    } else if running > 0 {
        format!("▸ {count} {noun} · {running} running")
    } else {
        format!("▸ {count} {noun}")
    }
}

/// Boxes mode: emit the lane's subagent-group toggle row (if any) at `y`,
/// directly under its box — `├─ ▸ N subagents · M running` on the lane.
/// Returns the next free row.
fn put_subagent_marker_box(c: &mut Canvas, y: usize, lane: &mut Lane) -> usize {
    let Some((count, running, expanded)) = lane.subagent_marker else {
        return y;
    };
    // The connector is part of the parent's lane — it lights up with it.
    c.put(
        y,
        lane.lane_col,
        "├─ ",
        &LineageSpan::Border {
            session_id: lane.node.session_id.clone(),
        },
    );
    c.put(
        y,
        lane.lane_col + 3,
        &subagent_marker_text(count, running, expanded),
        &LineageSpan::SubagentsToggle {
            session_id: lane.node.session_id.clone(),
            expanded,
        },
    );
    lane.last_row = lane.last_row.max(y);
    y + 1
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
        if let Some((at, _, _, _, _)) = lane.merge {
            events.push((at, EV_MERGE, i));
        }
        if let Some((at, _, _)) = lane.end {
            events.push((at, EV_END, i));
        }
    }
    events.sort_unstable();
    events
}

/// Interior (content) width of a lane's box: its widest wrapped label line.
fn box_content_w(lane: &Lane) -> usize {
    use unicode_width::UnicodeWidthStr;
    lane.label_lines
        .iter()
        .map(|l| UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0)
}

/// Total box height: wrapped label lines plus the two border rows.
fn box_rows(lane: &Lane) -> usize {
    lane.label_lines.len() + 2
}

/// Paint one session box with its top-left at `(x, y)`, register its
/// FIRST label row as the selectable anchor, and record its bounds for
/// mouse hit-testing. Label lines are padded to the interior width so the
/// whole rectangle interior belongs to the Node span (selection paints it
/// uniformly).
fn draw_box(c: &mut Canvas, lane: &Lane, x: usize, y: usize) {
    use unicode_width::UnicodeWidthStr;
    let lw = box_content_w(lane);
    let border = LineageSpan::Border {
        session_id: lane.node.session_id.clone(),
    };
    c.put(y, x, &format!("┌{}┐", "─".repeat(lw + 2)), &border);
    let status = lane.summary.map(|s| status_glyph(s.state));
    for (li, line) in lane.label_lines.iter().enumerate() {
        let pad = lw - UnicodeWidthStr::width(line.as_str());
        c.put(y + 1 + li, x, "│ ", &border);
        let mut tx = x + 2;
        let mut rest: &str = line;
        // The status glyph opens the first label line — split it into its
        // own state-colored span, like the session list's status column.
        if li == 0 {
            if let Some(g) = status {
                if let Some(stripped) = line.strip_prefix(g) {
                    c.put(
                        y + 1,
                        tx,
                        g,
                        &LineageSpan::NodeStatus {
                            session_id: lane.node.session_id.clone(),
                        },
                    );
                    tx += UnicodeWidthStr::width(g);
                    rest = stripped;
                }
            }
        }
        c.put(
            y + 1 + li,
            tx,
            &format!("{rest}{}", " ".repeat(pad)),
            &LineageSpan::Node {
                session_id: lane.node.session_id.clone(),
            },
        );
        c.put(y + 1 + li, x + 2 + lw, " │", &border);
    }
    let h = box_rows(lane);
    c.put(y + h - 1, x, &format!("└{}┘", "─".repeat(lw + 2)), &border);
    c.node_rows.push((y + 1, lane.node.session_id.clone()));
    c.boxes.push(LineageBoxBounds {
        session_id: lane.node.session_id.clone(),
        x,
        y,
        width: lw + 4,
        height: h,
    });
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
    let _root_idx = collect_lanes(root, by_id, None, now_ms, &mut lanes);
    let events = build_events(&lanes);

    let mut cur = 0usize;
    let mut gi = 0usize;
    while gi < events.len() {
        let (t, kind, i) = events[gi];
        match kind {
            EV_BOX => {
                let h = box_rows(&lanes[i]);
                let Some(p) = lanes[i].parent else {
                    // The root's box tops the diagram; its lane state
                    // starts right below.
                    draw_box(c, &lanes[i], 1, cur);
                    lanes[i].x_abs = 1;
                    lanes[i].lane_col = 3;
                    lanes[i].box_bottom = cur + h;
                    lanes[i].last_row = cur + h - 1;
                    lanes[i].placed = true;
                    cur += h;
                    cur = put_subagent_marker_box(c, cur, &mut lanes[i]);
                    gi += 1;
                    continue;
                };
                // A fork-out closes a window on the parent's lane. Every
                // turn-info line gets a lane-bar row above it (the row is
                // left blank here; the end-fill draws every live lane's
                // bar through it).
                if let Some(seq) = lanes[i].fork_seq {
                    let d = seq.saturating_sub(lanes[p].cp_seq);
                    if d > 0 {
                        cur += 1;
                        let busy = (lanes[i].fork_busy > 0)
                            .then(|| lanes[i].fork_busy.saturating_sub(lanes[p].cp_busy));
                        let shown = if lanes[i].fork_msgs > 0 {
                            lanes[i].fork_msgs.saturating_sub(lanes[p].cp_msgs)
                        } else {
                            d
                        };
                        let tokens = lanes[i].fork_tokens.saturating_sub(&lanes[p].cp_tokens);
                        put_segment(
                            c,
                            cur,
                            lanes[p].lane_col,
                            shown,
                            tokens,
                            lanes[p].cp_ms,
                            Some(lanes[i].box_ms),
                            now_ms,
                            None,
                            &lanes[p].node.session_id,
                            busy,
                        );
                        cur += 1;
                    }
                    lanes[p].cp_seq = seq;
                    lanes[p].cp_ms = lanes[i].box_ms;
                    lanes[p].cp_busy = lanes[p].cp_busy.max(lanes[i].fork_busy);
                    lanes[p].cp_msgs = lanes[p].cp_msgs.max(lanes[i].fork_msgs);
                    lanes[p].cp_tokens = lanes[p].cp_tokens.max(&lanes[i].fork_tokens);
                }
                // Icon-only arrow label — the glyph alone (⑂ / ▸ / ↺) marks
                // the edge kind. A fork synthesized automatically by a
                // context reset (spec 0085) gets a distinct glyph so it's
                // never confused with a fork the user made on purpose,
                // checked before the edge-kind match rather than as a
                // separate `LineageEdge` variant — it IS an ordinary fork
                // edge, just one `ForkedFrom` marks as auto-created.
                let edge_word = if is_reset_snapshot_edge(&lanes[i]) {
                    "↺"
                } else {
                    match lanes[i].node.edge {
                        LineageEdge::Fork => "⑂",
                        LineageEdge::Subagent => "▸",
                        LineageEdge::Root => "",
                    }
                };
                let ew = UnicodeWidthStr::width(edge_word);
                // Minimal-x placement: boxes only need THEIR OWN rows
                // free (every event gets fresh rows, so box rects never
                // collide), and a lane's bar simply gaps behind anything
                // painted over its column later. The only hard constraints
                // are the branch arrow's minimum reach from the parent's
                // lane and lane-column uniqueness among lanes whose
                // lifetimes overlap (two bars sharing a column would be
                // unreadable). Everything else — vertical lines crossing
                // under boxes and turn info — is allowed, which is what
                // keeps the diagram narrow.
                let (this_lo, this_hi) = (lanes[i].box_ms, lanes[i].close_ms);
                let bw = box_content_w(&lanes[i]) + 4;
                let mut x = lanes[p].lane_col + 7;
                // The lane may hang anywhere under the box's interior:
                // pick the leftmost free column at least SPREAD_APART
                // columns away from every lifetime-overlapping lane (or
                // the best separation available), so concurrent vertical
                // lines don't run side by side — without widening the
                // diagram, since the box's own span bounds the choice.
                const SPREAD_APART: usize = 6;
                let lane_pick = loop {
                    let lo = x + 2;
                    let hi = (x + bw).saturating_sub(2).max(lo);
                    let occupied: Vec<usize> = lanes
                        .iter()
                        .enumerate()
                        .filter(|(j, l)| {
                            *j != i && l.placed && l.box_ms <= this_hi && this_lo <= l.close_ms
                        })
                        .map(|(_, l)| l.lane_col)
                        .collect();
                    let best = (lo..=hi)
                        .filter(|cand| !occupied.contains(cand))
                        .max_by_key(|cand| {
                            let d = occupied
                                .iter()
                                .map(|o| o.abs_diff(*cand))
                                .min()
                                .unwrap_or(usize::MAX);
                            (d.min(SPREAD_APART), std::cmp::Reverse(*cand))
                        });
                    if let Some(cand) = best {
                        break cand;
                    }
                    // Every column under the box is taken by a concurrent
                    // lane — shift the box right and retry.
                    x += 2;
                };
                // If this box would sit directly on a live lane's very
                // FIRST bar row (covering its whole visible start), give
                // that lane one row of air so its timeline visibly begins
                // under its own box before disappearing behind this one.
                if lanes.iter().enumerate().any(|(j, l)| {
                    j != i
                        && l.placed
                        && !l.ended
                        && l.lane_col >= x
                        && l.lane_col < x + bw
                        && l.box_bottom >= cur
                }) {
                    cur += 1;
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
                        let sid = lane.node.session_id.clone();
                        c.put_if_empty(
                            ay,
                            lane.lane_col,
                            '│',
                            &LineageSpan::Border { session_id: sid },
                        );
                    }
                }
                let child_border = LineageSpan::Border {
                    session_id: lanes[i].node.session_id.clone(),
                };
                let parent_border = LineageSpan::Border {
                    session_id: lanes[p].node.session_id.clone(),
                };
                c.put(ay, plane, "├", &parent_border);
                c.put(ay, plane + 1, "─", &child_border);
                c.put(
                    ay,
                    plane + 3,
                    edge_word,
                    &LineageSpan::Edge {
                        kind: lanes[i].node.edge,
                        session_id: lanes[i].node.session_id.clone(),
                    },
                );
                for dx in (plane + 4 + ew)..x.saturating_sub(1) {
                    c.put_if_empty(ay, dx, '─', &child_border);
                }
                c.put(ay, x - 1, "▸", &child_border);
                lanes[p].last_row = lanes[p].last_row.max(ay);
                lanes[i].x_abs = x;
                lanes[i].lane_col = lane_pick;
                lanes[i].box_bottom = cur + h;
                lanes[i].last_row = cur + h - 1;
                lanes[i].placed = true;
                cur += h;
                cur = put_subagent_marker_box(c, cur, &mut lanes[i]);
                gi += 1;
            }
            EV_MERGE => {
                let (at, mseq, mbusy, mmsgs, mtokens) =
                    lanes[i].merge.expect("merge event has merge data");
                let p = lanes[i].parent.expect("a merging fork has a parent");
                let dp = mseq.saturating_sub(lanes[p].cp_seq);
                let df = lanes[i]
                    .summary
                    .map(|s| s.event_count.saturating_sub(lanes[i].cp_seq))
                    .unwrap_or(0);
                if dp > 0 || df > 0 {
                    // Lane-bar row above the turn info (filled later).
                    cur += 1;
                    // Both windows close at this same instant — they share
                    // one row, each on its own lane (the concept sketch's
                    // side-by-side "(turn info)" pair). Merging IS the
                    // fork's successful completion: its final window leads
                    // with ✓. With the tight column packing, the fork's
                    // lane may sit inside the parent's text — stagger onto
                    // the next row instead of colliding.
                    let mut row = cur;
                    if dp > 0 {
                        let busy = (mbusy > 0).then(|| mbusy.saturating_sub(lanes[p].cp_busy));
                        let shown = if mmsgs > 0 {
                            mmsgs.saturating_sub(lanes[p].cp_msgs)
                        } else {
                            dp
                        };
                        let tokens = mtokens.saturating_sub(&lanes[p].cp_tokens);
                        let end_x = put_segment(
                            c,
                            row,
                            lanes[p].lane_col,
                            shown,
                            tokens,
                            lanes[p].cp_ms,
                            Some(at),
                            now_ms,
                            None,
                            &lanes[p].node.session_id,
                            busy,
                        );
                        if df > 0 && lanes[i].lane_col < end_x + 2 {
                            row += 1;
                        }
                    }
                    if df > 0 {
                        let busy = (lanes[i].busy_total > 0)
                            .then(|| lanes[i].busy_total.saturating_sub(lanes[i].cp_busy));
                        let shown = if lanes[i].msgs_total > 0 {
                            lanes[i].msgs_total.saturating_sub(lanes[i].cp_msgs)
                        } else {
                            df
                        };
                        let tokens = lanes[i].tokens_total.saturating_sub(&lanes[i].cp_tokens);
                        put_segment(
                            c,
                            row,
                            lanes[i].lane_col,
                            shown,
                            tokens,
                            lanes[i].cp_ms,
                            Some(at),
                            now_ms,
                            Some(true),
                            &lanes[i].node.session_id,
                            busy,
                        );
                    }
                    cur = row + 1;
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
                        let sid = lane.node.session_id.clone();
                        c.put_if_empty(
                            cur,
                            lane.lane_col,
                            '│',
                            &LineageSpan::Border { session_id: sid },
                        );
                    }
                }
                let fork_border = LineageSpan::Border {
                    session_id: lanes[i].node.session_id.clone(),
                };
                let parent_border = LineageSpan::Border {
                    session_id: lanes[p].node.session_id.clone(),
                };
                c.put(cur, plane, "│", &parent_border);
                // Icon-only merge arrow: ◂─ ↩ ──…──┘
                c.put(cur, plane + 1, "◂─", &fork_border);
                c.put(cur, plane + 4, "↩", &fork_border);
                for dx in (plane + 6)..flane {
                    c.put_if_empty(cur, dx, '─', &fork_border);
                }
                c.put(cur, flane, "┘", &fork_border);
                lanes[p].cp_seq = mseq;
                lanes[p].cp_ms = at;
                lanes[p].cp_busy = lanes[p].cp_busy.max(mbusy);
                lanes[p].cp_msgs = lanes[p].cp_msgs.max(mmsgs);
                lanes[p].cp_tokens = lanes[p].cp_tokens.max(&mtokens);
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
                    // Lane-bar row above the final turn info; nothing
                    // below it — the lane ends here. Infos pack onto one
                    // row left-to-right, staggering to the next row when a
                    // lane's marker would land inside the previous text.
                    cur += 1;
                    let mut order = closing.clone();
                    order.sort_by_key(|&(j, _)| lanes[j].lane_col);
                    let mut row_end_x = 0usize;
                    for &(j, d) in &order {
                        let (_, label_end, outcome) = lanes[j].end.expect("end event has end data");
                        if row_end_x > 0 && lanes[j].lane_col < row_end_x + 2 {
                            cur += 1;
                            row_end_x = 0;
                        }
                        let owner = lanes[j].node.session_id.clone();
                        let busy = (lanes[j].busy_total > 0)
                            .then(|| lanes[j].busy_total.saturating_sub(lanes[j].cp_busy));
                        let shown = if lanes[j].msgs_total > 0 {
                            lanes[j].msgs_total.saturating_sub(lanes[j].cp_msgs)
                        } else {
                            d
                        };
                        let tokens = lanes[j].tokens_total.saturating_sub(&lanes[j].cp_tokens);
                        row_end_x = put_segment(
                            c,
                            cur,
                            lanes[j].lane_col,
                            shown,
                            tokens,
                            lanes[j].cp_ms,
                            label_end,
                            now_ms,
                            outcome,
                            &owner,
                            busy,
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
        let border = LineageSpan::Border {
            session_id: lane.node.session_id.clone(),
        };
        for yy in lane.box_bottom..=lane.last_row {
            c.put_if_empty(yy, lane.lane_col, '│', &border);
        }
    }
}

/// [`flatten_with_boxes`]'s git-graph-style sibling: one 2-column rail per
/// session (columns reused once a lane closes), events as one-line entries
/// in the same global time order the boxed layout uses, with connectors
/// curving between rails and all text in one left-aligned column right of
/// the rails. The returned "box" bounds are each session's label-row rect,
/// so hover/click hit-testing works identically in both modes.
pub fn flatten_rails(
    root: &LineageNode,
    sessions: &[SessionSummary],
    now_ms: i64,
) -> (Vec<LineageRow>, Vec<LineageBoxBounds>) {
    use unicode_width::UnicodeWidthStr;
    let by_id: HashMap<&str, &SessionSummary> =
        sessions.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut lanes: Vec<Lane> = Vec::new();
    let root_idx = collect_lanes(root, &by_id, None, now_ms, &mut lanes);
    let events = build_events(&lanes);

    // Rail assignment: root owns rail 0 forever; every other lane takes
    // the lowest rail whose previous occupant closed before it branched
    // (classic git-graph column reuse). `rail_close[r]` tracks when rail
    // r frees up.
    let mut rail_of: Vec<usize> = vec![0; lanes.len()];
    let mut rail_close: Vec<i64> = vec![i64::MAX];
    let mut branch_order: Vec<usize> = (0..lanes.len()).filter(|&i| i != root_idx).collect();
    branch_order.sort_by_key(|&i| (lanes[i].box_ms, i));
    for &i in &branch_order {
        let rail = (1..rail_close.len())
            .find(|&r| rail_close[r] < lanes[i].box_ms)
            .unwrap_or(rail_close.len());
        if rail == rail_close.len() {
            rail_close.push(lanes[i].close_ms);
        } else {
            rail_close[rail] = lanes[i].close_ms;
        }
        rail_of[i] = rail;
    }
    let nrails = rail_close.len();
    let col = |r: usize| 1 + 2 * r;
    let text_x = 1 + 2 * nrails + 1;

    let mut c = Canvas::default();
    // Per-lane row extents for the end-fill: (first row below its entry
    // row, last row its rail reaches).
    let mut first_row: Vec<usize> = vec![0; lanes.len()];
    let mut last_row: Vec<usize> = vec![0; lanes.len()];
    let mut ended: Vec<bool> = vec![false; lanes.len()];
    let mut placed: Vec<bool> = vec![false; lanes.len()];

    let put_label = |c: &mut Canvas, row: usize, lane: &Lane, edge_prefix: Option<&str>| {
        let mut x = text_x;
        if let Some(glyph) = edge_prefix {
            c.put(
                row,
                x,
                glyph,
                &LineageSpan::Edge {
                    kind: lane.node.edge,
                    session_id: lane.node.session_id.clone(),
                },
            );
            x += UnicodeWidthStr::width(glyph) + 1;
        }
        let label = lane.label_lines.join(" ");
        let mut rest: &str = &label;
        // Split the leading status glyph into its own state-colored span,
        // like the session list's status column.
        if let Some(g) = lane.summary.map(|s| status_glyph(s.state)) {
            if let Some(stripped) = label.strip_prefix(g) {
                c.put(
                    row,
                    x,
                    g,
                    &LineageSpan::NodeStatus {
                        session_id: lane.node.session_id.clone(),
                    },
                );
                x += UnicodeWidthStr::width(g);
                rest = stripped;
            }
        }
        let rest = rest.to_string();
        c.put(
            row,
            x,
            &rest,
            &LineageSpan::Node {
                session_id: lane.node.session_id.clone(),
            },
        );
        c.node_rows.push((row, lane.node.session_id.clone()));
        c.boxes.push(LineageBoxBounds {
            session_id: lane.node.session_id.clone(),
            x: text_x,
            y: row,
            width: x - text_x + UnicodeWidthStr::width(rest.as_str()),
            height: 1,
        });
    };
    let put_info = |c: &mut Canvas,
                    row: usize,
                    rail_col: usize,
                    delta: u64,
                    tokens: TokenTally,
                    start: i64,
                    end: Option<i64>,
                    outcome: Option<bool>,
                    owner: &str,
                    busy: Option<u64>| {
        match outcome {
            Some(ok) => c.put(
                row,
                rail_col,
                if ok { "✓" } else { "✗" },
                &LineageSpan::SegmentOutcome {
                    ok,
                    session_id: owner.to_string(),
                },
            ),
            None => c.put(
                row,
                rail_col,
                "•",
                &LineageSpan::SegmentBullet {
                    session_id: owner.to_string(),
                },
            ),
        }
        let text = match busy {
            Some(b) => segment_label_busy(delta, tokens, b),
            None => segment_label(delta, tokens, start, end, now_ms),
        };
        c.put(
            row,
            text_x,
            &text,
            &LineageSpan::Segment {
                delta_events: delta,
                tokens,
                start_ms: start,
                end_ms: end,
                session_id: owner.to_string(),
            },
        );
    };
    // Horizontal connector between two rails, breaking around live rails
    // it crosses (their bar is painted first, dashes fill only empty
    // cells).
    let bridge_and_dash = |c: &mut Canvas,
                           row: usize,
                           from: usize,
                           to: usize,
                           lanes: &[Lane],
                           lanes_placed: &[bool],
                           lanes_ended: &[bool],
                           rail_of: &[usize],
                           owner: &str| {
        let (lo, hi) = (from.min(to), from.max(to));
        for (i, &r) in rail_of.iter().enumerate() {
            let rc = col(r);
            if rc > lo && rc < hi && lanes_placed[i] && !lanes_ended[i] {
                let sid = lanes[i].node.session_id.clone();
                c.put_if_empty(row, rc, '│', &LineageSpan::Border { session_id: sid });
            }
        }
        let dash = LineageSpan::Border {
            session_id: owner.to_string(),
        };
        for x in (lo + 1)..hi {
            c.put_if_empty(row, x, '─', &dash);
        }
    };

    let mut cur = 0usize;
    let mut gi = 0usize;
    while gi < events.len() {
        let (t, kind, i) = events[gi];
        match kind {
            EV_BOX => {
                if lanes[i].parent.is_none() {
                    let border = LineageSpan::Border {
                        session_id: lanes[i].node.session_id.clone(),
                    };
                    c.put(cur, col(0), "●", &border);
                    put_label(&mut c, cur, &lanes[i], None);
                    placed[i] = true;
                    first_row[i] = cur + 1;
                    last_row[i] = cur;
                    cur += 1;
                    if let Some((count, running, expanded)) = lanes[i].subagent_marker {
                        c.put(cur, col(rail_of[i]), "├", &border);
                        c.put(
                            cur,
                            text_x,
                            &subagent_marker_text(count, running, expanded),
                            &LineageSpan::SubagentsToggle {
                                session_id: lanes[i].node.session_id.clone(),
                                expanded,
                            },
                        );
                        last_row[i] = cur;
                        cur += 1;
                    }
                    gi += 1;
                    continue;
                }
                let p = lanes[i].parent.expect("child lane has a parent");
                if let Some(seq) = lanes[i].fork_seq {
                    let d = seq.saturating_sub(lanes[p].cp_seq);
                    if d > 0 {
                        let owner = lanes[p].node.session_id.clone();
                        let busy = (lanes[i].fork_busy > 0)
                            .then(|| lanes[i].fork_busy.saturating_sub(lanes[p].cp_busy));
                        let shown = if lanes[i].fork_msgs > 0 {
                            lanes[i].fork_msgs.saturating_sub(lanes[p].cp_msgs)
                        } else {
                            d
                        };
                        let tokens = lanes[i].fork_tokens.saturating_sub(&lanes[p].cp_tokens);
                        put_info(
                            &mut c,
                            cur,
                            col(rail_of[p]),
                            shown,
                            tokens,
                            lanes[p].cp_ms,
                            Some(lanes[i].box_ms),
                            None,
                            &owner,
                            busy,
                        );
                        last_row[p] = last_row[p].max(cur);
                        cur += 1;
                    }
                    lanes[p].cp_seq = seq;
                    lanes[p].cp_ms = lanes[i].box_ms;
                    lanes[p].cp_busy = lanes[p].cp_busy.max(lanes[i].fork_busy);
                    lanes[p].cp_msgs = lanes[p].cp_msgs.max(lanes[i].fork_msgs);
                    lanes[p].cp_tokens = lanes[p].cp_tokens.max(&lanes[i].fork_tokens);
                }
                let (pc, cc) = (col(rail_of[p]), col(rail_of[i]));
                let child_border = LineageSpan::Border {
                    session_id: lanes[i].node.session_id.clone(),
                };
                let parent_border = LineageSpan::Border {
                    session_id: lanes[p].node.session_id.clone(),
                };
                bridge_and_dash(
                    &mut c,
                    cur,
                    pc,
                    cc,
                    &lanes,
                    &placed,
                    &ended,
                    &rail_of,
                    &lanes[i].node.session_id.clone(),
                );
                c.put(cur, pc, "├", &parent_border);
                c.put(cur, cc, if cc > pc { "┐" } else { "┌" }, &child_border);
                let glyph = if is_reset_snapshot_edge(&lanes[i]) {
                    "↺"
                } else {
                    match lanes[i].node.edge {
                        LineageEdge::Fork => "⑂",
                        LineageEdge::Subagent => "▸",
                        LineageEdge::Root => "",
                    }
                };
                put_label(&mut c, cur, &lanes[i], Some(glyph));
                placed[i] = true;
                first_row[i] = cur + 1;
                last_row[i] = cur;
                last_row[p] = last_row[p].max(cur);
                cur += 1;
                if let Some((count, running, expanded)) = lanes[i].subagent_marker {
                    c.put(cur, col(rail_of[i]), "├", &child_border);
                    c.put(
                        cur,
                        text_x,
                        &subagent_marker_text(count, running, expanded),
                        &LineageSpan::SubagentsToggle {
                            session_id: lanes[i].node.session_id.clone(),
                            expanded,
                        },
                    );
                    last_row[i] = cur;
                    cur += 1;
                }
                gi += 1;
            }
            EV_MERGE => {
                let (at, mseq, mbusy, mmsgs, mtokens) =
                    lanes[i].merge.expect("merge event has merge data");
                let p = lanes[i].parent.expect("a merging fork has a parent");
                let dp = mseq.saturating_sub(lanes[p].cp_seq);
                if dp > 0 {
                    let owner = lanes[p].node.session_id.clone();
                    let busy = (mbusy > 0).then(|| mbusy.saturating_sub(lanes[p].cp_busy));
                    let shown = if mmsgs > 0 {
                        mmsgs.saturating_sub(lanes[p].cp_msgs)
                    } else {
                        dp
                    };
                    let tokens = mtokens.saturating_sub(&lanes[p].cp_tokens);
                    put_info(
                        &mut c,
                        cur,
                        col(rail_of[p]),
                        shown,
                        tokens,
                        lanes[p].cp_ms,
                        Some(at),
                        None,
                        &owner,
                        busy,
                    );
                    last_row[p] = last_row[p].max(cur);
                    cur += 1;
                }
                let df = lanes[i]
                    .summary
                    .map(|s| s.event_count.saturating_sub(lanes[i].cp_seq))
                    .unwrap_or(0);
                if df > 0 {
                    let owner = lanes[i].node.session_id.clone();
                    let busy = (lanes[i].busy_total > 0)
                        .then(|| lanes[i].busy_total.saturating_sub(lanes[i].cp_busy));
                    let shown = if lanes[i].msgs_total > 0 {
                        lanes[i].msgs_total.saturating_sub(lanes[i].cp_msgs)
                    } else {
                        df
                    };
                    let tokens = lanes[i].tokens_total.saturating_sub(&lanes[i].cp_tokens);
                    put_info(
                        &mut c,
                        cur,
                        col(rail_of[i]),
                        shown,
                        tokens,
                        lanes[i].cp_ms,
                        Some(at),
                        Some(true),
                        &owner,
                        busy,
                    );
                    last_row[i] = last_row[i].max(cur);
                    cur += 1;
                }
                let (pc, cc) = (col(rail_of[p]), col(rail_of[i]));
                let fork_border = LineageSpan::Border {
                    session_id: lanes[i].node.session_id.clone(),
                };
                let parent_border = LineageSpan::Border {
                    session_id: lanes[p].node.session_id.clone(),
                };
                let owner = lanes[i].node.session_id.clone();
                bridge_and_dash(
                    &mut c, cur, pc, cc, &lanes, &placed, &ended, &rail_of, &owner,
                );
                c.put(cur, pc, "├", &parent_border);
                c.put(cur, cc, if cc > pc { "┘" } else { "└" }, &fork_border);
                c.put(cur, text_x, "↩ merge", &fork_border);
                lanes[p].cp_seq = mseq;
                lanes[p].cp_ms = at;
                lanes[p].cp_busy = lanes[p].cp_busy.max(mbusy);
                lanes[p].cp_msgs = lanes[p].cp_msgs.max(mmsgs);
                lanes[p].cp_tokens = lanes[p].cp_tokens.max(&mtokens);
                last_row[i] = last_row[i].max(cur);
                last_row[p] = last_row[p].max(cur);
                ended[i] = true;
                cur += 1;
                gi += 1;
            }
            EV_END => {
                let mut group = vec![i];
                while gi + 1 < events.len() && events[gi + 1].0 == t && events[gi + 1].1 == EV_END {
                    gi += 1;
                    group.push(events[gi].2);
                }
                for &j in &group {
                    let border = LineageSpan::Border {
                        session_id: lanes[j].node.session_id.clone(),
                    };
                    for n in std::mem::take(&mut lanes[j].more) {
                        c.put(cur, col(rail_of[j]), "├", &border);
                        c.put(cur, text_x, &format!("+{n} more"), &LineageSpan::More(n));
                        last_row[j] = last_row[j].max(cur);
                        cur += 1;
                    }
                    let d = lanes[j]
                        .summary
                        .map(|s| s.event_count.saturating_sub(lanes[j].cp_seq))
                        .unwrap_or(0);
                    if d > 0 {
                        let (_, label_end, outcome) = lanes[j].end.expect("end event has end data");
                        let owner = lanes[j].node.session_id.clone();
                        let busy = (lanes[j].busy_total > 0)
                            .then(|| lanes[j].busy_total.saturating_sub(lanes[j].cp_busy));
                        let shown = if lanes[j].msgs_total > 0 {
                            lanes[j].msgs_total.saturating_sub(lanes[j].cp_msgs)
                        } else {
                            d
                        };
                        let tokens = lanes[j].tokens_total.saturating_sub(&lanes[j].cp_tokens);
                        put_info(
                            &mut c,
                            cur,
                            col(rail_of[j]),
                            shown,
                            tokens,
                            lanes[j].cp_ms,
                            label_end,
                            outcome,
                            &owner,
                            busy,
                        );
                        last_row[j] = last_row[j].max(cur);
                        cur += 1;
                    }
                    ended[j] = true;
                }
                gi += 1;
            }
            _ => unreachable!(),
        }
    }

    // End-fill: every rail runs unbroken from below its entry row to its
    // last content row (an empty range when it has nothing below).
    for i in 0..lanes.len() {
        if !placed[i] {
            continue;
        }
        let rc = col(rail_of[i]);
        let border = LineageSpan::Border {
            session_id: lanes[i].node.session_id.clone(),
        };
        for yy in first_row[i]..=last_row[i] {
            c.put_if_empty(yy, rc, '│', &border);
        }
    }

    let boxes = std::mem::take(&mut c.boxes);
    (c.into_rows(), boxes)
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
    format_duration_ms(now_ms.saturating_sub(since_ms).max(0) as u64)
}

/// Compact duration label for a millisecond span.
pub fn format_duration_ms(ms: u64) -> String {
    let secs = ms / 1000;
    let m = secs / 60;
    let s = secs % 60;
    if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// [`segment_label`]'s compute-time sibling: `"<count> · <summed busy>"` —
/// the total time the session actually spent computing within the window,
/// not the wall-clock span between its boundary events.
pub fn segment_label_busy(delta_events: u64, tokens: TokenTally, busy_ms: u64) -> String {
    format!(
        "{} \u{00b7} {}",
        segment_count_label(delta_events, tokens),
        format_duration_ms(busy_ms)
    )
}

/// Renderable text for one activity-segment row: `"<count> · elapsed"`.
/// `end_ms` is the segment's own end when known, else `now_ms` (the render
/// frame's live clock) — same split `render_lineage_row` used to take
/// `now_ms` for per-node stats before those moved to segments. Cost is
/// deliberately not shown here (unlike the old per-node stats label): it's
/// a single cumulative total on `SessionSummary`, with no per-checkpoint
/// snapshot the way `event_count` has via `transcript_seq`/`merged_seq`, so
/// there's no correct way to attribute it to one window rather than another.
pub fn segment_label(
    delta_events: u64,
    tokens: TokenTally,
    start_ms: i64,
    end_ms: Option<i64>,
    now_ms: i64,
) -> String {
    let elapsed = format_elapsed_ms(start_ms, end_ms.unwrap_or(now_ms));
    format!(
        "{} \u{00b7} {elapsed}",
        segment_count_label(delta_events, tokens)
    )
}

/// The count half of a turn-info label: token volume (input + output) when
/// the window has tracked token data, message count otherwise (spec 0103).
pub fn segment_count_label(delta_events: u64, tokens: TokenTally) -> String {
    if tokens.total() > 0 {
        format!("{} tok", format_token_count(tokens.total()))
    } else {
        let unit = if delta_events == 1 { "msg" } else { "msgs" };
        format!("{delta_events} {unit}")
    }
}

/// Hover detail for a turn-info line (spec 0103): the window's message
/// count plus its token breakdown. A window with no output tokens shows a
/// single total instead of fabricating an in/out split (codex reports one
/// unsplit figure); the cached part appears only when the provider
/// reported prompt-cache reads.
pub fn segment_tooltip_label(delta_events: u64, tokens: &TokenTally) -> String {
    let unit = if delta_events == 1 { "msg" } else { "msgs" };
    let mut parts = vec![format!("{delta_events} {unit}")];
    if tokens.output == 0 {
        parts.push(format!("{} tok total", format_token_count(tokens.total())));
    } else {
        parts.push(format!("in {}", format_token_count(tokens.input)));
        parts.push(format!("out {}", format_token_count(tokens.output)));
    }
    if tokens.cached > 0 {
        parts.push(format!("cached {}", format_token_count(tokens.cached)));
    }
    format!(" {} ", parts.join(" \u{00b7} "))
}

/// Compact human token count: `950`, `4.2k`, `87k`, `1.3M`. One decimal
/// only while it's informative (below one order of magnitude past the
/// unit), so labels stay narrow.
pub fn format_token_count(n: u64) -> String {
    if n >= 10_000_000 {
        format!("{}M", n / 1_000_000)
    } else if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 10_000 {
        format!("{}k", n / 1_000)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use construct_protocol::{ForkMerge, ForkedFrom};
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
            effort: None,
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
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: Default::default(),
            context_used: None,
            context_window: None,
            approval_mode: construct_protocol::ApprovalMode::Manual,
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
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: false,
        });
        s
    }

    fn subagent_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.kind = SessionKind::Subagent;
        s.parent_session_id = Some(parent.to_string());
        s
    }

    /// Like `forked_from`, but marked as a fork synthesized automatically
    /// by a harness-native context reset (spec 0085) rather than a
    /// user-initiated fork — drives the distinct `↺` edge glyph.
    fn reset_snapshot_of(mut s: SessionSummary, parent: &str) -> SessionSummary {
        s.archived = true;
        s.forked_from = Some(ForkedFrom {
            session_id: parent.to_string(),
            transcript_seq: 0,
            at_ms: 0,
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: true,
        });
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
            parent_busy_ms: 0,
            parent_message_count: 0,
            parent_tokens: Default::default(),
            is_reset_snapshot: false,
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
            merged_busy_ms: 0,
            merged_message_count: 0,
            merged_tokens: Default::default(),
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
                    ..
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

    // -- spec 0085: reset = fork-and-archive, distinct edge glyph --------

    #[test]
    fn reset_synthesized_fork_renders_a_distinct_glyph() {
        // "b" is an ordinary, user-initiated fork of "a"; "c" is a fork
        // synthesized automatically by a context reset. Both are plain
        // Fork edges — only `forked_from.is_reset_snapshot` tells them
        // apart, and only the glyph should differ.
        let sessions = vec![
            base("a"),
            forked_from(base("b"), "a"),
            reset_snapshot_of(base("c"), "a"),
        ];
        let tree = build_tree("a", &sessions).expect("tree");
        assert_eq!(tree.children.len(), 2);

        let rows = flatten(&tree, &sessions, 9_000);
        let text = diagram_text(&rows).join("\n");
        assert!(
            text.contains('⑂'),
            "expected the ordinary fork glyph in:\n{text}"
        );
        assert!(
            text.contains('↺'),
            "expected the reset-snapshot glyph in:\n{text}"
        );

        let (rail_rows, _) = flatten_rails(&tree, &sessions, 9_000);
        let rail_text = diagram_text(&rail_rows).join("\n");
        assert!(
            rail_text.contains('⑂'),
            "expected the ordinary fork glyph in rails mode:\n{rail_text}"
        );
        assert!(
            rail_text.contains('↺'),
            "expected the reset-snapshot glyph in rails mode:\n{rail_text}"
        );
    }

    #[test]
    fn plain_fork_without_reset_flag_is_unaffected() {
        // No `ForkedFrom` in this tree sets `is_reset_snapshot` — must be a
        // complete no-op versus the pre-0085 glyph selection.
        let sessions = vec![base("a"), forked_from(base("b"), "a")];
        let tree = build_tree("a", &sessions).expect("tree");
        let rows = flatten(&tree, &sessions, 9_000);
        let text = diagram_text(&rows).join("\n");
        assert!(text.contains('⑂'));
        assert!(!text.contains('↺'), "no reset fork exists:\n{text}");
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
            merged_busy_ms: 0,
            merged_message_count: 0,
            merged_tokens: Default::default(),
            merged_seq: 0,
        });
        assert_eq!(ForkStatus::of(&open), ForkStatus::Merged);

        open.merge = Some(ForkMerge {
            mode: ForkMergeMode::Discard,
            at_ms: 0,
            merged_busy_ms: 0,
            merged_message_count: 0,
            merged_tokens: Default::default(),
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
                // Every turn-info line gets a lane-bar row above it; the
                // structural row below (box top, arrow) carries the bar
                // for mid-timeline windows, and a terminal window gets
                // nothing below — its lane ends there.
                "   │".to_string(),
                "   • 12 msgs · 5m00s".to_string(),
                "   │      ┌─────────┐".to_string(),
                format!("   ├─ ⑂ ─▸│ {g} smith │"),
                "   │      └─────────┘".to_string(),
                "   │        │".to_string(),
                // Labels reserve no columns: the parent's window text runs
                // underneath the fork's lane (the bar shows a gap on that
                // row), and the fork's ✓ window — which would collide —
                // staggers onto the next row. Merging IS the fork's
                // completion, so its final window leads with ✓ (and its
                // box carries no "↩ merged" marker).
                "   • 3 msgs · 3m20s".to_string(),
                "   │        ✓ 2 msgs · 3m20s".to_string(),
                "   │◂─ ↩ ───┘".to_string(),
                "   │".to_string(),
                "   • 5 msgs · 5m00s".to_string(),
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

    /// Every turn-info window's label text, in on-screen order.
    fn segment_texts(rows: &[LineageRow]) -> Vec<String> {
        rows.iter()
            .flat_map(|r| r.spans.iter())
            .filter_map(|s| match &s.role {
                LineageSpan::Segment { .. } => Some(s.text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Rows after `idx` must contain nothing owned by `owner` — the lane
    /// has ended and its rail/bars must not continue below.
    fn assert_lane_discontinues(rows: &[LineageRow], owner: &str, after: usize) {
        for (i, row) in rows.iter().enumerate().skip(after + 1) {
            for span in &row.spans {
                assert!(
                    span.role.owner() != Some(owner),
                    "row {i} still carries a span owned by {owner:?} after its \
                     lane ended at row {after}: {:?}",
                    row.text()
                );
            }
        }
    }

    fn last_owned_row(rows: &[LineageRow], owner: &str) -> usize {
        rows.iter()
            .rposition(|r| r.spans.iter().any(|sp| sp.role.owner() == Some(owner)))
            .expect("owner appears somewhere")
    }

    #[test]
    fn subagents_collapse_behind_a_toggle_marker_by_default() {
        // With expansion tracking on (the section's mode), subagent
        // children sit behind one "▸ N subagents · M running" row; forks
        // always materialize — they're the structure the user built.
        let root = base("root");
        let mut sub_a = base("sub-a");
        sub_a.kind = SessionKind::Subagent;
        sub_a.parent_session_id = Some("root".into());
        let mut sub_b = base("sub-b");
        sub_b.kind = SessionKind::Subagent;
        sub_b.parent_session_id = Some("root".into());
        sub_b.state = SessionState::Done;
        let fork = forked_from(base("fork"), "root");
        let sessions = vec![root, sub_a, sub_b, fork];

        let none = HashSet::new();
        let tree = build_tree_with_expansions("root", &sessions, Some(&none)).unwrap();
        assert_eq!(
            tree.children.len(),
            2,
            "marker + the fork; the subagent nodes are not materialized"
        );
        assert_eq!(
            tree.children[0],
            LineageChild::Subagents {
                count: 2,
                running: 1,
                expanded: false
            },
            "sub-a is Running, sub-b is Done — one of two running"
        );
        let LineageChild::Node(f) = &tree.children[1] else {
            panic!("fork materializes");
        };
        assert_eq!(f.session_id, "fork");
        // The marker row renders with the running tally, in both layouts.
        for text in [
            flatten(&tree, &sessions, 9_000)
                .iter()
                .map(|r| r.text())
                .collect::<Vec<_>>()
                .join("\n"),
            flatten_rails(&tree, &sessions, 9_000)
                .0
                .iter()
                .map(|r| r.text())
                .collect::<Vec<_>>()
                .join("\n"),
        ] {
            assert!(
                text.contains("▸ 2 subagents · 1 running"),
                "collapsed marker with running tally: {text}"
            );
        }

        // Expanding the parent materializes the group after a ▾ marker.
        let mut expanded = HashSet::new();
        expanded.insert("root".to_string());
        let tree = build_tree_with_expansions("root", &sessions, Some(&expanded)).unwrap();
        assert_eq!(
            tree.children[0],
            LineageChild::Subagents {
                count: 2,
                running: 1,
                expanded: true
            }
        );
        let ids: Vec<&str> = tree
            .children
            .iter()
            .filter_map(|c| match c {
                LineageChild::Node(n) => Some(n.session_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids, vec!["sub-a", "sub-b", "fork"]);
        let text = flatten(&tree, &sessions, 9_000)
            .iter()
            .map(|r| r.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("▾ 2 subagents"),
            "expanded marker keeps the collapse affordance: {text}"
        );

        // Without tracking (legacy build_tree), no markers — everything
        // materializes, exactly as before.
        let tree = build_tree("root", &sessions).unwrap();
        assert!(tree
            .children
            .iter()
            .all(|c| matches!(c, LineageChild::Node(_))));
    }

    #[test]
    fn the_focused_sessions_chain_is_always_expanded() {
        // The section shows the SELECTED session's tree — a subagent must
        // stay visible even while every group defaults to collapsed.
        let root = base("root");
        let mut sub = base("sub");
        sub.kind = SessionKind::Subagent;
        sub.parent_session_id = Some("root".into());
        let mut nested = base("nested");
        nested.kind = SessionKind::Subagent;
        nested.parent_session_id = Some("sub".into());
        let sessions = vec![root, sub, nested];

        let none = HashSet::new();
        let tree = build_tree_with_expansions("nested", &sessions, Some(&none)).unwrap();
        let LineageChild::Node(sub_node) = &tree.children[1] else {
            panic!("focus ancestor materializes: {:?}", tree.children);
        };
        assert_eq!(sub_node.session_id, "sub");
        let LineageChild::Node(nested_node) = &sub_node.children[1] else {
            panic!("focus itself materializes: {:?}", sub_node.children);
        };
        assert_eq!(nested_node.session_id, "nested");
    }

    #[test]
    fn exited_subagent_lane_discontinues_instead_of_running_to_now() {
        // A native subagent can exit (state `Done`, the ✓ icon) without a
        // recorded `last_event_at`. Its lane must close on the timeline —
        // final ✓ window, rail stops — while the live parent's lane keeps
        // running below it, in BOTH layouts. (It used to fall back to
        // "now", keeping the dead lane visually alive to the bottom.)
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 10);
        root.last_event_at = Some(Utc.timestamp_millis_opt(800_000).unwrap());
        let mut sub = with_event_count(with_created_at_ms(base("sub"), 100_000), 3);
        sub.kind = SessionKind::Subagent;
        sub.parent_session_id = Some("root".to_string());
        sub.state = SessionState::Done;
        sub.last_event_at = None;
        let sessions = vec![root, sub];
        let tree = build_tree("root", &sessions).unwrap();

        for rows in [
            flatten(&tree, &sessions, 900_000),
            flatten_rails(&tree, &sessions, 900_000).0,
        ] {
            let text = rows.iter().map(|r| r.text()).collect::<Vec<_>>().join("\n");
            // The glyph and text sit in separate columns (rails mode), so
            // assert via the span roles rather than adjacency.
            let has_outcome = rows.iter().flat_map(|r| r.spans.iter()).any(|sp| {
                matches!(
                    &sp.role,
                    crate::lineage::LineageSpan::SegmentOutcome { ok: true, session_id }
                        if session_id == "sub"
                )
            });
            assert!(
                has_outcome && text.contains("3 msgs"),
                "the exited child's final window carries its ✓ outcome: {text}"
            );
            let sub_last = last_owned_row(&rows, "sub");
            let root_last = last_owned_row(&rows, "root");
            assert!(
                sub_last < root_last,
                "the exited child's lane ends before the live parent's \
                 trailing window (sub row {sub_last} vs root row {root_last}):\n{text}"
            );
            assert_lane_discontinues(&rows, "sub", sub_last);
        }
    }

    #[test]
    fn turn_info_durations_are_summed_compute_time_not_wall_clock() {
        // Same timeline as the concept-layout test (fork at 300s, merge at
        // 500s, now at 800s — wall labels would read 5m00s / 3m20s /
        // 3m20s / 5m00s), but with busy-time stamps that differ from every
        // wall span. Each window must label the busy DELTA between its
        // boundary stamps — the time the session actually spent computing —
        // not the wall-clock gap between its boundary events.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        root.busy_ms = 400_000; // lifetime compute: 6m40s
        let mut fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 12, 300_000), 300_000),
                2,
            ),
            ForkMergeMode::Result,
            15,
            500_000,
        );
        fork.forked_from.as_mut().unwrap().parent_busy_ms = 250_000; // 4m10s
        fork.merge.as_mut().unwrap().merged_busy_ms = 310_000; // parent +1m00s
        fork.busy_ms = 90_000; // fork's own compute: 1m30s
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let expect = vec![
            "12 msgs · 4m10s".to_string(), // 250_000 − 0
            "3 msgs · 1m00s".to_string(),  // 310_000 − 250_000
            "2 msgs · 1m30s".to_string(),  // fork: 90_000 − 0
            "5 msgs · 1m30s".to_string(),  // 400_000 − 310_000
        ];
        assert_eq!(segment_texts(&flatten(&tree, &sessions, 800_000)), expect);
        // The compact layout emits the identical windows.
        assert_eq!(
            segment_texts(&flatten_rails(&tree, &sessions, 800_000).0),
            expect
        );
    }

    #[test]
    fn turn_info_counts_only_chat_messages_when_tracked() {
        // `event_count` advances on every persisted transcript event (tool
        // blocks, status rows, PTY ordering markers) — but "N msgs" must
        // count actual chat messages. Same scenario as the busy-time test,
        // now with message tallies stamped at the same boundaries; every
        // window's count must come from the message deltas (5/2/2/1), not
        // the raw event deltas (12/3/2/5).
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        root.busy_ms = 400_000;
        root.message_count = 8;
        let mut fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 12, 300_000), 300_000),
                2,
            ),
            ForkMergeMode::Result,
            15,
            500_000,
        );
        {
            let ff = fork.forked_from.as_mut().unwrap();
            ff.parent_busy_ms = 250_000;
            ff.parent_message_count = 5;
        }
        {
            let m = fork.merge.as_mut().unwrap();
            m.merged_busy_ms = 310_000;
            m.merged_message_count = 7;
        }
        fork.busy_ms = 90_000;
        fork.message_count = 2;
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let expect = vec![
            "5 msgs · 4m10s".to_string(), // messages 0→5 while fork split off
            "2 msgs · 1m00s".to_string(), // parent 5→7 while the fork ran
            "2 msgs · 1m30s".to_string(), // the fork's own two messages
            "1 msg · 1m30s".to_string(),  // parent 7→8 after merge (singular)
        ];
        assert_eq!(segment_texts(&flatten(&tree, &sessions, 800_000)), expect);
        assert_eq!(
            segment_texts(&flatten_rails(&tree, &sessions, 800_000).0),
            expect
        );
    }

    #[test]
    fn turn_info_prefers_token_deltas_when_tracked() {
        // Same scenario as the message-count test, now with token tallies
        // stamped at the same boundaries (spec 0103): every window labels
        // its token volume — the input+output delta between its boundary
        // stamps — instead of the message count.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        root.busy_ms = 400_000;
        root.message_count = 8;
        root.tokens = TokenTally {
            input: 100_000,
            output: 12_000,
            cached: 80_000,
        };
        let mut fork = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("f"), "root", 12, 300_000), 300_000),
                2,
            ),
            ForkMergeMode::Result,
            15,
            500_000,
        );
        {
            let ff = fork.forked_from.as_mut().unwrap();
            ff.parent_busy_ms = 250_000;
            ff.parent_message_count = 5;
            ff.parent_tokens = TokenTally {
                input: 40_000,
                output: 2_000,
                cached: 30_000,
            };
        }
        {
            let m = fork.merge.as_mut().unwrap();
            m.merged_busy_ms = 310_000;
            m.merged_message_count = 7;
            m.merged_tokens = TokenTally {
                input: 60_000,
                output: 3_000,
                cached: 45_000,
            };
        }
        fork.busy_ms = 90_000;
        fork.message_count = 2;
        fork.tokens = TokenTally {
            input: 9_000,
            output: 500,
            cached: 6_000,
        };
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let expect = vec![
            "42k tok · 4m10s".to_string(),  // 42_000 while the fork split off
            "21k tok · 1m00s".to_string(),  // parent 42k→63k while the fork ran
            "9.5k tok · 1m30s".to_string(), // the fork's own consumption
            "49k tok · 1m30s".to_string(),  // parent 63k→112k after merge
        ];
        assert_eq!(segment_texts(&flatten(&tree, &sessions, 800_000)), expect);
        assert_eq!(
            segment_texts(&flatten_rails(&tree, &sessions, 800_000).0),
            expect
        );
    }

    #[test]
    fn turn_info_falls_back_to_wall_clock_without_busy_stamps() {
        // Records written before busy tracking carry zero busy stamps; their
        // windows keep the legacy wall-clock spans (and their counts fall
        // back to raw transcript-event deltas).
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
        let expect = vec![
            "12 msgs · 5m00s".to_string(),
            "3 msgs · 3m20s".to_string(),
            "2 msgs · 3m20s".to_string(),
            "5 msgs · 5m00s".to_string(),
        ];
        assert_eq!(segment_texts(&flatten(&tree, &sessions, 800_000)), expect);
        assert_eq!(
            segment_texts(&flatten_rails(&tree, &sessions, 800_000).0),
            expect
        );
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
        assert!(
            text.contains("✓ 3 msgs"),
            "the terminal glyph replaces the bullet on the final window: {text}"
        );
        assert!(
            rows.iter().flat_map(|r| r.spans.iter()).any(|s| matches!(
                s.role,
                LineageSpan::SegmentOutcome { ok: true, .. }
            ) && s.text == "✓"),
            "{text}"
        );

        let mut errored = with_event_count(with_created_at_ms(base("err"), 0), 3);
        errored.state = SessionState::Errored;
        let rows = flatten(
            &build_tree("err", &[errored.clone()]).unwrap(),
            &[errored],
            9_000,
        );
        assert!(rows.iter().flat_map(|r| r.spans.iter()).any(|s| matches!(
            s.role,
            LineageSpan::SegmentOutcome { ok: false, .. }
        ) && s.text == "✗"));

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
        // And it belongs to root's final window, not the fork's.
        let outcome_owner = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .find_map(|s| match &s.role {
                LineageSpan::SegmentOutcome { session_id, .. } => Some(session_id.clone()),
                _ => None,
            })
            .expect("outcome span");
        assert_eq!(outcome_owner, "root");
    }

    #[test]
    fn merge_arrows_bridge_over_live_lanes_in_the_compact_layout() {
        // A branches FIRST (t=1000) and stays open; B branches later
        // (t=2000, shifted just right of A's lane) and merges back at
        // t=5000 while A still runs. With minimal-x placement, B's merge
        // arrow travels back to the parent lane THROUGH A's live lane —
        // and bridges over its bar (─│─), exactly like git-graph merge
        // lines, instead of forcing B's whole column further out.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let a = with_event_count(
            with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
            3,
        );
        let b = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("b"), "root", 10, 2_000), 2_000),
                2,
            ),
            ForkMergeMode::Result,
            14,
            5_000,
        );
        let sessions = vec![root, a, b];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
        let text = diagram_text(&rows);

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
            unreachable!()
        };
        let row_of = |id: &str| {
            rows.iter()
                .position(|r| r.session_id() == Some(id))
                .unwrap()
        };
        assert!(
            row_of("a") < row_of("b"),
            "a branched first, so its box renders first:\n{}",
            text.join("\n")
        );
        assert_eq!(
            label_col("b"),
            label_col("a"),
            "both boxes pack at the arrow-minimum x (rows never collide); \
             only their lanes differ:\n{}",
            text.join("\n")
        );
        // B's merge arrow crosses A's live lane and bridges over its bar.
        let merge_line = text
            .iter()
            .find(|l| l.contains("◂─ ↩"))
            .expect("merge arrow row");
        assert!(
            merge_line.contains("─│─") || merge_line.contains("─│") && merge_line.contains("┘"),
            "the merge arrow bridges over a's live lane:\n{}",
            text.join("\n")
        );
    }

    #[test]
    fn rails_mode_renders_the_git_graph_style_compact_view() {
        // Same canonical scenario as the boxed-mode snapshot, in rails
        // mode: one 2-column rail per session, one-line entries in global
        // time order, connectors curving between rails, text in one
        // left-aligned column.
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
        let (rows, boxes) = flatten_rails(&tree, &sessions, 800_000);
        let g = status_glyph(SessionState::Running);
        assert_eq!(
            diagram_text(&rows),
            vec![
                format!(" \u{25cf}    {g} smith"),
                " \u{2022}    12 msgs \u{b7} 5m00s".to_string(),
                format!(" \u{251c}\u{2500}\u{2510}  \u{2442} {g} smith"),
                " \u{2022} \u{2502}  3 msgs \u{b7} 3m20s".to_string(),
                " \u{2502} \u{2713}  2 msgs \u{b7} 3m20s".to_string(),
                " \u{251c}\u{2500}\u{2518}  \u{21a9} merge".to_string(),
                " \u{2022}    5 msgs \u{b7} 5m00s".to_string(),
            ]
        );
        // Selection and hit-testing work identically to boxed mode: the
        // two label rows are selectable and report bounds.
        let ids: Vec<_> = selectable_indices(&rows)
            .into_iter()
            .map(|i| rows[i].session_id().unwrap().to_string())
            .collect();
        assert_eq!(ids, vec!["root".to_string(), "f".to_string()]);
        assert_eq!(boxes.len(), 2);
        assert!(boxes.iter().all(|b| b.height == 1));
    }

    #[test]
    fn rails_mode_reuses_a_rail_after_its_lane_closes() {
        // Sequential forks share one rail; the diagram stays two rails
        // wide — classic git-graph column reuse.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 30);
        let a = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
                4,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let b = with_event_count(
            with_created_at_ms(forked_from_at(base("b"), "root", 15, 5_000), 5_000),
            2,
        );
        let sessions = vec![root, a, b];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten_rails(&tree, &sessions, 9_000).0;
        let label_col = |id: &str| {
            let row = rows
                .iter()
                .find(|r| r.session_id() == Some(id))
                .unwrap_or_else(|| panic!("{id} row"));
            row.text()
                .find('\u{2442}')
                .unwrap_or_else(|| panic!("{id} glyph"))
        };
        // b branched after a merged, so both labels sit at the same text
        // column and the diagram stays two rails wide.
        assert_eq!(label_col("a"), label_col("b"));
    }

    #[test]
    fn a_fresh_lanes_first_bar_gets_air_before_the_next_box_covers_it() {
        // Fork A branches, then a subagent's box is drawn immediately
        // after A's box, spanning A's lane column. Without a spacer row,
        // A's lane would vanish behind that box with zero visible start —
        // one row of air shows its timeline beginning under its own box.
        // Concurrent lanes also spread apart under the shared box span
        // instead of running side by side.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let a = with_event_count(
            with_created_at_ms(forked_from_at(base("a"), "root", 5, 1_000), 1_000),
            3,
        );
        let sub = with_event_count(with_created_at_ms(subagent_of(base("s"), "root"), 1_500), 2);
        let sessions = vec![root, a, sub];
        let tree = build_tree("root", &sessions).unwrap();
        let (rows, boxes) = flatten_with_boxes(&tree, &sessions, 9_000);
        let text = diagram_text(&rows);
        let a_box = boxes.iter().find(|b| b.session_id == "a").expect("a box");
        let s_box = boxes.iter().find(|b| b.session_id == "s").expect("s box");
        let a_lane_col = {
            // A's lane shows on the spacer row between the two boxes.
            let spacer = a_box.y + a_box.height;
            assert!(
                s_box.y > spacer,
                "one row of air between a's box and the covering box:\n{}",
                text.join("\n")
            );
            text[spacer]
                .char_indices()
                .filter(|(_, ch)| *ch == '│')
                .map(|(idx, _)| text[spacer][..idx].chars().count())
                .find(|col| *col >= a_box.x && *col < a_box.x + a_box.width)
                .expect("a's bar visible under its own box on the spacer row")
        };
        // And the subagent's lane sits well apart from a's.
        let s_rows_after = s_box.y + s_box.height;
        let s_lane_col = text[s_rows_after]
            .char_indices()
            .filter(|(_, ch)| *ch == '│')
            .map(|(idx, _)| text[s_rows_after][..idx].chars().count())
            .find(|col| *col != a_lane_col && *col > 3)
            .expect("s's own lane visible below its box");
        assert!(
            s_lane_col.abs_diff(a_lane_col) >= 6,
            "concurrent lanes spread apart (a at {a_lane_col}, s at {s_lane_col}):\n{}",
            text.join("\n")
        );
    }

    #[test]
    fn a_late_open_fork_reuses_columns_freed_by_closed_lanes() {
        // The screenshot scenario: a merged fork (M) and an early open fork
        // (O1) force two slots — O1 must sit outside M because M's merge
        // arrow returns to the parent lane. But a LATER open fork (O2),
        // branching after M closed, overlaps only O1 — and O1 never merges
        // (no returning arrow to cross), so O2 takes the freed inner slot
        // instead of stacking a third column further right.
        let root = with_event_count(with_created_at_ms(base("root"), 0), 40);
        let m = merged_at(
            with_event_count(
                with_created_at_ms(forked_from_at(base("m"), "root", 5, 1_000), 1_000),
                4,
            ),
            ForkMergeMode::Result,
            10,
            3_000,
        );
        let o1 = with_event_count(
            with_created_at_ms(forked_from_at(base("o1"), "root", 3, 500), 500),
            2,
        );
        let o2 = with_event_count(
            with_created_at_ms(forked_from_at(base("o2"), "root", 20, 5_000), 5_000),
            2,
        );
        let sessions = vec![root, m, o1, o2];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 9_000);
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
            unreachable!()
        };
        assert_eq!(
            label_col("o2"),
            label_col("m"),
            "boxes share the minimal column (rows never collide):\n{}",
            diagram_text(&rows).join("\n")
        );
        assert_eq!(
            label_col("o1"),
            label_col("o2"),
            "all sibling boxes pack at the arrow-minimum x:\n{}",
            diagram_text(&rows).join("\n")
        );
    }

    #[test]
    fn live_lane_elapsed_measures_to_last_activity_not_now() {
        // A live session's trailing window freezes its elapsed at
        // `last_event_at` — execution time for the turns, not idle
        // wall-clock ticking toward now.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 5);
        root.last_event_at = Some(Utc.timestamp_millis_opt(60_000).unwrap());
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let rows = flatten(&tree, &sessions, 3_600_000);
        assert_eq!(
            segments(&rows),
            vec![(5, 0, Some(60_000))],
            "elapsed ends at last_event_at, not at now (an hour later)"
        );
        let text = diagram_text(&rows).join("\n");
        assert!(text.contains("1m00s"), "{text}");
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
                    .any(|s| matches!(s.role, LineageSpan::SegmentOutcome { ok: true, .. }))
            })
            .expect("s's ✓ row");
        let fork_arrow_row = rows
            .iter()
            .position(|r| r.text().contains('⑂'))
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
            .filter(|(_, l)| l.contains('⑂'))
            .map(|(i, _)| i)
            .collect();
        // "◂─ ↩" is the merge arrow's icon-only head.
        let merge_row = text
            .iter()
            .position(|l| l.contains("◂─ ↩"))
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
        assert_eq!(
            label_col(b_row),
            label_col(a_row),
            "concurrent boxes pack at the same minimal x; their lanes are \
             what stay distinct: {}",
            text.join("\n")
        );
        // (With minimal-x placement B sits just right of A's lane, so its
        // branch arrow no longer crosses it — merge-arrow bridging is
        // covered by `merge_arrows_bridge_over_live_lanes_in_the_compact_
        // layout`.)

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
        let label = segment_label(42, TokenTally::default(), 0, Some(65_000), 999_999);
        assert!(label.contains("42 msgs"));
        assert!(label.contains("1m05s"));
    }

    #[test]
    fn segment_label_singular_for_one_message() {
        let label = segment_label(1, TokenTally::default(), 0, Some(1_000), 999_999);
        assert!(label.contains("1 msg "), "expected singular 'msg': {label}");
        assert!(!label.contains("msgs"));
    }

    #[test]
    fn segment_label_falls_back_to_now_when_end_is_open() {
        // An open-ended segment (`end_ms: None`) measures against the live
        // render-time clock (`now_ms`), not a baked-in end.
        let label = segment_label(3, TokenTally::default(), 0, None, 5_000);
        assert!(
            label.contains("5s"),
            "expected elapsed against now_ms: {label}"
        );
    }

    #[test]
    fn segment_count_label_prefers_tokens_over_messages() {
        assert_eq!(segment_count_label(3, TokenTally::default()), "3 msgs");
        assert_eq!(segment_count_label(1, TokenTally::default()), "1 msg");
        assert_eq!(
            segment_count_label(
                3,
                TokenTally {
                    input: 1_200,
                    output: 300,
                    cached: 0
                }
            ),
            "1.5k tok"
        );
    }

    #[test]
    fn format_token_count_scales_units() {
        assert_eq!(format_token_count(950), "950");
        assert_eq!(format_token_count(1_500), "1.5k");
        assert_eq!(format_token_count(42_000), "42k");
        assert_eq!(format_token_count(1_300_000), "1.3M");
        assert_eq!(format_token_count(12_000_000), "12M");
    }

    #[test]
    fn segment_tooltip_label_splits_omits_and_totals() {
        // Full split with cache detail.
        assert_eq!(
            segment_tooltip_label(
                5,
                &TokenTally {
                    input: 118_200,
                    output: 1_800,
                    cached: 96_400
                }
            ),
            " 5 msgs \u{00b7} in 118k \u{00b7} out 1.8k \u{00b7} cached 96k "
        );
        // No cache reads → the cached part is omitted.
        assert_eq!(
            segment_tooltip_label(
                1,
                &TokenTally {
                    input: 800,
                    output: 200,
                    cached: 0
                }
            ),
            " 1 msg \u{00b7} in 800 \u{00b7} out 200 "
        );
        // Unsplit total (codex reports one figure): no fabricated split.
        assert_eq!(
            segment_tooltip_label(
                3,
                &TokenTally {
                    input: 2_300,
                    output: 0,
                    cached: 0
                }
            ),
            " 3 msgs \u{00b7} 2.3k tok total "
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
        // Both windows close on the final "now" rows (root's first, the
        // subagent's staggered below it since the tight columns collide),
        // and the branch arrow must be labeled as a subagent edge, not a
        // fork.
        let row_of = |delta: u64| {
            rows.iter()
                .position(|r| {
                    r.spans.iter().any(
                        |s| matches!(s.role, LineageSpan::Segment { delta_events, .. } if delta_events == delta),
                    )
                })
                .unwrap_or_else(|| panic!("row with delta {delta}"))
        };
        assert!(row_of(9) < row_of(2));
        assert_eq!(row_of(9) + 1, row_of(2), "staggered onto adjacent rows");
        assert!(rows.iter().flat_map(|r| r.spans.iter()).any(|s| matches!(
            s.role,
            LineageSpan::Edge {
                kind: LineageEdge::Subagent,
                ..
            }
        ) && s.text == "▸"));
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
    fn long_titles_wrap_the_box_and_cap_with_an_ellipsis() {
        use unicode_width::UnicodeWidthStr;
        // A title longer than one box line wraps onto a second row (box
        // grows taller); content past MAX_BOX_LINES is ellipsized.
        let mut root = with_event_count(with_created_at_ms(base("root"), 0), 1);
        root.title = Some(
            "a very long session title that cannot possibly fit on one single box line at all"
                .to_string(),
        );
        let sessions = vec![root];
        let tree = build_tree("root", &sessions).unwrap();
        let (rows, boxes) = flatten_with_boxes(&tree, &sessions, 0);
        let b = &boxes[0];
        assert_eq!(
            b.height,
            MAX_BOX_LINES + 2,
            "box grows to the line cap: {:#?}",
            diagram_text(&rows)
        );
        assert!(
            b.width <= MAX_BOX_CONTENT_W + 4,
            "box width caps at the content limit plus borders/padding"
        );
        let text = diagram_text(&rows).join("\n");
        assert!(
            text.contains('…'),
            "overflow past the cap ellipsizes: {text}"
        );
        // Every label row shares the same border columns (no shearing).
        let widths: Vec<usize> = diagram_text(&rows)
            .iter()
            .take(b.height)
            .map(|l| UnicodeWidthStr::width(l.as_str()))
            .collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{widths:?}");
    }

    #[test]
    fn boxes_report_their_bounds_for_hit_testing() {
        let root = with_event_count(with_created_at_ms(base("root"), 0), 20);
        let fork = with_event_count(
            with_created_at_ms(forked_from_at(base("f"), "root", 12, 500), 500),
            2,
        );
        let sessions = vec![root, fork];
        let tree = build_tree("root", &sessions).unwrap();
        let (rows, boxes) = flatten_with_boxes(&tree, &sessions, 9_000);
        assert_eq!(boxes.len(), 2);
        let text = diagram_text(&rows);
        for b in &boxes {
            assert!(
                text[b.y].contains('┌') && text[b.y + b.height - 1].contains('└'),
                "bounds frame the border rows for {b:?}:\n{}",
                text.join("\n")
            );
            assert!(b.width >= 4 && b.height >= 3);
        }
        assert_ne!(boxes[0].session_id, boxes[1].session_id);
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
