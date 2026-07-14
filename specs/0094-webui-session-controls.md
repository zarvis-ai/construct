# 0094-webui-session-controls

Status: accepted
Date: 2026-07-14
Area: ux
Scope: Where the web UI's per-session controls live and how session reorder works.

## Decision

- The session list has no action toolbar. Reordering is direct
  manipulation on the rows themselves: mouse drag, or press-and-hold then
  drag on touch (an immediate swipe stays a scroll). A drop is replayed
  as single-step daemon moves, so region semantics — pinned block, fork
  sibling runs, group membership — stay daemon-owned and a drop stops at
  a region edge rather than jumping it.
- Rename acts on the current session from the title bar, next to the
  session name.
- The remaining lifecycle actions live in a session menu at the top-right
  of the session view (next to the terminal scroll controls), mirroring
  the TUI's session-title menu: rename, pin/unpin, fork conversation,
  restart, archive/unarchive, merge and archive (enabled only for forks,
  visible otherwise so the menu teaches the workflow), delete. TUI-only
  entries that manage split panes are omitted — the web UI has no splits.
- Client-side preferences (currently the theme) live in a settings sheet
  opened by activating the matrix-rain connection badge in the header,
  which doubles as the settings button.

## Reason

The bottom toolbar spent permanent space on rarely-used buttons, hid
which session they acted on, and reorder-by-arrow-taps was slow. Direct
manipulation, title-bar placement next to the session's name, and a
single menu matching the TUI keep both clients teaching the same model.

## Consequences

- Reorder issues single-step move requests; clients must tolerate a drop
  that partially completes at a region boundary (the daemon reports when
  nothing moved).
- New lifecycle actions belong in the session menu, not new standalone
  buttons; keep its item list aligned with the TUI menu where the action
  exists on both surfaces.
- The connection badge is an interactive control; it must stay focusable
  and keyboard-activatable.

## Non-Goals

- Cross-region drops (e.g. dragging into a different project group) are
  not reorder semantics; grouping stays a separate operation.
- The TUI's split-pane management stays TUI-only.
