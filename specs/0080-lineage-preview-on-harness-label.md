# 0080-lineage-preview-on-harness-label

Status: superseded by 0081-lineage-section-in-sidebar
Date: 2026-07-09
Area: tui
Scope: The pane title bar's harness label doubles as a hover/click trigger for a small, session-attached preview of that session's fork/subagent lineage, and that preview can itself be given keyboard focus to navigate, merge, discard, and jump — there is no separate full-screen lineage dialog.

## Decision

A session that has fork/subagent lineage to show (it was itself forked from a
parent, or it has at least one live fork/subagent descendant) gets an
additional behavior on its pane title bar's existing harness label (the
right-aligned harness name in `apply_pane_title_right_cluster`): hovering it
reveals a small preview box anchored to that session's own pane, rendering
the same tree data a fork/subagent lineage graph shows (edge glyphs, status,
and activity stats — see "Activity stats are per-segment, not per-node"
below). Clicking the label toggles a persistent pin, keeping the preview
open regardless of hover. Ordinary sessions with no lineage get no hit-rect
on the label at all — it renders exactly as it always has, with no
hover/click behavior and no visual change.

This preview is the ONLY lineage UI. An earlier iteration of this feature
(spec 0079) had a second, architecturally distinct surface — a full-screen
`C-x q` / `q` modal with its own global `App` slot — presented as the
"real" interactive view while this preview stayed read-only. That modal has
been deleted. Its interaction vocabulary was ported onto the preview itself
rather than kept as a second surface, because a session's lineage is
inherently a per-session concern; a global dialog for it was an unnecessary
architectural split, not a distinct need.

### Two visibility states, two label intensities

- **Hover-only** (the pointer is over the label, or has been recently — a
  short grace period lets it travel down onto the preview body without the
  preview vanishing mid-move): the "light" tier. The label bolds in a
  hover-flash color.
- **Pinned** (toggled on by a click, persists until unpinned): the "strong"
  tier. The label bolds in the accent color AND gains `Modifier::REVERSED`
  (inverts fg/bg) — this codebase's established convention for an emphatic,
  persistently-active interactive-text cue (the same modifier the
  action-link/URL hover styling uses, applied here to the *persistent*
  state rather than a hover flash). The two states must read as visibly
  different intensities, not just different hues, since the pinned state
  means "this stays here" while hover means "this is a transient glance."

Visibility itself (pinned-OR-unexpired-hover) and the hover-grace timing
mirror the shape of the (separately deprecated, spec 0003) session-widget
hover/pin system, kept as independent state rather than a dependency on it.

### Keyboard focus

A visible preview can be given keyboard focus two ways:

- Clicking inside the preview's body (its rows/content area — not the
  harness label itself, which only ever toggles the pin).
