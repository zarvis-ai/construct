# 0106-session-list-view-modes

Status: accepted
Date: 2026-07-21
Area: tui
Scope: The sidebar session list offers a compact one-line view and a full two-line card view per session, toggled from the pane's border and persisted per user.

## Decision

The session list renders in one of two user-selectable view modes:

- **Compact** (the default): one line per session — lineage/pin markers, the
  status glyph, the session name, an attention marker, and the right-aligned
  harness label.
- **Full**: the compact line plus a muted second detail line aligned under the
  name, showing (in display order) the model and reasoning effort, a small
  context-window gauge with percentage, current activity (live busy time while
  running; otherwise a coarse age since the last chat message, so status
  rows, tool blocks, and daemon-restart resume events do not reset it —
  sessions with no messages fall back to the last recorded event), and
  lifetime token
  volume. Cost is deliberately excluded. The gauge's fill rounds to the
  nearest step so the bar tracks the percentage (just over half reads as
  half, not three quarters).

Rules both modes must preserve:

- The toggle is a small labeled control on the list pane's border,
  right-aligned immediately before the pane's collapse control — the same
  placement and label shape as the lineage section's full/compact toggle. The
  choice persists across launches; legacy state restores to compact.
- The detail line only ever shows data the session actually reported — absent
  fields are omitted, never rendered as placeholders. A session reporting no
  model/usage data at all (e.g. a plain shell) falls back to showing where it
  lives (its worktree or working directory).
- On a narrow sidebar the detail line drops segments rather than wrapping,
  least important first: tokens, then activity, then model, keeping the
  context gauge longest. Full mode never forces the sidebar wider and never
  horizontally scrolls.
- Group headers and archived-disclosure rows stay one line in both modes.
- The web UI's session list shows the same detail line with the same
  content and omission/fallback rules, but always on — it has no
  compact/full mode pair. Its gauge may render at finer resolution than
  the TUI's cell bar (a continuous fill), since the constraint being
  mirrored is the information and its semantics, not the glyphs.
- Selection, keyboard navigation, and scrolling operate on items, not display
  rows; a click anywhere within a card selects it, while gutter affordances
  (disclosure triangle, pin target) live on the card's first line only.
  Hit-testing must consult the rendered row-to-item mapping, never assume one
  display row per item.

## Reason

The compact list answers "which session" but not "how is it doing" — model,
context pressure, activity, and spend previously required selecting each
session and reading the modeline. Scanning a fleet benefits from a denser
per-session summary, but permanently taller rows would halve the visible
session count, so the density is a user choice, mirroring the precedent the
lineage section set for a full/compact pair.

## Consequences

- Display rows and list items are no longer 1:1; any future list interaction
  (hover zones, drag targets, new gutter affordances) must go through the
  row-to-item mapping and declare which line of a card it lives on.
- Scroll limits and scrollbar geometry are measured in display rows, so
  mixed-height items (one-line headers among two-line cards) stay correct.
- Adding new per-session data to the detail line means placing it in the
  existing drop-priority order, not appending unconditionally.

## Non-Goals

- No third, denser-still or taller-still mode; two modes keep the toggle a
  binary.
- A web-UI mode switch: the web list always shows the detail line.
- The detail line is a summary, not a control surface — it hosts no buttons.