- Pressing `C-x Tab` on the selected session (a no-op if it has no lineage
  to show, same gate the harness label's own hover/click affordance uses).
  A second press on the same session's preview closes it — un-pins and
  clears focus, a single-keystroke open/close toggle.

Either entry path also pins the preview open if it wasn't already —
focusing something implies wanting to keep interacting with it, so a
preview about to auto-hide from a hover timeout shouldn't vanish out from
under active keyboard interaction.

While a preview holds focus, it owns the keyboard for its own vocabulary —
the same vocabulary the deleted modal used to own exclusively:

- `j`/`k`/arrows/`C-n`/`C-p`: move the row selection.
- `Enter`: jump into the selected session (a *merged* fork jumps to its
  parent instead, since the merge point in the graph and the transcript
  message it injected are the same event — spec 0078). Jumping in also
  clears both focus and the pin for this preview: leaving to go work in
  that session means the preview has served its purpose.
- `m` / `d`: merge or discard the selected row, via the exact same
  merge/discard path the `C-x m` minibuffer menu uses — a direct-key
  shortcut for it, not a second implementation.
- `Esc`: clears focus ONLY. It does not un-pin. A preview the user
  explicitly pinned stays visible after they're done navigating it — Esc
  backs out one level (stop owning the keyboard), it doesn't dismiss
  unrelated state, matching what Esc means everywhere else in this UI.

Any other key clears focus and is reported as unhandled, so the caller
re-dispatches the SAME keystroke through ordinary routing — the same
"a closing overlay never eats a live keystroke" rule other dismissable
overlays in this UI follow (e.g. `/configure`), so `C-x C-c` still quits
and `C-x b` still switches sessions while a preview is focused.

Tree construction, row formatting, and the merge/discard action are all
reused, never duplicated: the preview calls the same
`crate::lineage::build_tree`/`flatten`, the same per-row rendering, and the
same `App::apply_fork_merge` any other merge/discard path in this UI uses.

### The widget's border is the lineage accent, not the pane border color

The preview's border deliberately does NOT reuse the session panes' border
colors — it uses the default foreground text color, so the widget reads
as content-level chrome rather than another pane. Keyboard focus
brightens the border (bold) rather than changing its hue.

### Two visualization modes, toggled from the top border

The preview draws the tree in one of two modes, switched by a toggle
button on its top border (showing the current mode's name — "lineage"
for the boxed-lane diagram, "lineage (compact)" for the rails view; the
preview's minimum width keeps the toggle visible): the boxed-lane
diagram described below, or a git-graph-style compact view —
one two-column rail per session (columns reused once a lane closes),
one-line entries in the same global time order, connectors curving
between rails, and all text in a single left-aligned column right of the
rails. Both modes share the same row/selection/hit model, so keyboard
navigation, box hover/click, and scrolling behave identically; toggling
resets the scroll offsets since the two geometries don't correspond.

### Sizing, scrolling, resizing

The preview sizes itself to the diagram's content by default (width and
height, clamped to its pane), keeping one blank padding row between the
last content line and the bottom border. The mouse wheel scrolls it
vertically and a horizontal wheel scrolls it sideways, in every mode
(hover, pinned, focused); while keyboard-focused, moving the selection
still pulls the viewport to keep the selected box visible. Dragging the
preview's LEFT border resizes its width, the BOTTOM border its height
(the corner does both — the preview is anchored top-right, so those are
its free edges); a drag-resize overrides content sizing until the
preview is closed.

### The preview renders as a boxed-lane diagram

The preview draws each session as a small bordered box (status glyph +
title/harness) with that session's own timeline as a vertical lane
hanging below the box — indented two columns from the box's left edge —
read top to bottom. A long session name wraps onto additional box rows
(the box grows taller) up to a small line cap, after which it
ellipsizes; box width is capped so one verbose title can't stretch the
whole diagram. Labels mirror the session list's styling: only the
status glyph carries the live-state color (a Done session's check mark
goes blue-ish); the name itself keeps the default text color, never
dimmed for being a fork, with a discarded fork adding a strikethrough. A fork branches off the parent's lane with an
arrow (`├─ ⑂ ──▸`, icon-only with a space either side of the glyph —
the glyph alone marks the edge kind) into the child's box, placed to
the right with its own lane below it; a fork that merged returns to the
parent's lane with an icon-only merge arrow (`│◂─ ↩ ──┘`). A subagent
branches the same way (`▸` arrow, same brightness) but never merges
back. Turn-info labels reserve NO horizontal space: boxes pack at the
arrows' minimum offsets, and a window's text simply runs underneath any
lane bar it crosses (the bar shows a gap on that row); two windows that
would collide on a shared closing row stagger onto adjacent rows
instead. A merged fork's box
carries NO marker (the merge arrow and its ✓'d final window already say
it); a discarded fork's box keeps `✗ discarded`, since a discard draws
no arrow. Rows pack tight — no blank spacer rows between elements.

Rows are allocated from ONE global, time-ordered event queue spanning the
whole tree: every fork-out, subagent spawn, merge-back, and lane end (a
session going Done/Errored at its last event, a fork being discarded, or
"now" for live sessions) gets its rows at its position in global time
order, regardless of which lane it belongs to. Fork A, then fork B, then
merge A renders exactly those three connectors top to bottom; a subagent
that finished before a later fork branched shows its final ✓-marked turn
info above that fork's arrow. A turn-info window renders at the row where
its CLOSING event lands, on its own lane — so windows closing at the same
instant share one row side by side (a merged fork's life next to its
parent's while-it-was-out window at the merge; all live lanes' trailing
windows together on the final "now" row). Box placement is minimal-x:
a box only needs its own rows free (every event gets fresh rows, so box
rectangles never collide), leaving just two hard constraints — the
branch arrow's minimum reach from the parent's lane, and lane-column
uniqueness among lanes whose lifetimes overlap. Everything else
overlaps freely: boxes reuse columns across different rows, vertical
lines pass BEHIND boxes and turn info they cross (the bar gaps on those
rows), and a merge arrow crossing a live lane bridges over its bar,
exactly like git-graph merge lines do. Two softeners keep it readable:
a lane hangs anywhere under its box's span, placed to keep concurrent
vertical lines several columns apart when the span allows (leftmost
otherwise — never widening the diagram); and when a box would sit
directly on a live lane's very first bar row, one spacer row is
inserted so that lane's timeline visibly begins under its own box
before passing behind the covering box. The rails mode reuses columns
by lifetime interval the same way.

Every rail glyph, lane bar, connector, and turn-info span is tagged with
its owning session: selecting or hovering a session (its box in boxes
mode, its label row in rails mode) brightens that session's entire
timeline — rails, arrows, and windows — leaving every other lane
untouched.

Keyboard selection lands on the box label rows; boxes are the only
selectable rows, and opening the preview from a fork or subagent starts
the selection on THAT session's box, not the tree's root. The selection
fills exactly the selected box's INTERIOR with the highlight background
and brightens its border LINE (fg only, in the same color as the
preview widget's own border — border glyphs never get a background);
nothing outside the box is touched. The boxes are also
mouse targets: hovering one brightens its border the same way, and
clicking it jumps to that session (closing the preview, exactly like
Enter on the keyboard selection). A click on the preview's body outside
any box still gives the preview keyboard focus.

Turn-info lines always have a lane-bar row directly above them; below,
the next structural row (a box side, a branch/merge arrow) carries the
lane onward for mid-timeline windows, while a lane's FINAL window has
nothing below it — the lane ends there.

### Activity stats are per-segment, not per-node

Activity stats (message count, compute time) render as turn-info
lines ON the lanes — a marker sitting where the lane's bar would be,
with the text to its right — positioned BETWEEN the markers that bound
them, never on a node's own box label. The marker is `•` mid-timeline;
each lane's FINAL window leads with a terminal-outcome glyph instead:
`✓` when the lane ended well (fork merged, session `Done`), `✗` when it
dead-ended (fork discarded, session `Errored`), and `•` still while
live. The window boundaries on a node's own timeline are: its own
creation, each fork child's fork-out point, each fork child's merge-back
point (only when it actually merged — a discard doesn't inject anything
into the parent's transcript, so it isn't a boundary), and the lane's
own end. Each gap between consecutive boundaries becomes one turn-info
line describing exactly that window, emitted at the row where the
window's CLOSING event renders:

```
┌───────────────────────────┐
│ ● auth-refactor (claude)  │
└───────────────────────────┘
 • 12 msgs · 8m12s
 │                   ┌───────────────────┐
 ├─ ⑂ fork ─────────▸│ ● idea A (claude) │
 │                   └───────────────────┘
 • 5 msgs · 3m40s     ✓ 2 msgs · 1m05s
 │◂─ ↩ merge ─────────┘
 • 3 msgs · 2m00s
```

A childless node still gets exactly one window (its whole life), so
every node's activity ends up visible somewhere, not just nodes with
forks. A window's duration is the session's summed COMPUTE time within
it — the total time the session actually spent in a working state
across the window's turns — not the wall-clock gap between the
boundary events (which would count idle sitting-at-a-prompt time). The
daemon accumulates working time as session-state transitions happen
and snapshots the running total at each boundary that splits a
timeline (fork-out stamps the parent's total at fork time; merge-back
stamps it at merge time), so a window's duration is the difference
between its two boundary snapshots, and a lane's final window drains
whatever the lifetime total hasn't yet attributed. Records written
before compute-time tracking existed carry no snapshots; their windows
fall back to the wall-clock span between boundary events (such a live
lane's final window measures only up to the session's last activity,
not idle ticking toward "now"). Either way, only the window's ROW
position reflects event time on the timeline.

A window's COUNT follows the same tracked-with-fallback shape: it is
the number of actual chat messages within the window — the daemon
keeps a message-only tally per session (self-healing on load by
recounting the transcript) and snapshots it at the same fork/merge
boundaries — NOT the raw transcript-event delta, which also advances
on tool blocks, status rows, and output markers and so overstates the
conversation. Windows on records without message tallies fall back to
the raw transcript-event delta. A window with zero transcript events
in it is skipped entirely rather than rendered as an empty line —
leaving just the lane bar. Diagram rows never wrap; a too-wide diagram
clips at the preview's edge.

This is possible without any extra fetch because `SessionSummary::event_count`,
`ForkedFrom::transcript_seq`, and `ForkMerge::merged_seq` are all the same
counter (the transcript's own sequence number) — a child's
`forked_from.transcript_seq` is a precise, already-in-memory snapshot of the
parent's position at fork time, and `ForkMerge::merged_seq` (stamped by the
daemon from the parent's own `event_count` at the moment of merge) is the
same for the merge-back point. The compute-time and message-count snapshots
ride the same two records (fork stamp, merge stamp) plus lifetime totals on
the summary.
Segment math is therefore plain arithmetic over data already on
`SessionSummary`, computed fresh on every render from live session state —
never a stored/cached total.

Subagent children (spec 0014) don't stamp a parent-timeline position the
way forks do, so they don't act as boundary markers; they're simply
recursed into in place without splitting the parent's timeline.

## Reason

A session's fork/subagent lineage is a per-session fact, not a
fleet-wide or otherwise global one — asking "what's this session's lineage
shape, and can I act on it" never has a reason to open a screen-centered
dialog disconnected from the session's own pane. The original two-surface
design (read-only preview + a separate fully-interactive modal) existed
because the preview shipped first as a lighter-weight glance and the modal
predated it as the only interactive surface; once the preview existed,
maintaining two places that both render the same tree — one visually
anchored to the session, one not — was duplication with no upside. Folding
the modal's interaction vocabulary into the preview keeps exactly one
lineage UI: ambient and glanceable by default (hover), pinnable for a longer
look, and keyboard-interactive on demand without ever leaving the session's
own pane.

## Consequences

- The title bar's right cluster keeps its existing width and elements
  (widgets, harness label, close button) — this feature adds no new column
  to the cluster, only new behavior on the harness label's existing span.
- `LayoutSnapshot` gained `harness_label_hits` (populated only for sessions
  with lineage), `lineage_preview_area` (the last-rendered preview box, used
  to swallow stray clicks so they don't fall through to the pane
  underneath), and `lineage_preview_body_hit` (the rows/content area alone,
  tagged with its owning session — clicking here enters focus).
- `App` gained `lineage_preview_hover` / `lineage_preview_pinned` (mirroring
  but not reusing the session-widget hover/pin fields) plus
  `lineage_preview_focused` and its selection/scroll state for the
  keyboard-focused mode.
- There is exactly one `KeyAction` for keyboard entry into lineage
  (`C-x Tab`, both keymap profiles) instead of a dedicated fork-log-popup
  action — a single compound chord toggles the whole interactive
  experience on and off.
- A future removal of the session-widget system (spec 0003) does not need
  to touch this feature — it was built to mirror that system's shape, not
  depend on its code.
- `ForkMerge` (protocol) gained `merged_seq: u64`, stamped by the daemon
  from the parent's `event_count` at merge time — the parent-timeline
  counterpart to `ForkedFrom::transcript_seq`, and the one piece of data
  segment rendering needed that wasn't already on `SessionSummary`.
- The lineage row model gained a non-selectable `Segment` row kind,
  interleaved into the flattened row list at the correct points alongside
  node rows and the existing "+N more" collapse marker.

## Non-Goals

- Does not change what counts as fork or subagent lineage, or how the graph
  is built/capped (spec 0078 still governs that; spec 0079's tree-
  construction rules are unchanged, only its "second global dialog"
  delivery mechanism was removed).
- Does not add a docked/always-visible panel — the preview is still
  hover/pin/focus-triggered, never rendered unconditionally.
- Does not change what merge/discard or jump-in DO (spec 0078 governs
  those); it only changes where the keys that trigger them live.
- Does not attribute cost (`SessionSummary::cost_usd`) to individual
  segments — it's a single cumulative total with no per-checkpoint snapshot
  the way `event_count` has, so it was dropped from the lineage view
  entirely rather than approximated or misattributed to one window.
