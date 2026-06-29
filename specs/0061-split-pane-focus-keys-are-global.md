# 0061-split-pane-focus-keys-are-global

Status: accepted
Date: 2026-06-29
Area: tui
Scope: Keys that only move keyboard focus between the session list and split view panes take effect even while a focused pane is forwarding keystrokes to its child.

## Decision

The TUI offers fast focus-movement keys for the split layout, in addition to
the window-management prefix:

- A "cycle to the next pane" key that walks the list and every visible split
  window in layout order, wrapping around (emacs `other-window`).
- Direct "focus pane N" keys for the first several panes, where pane 1 is the
  session list and the higher numbers are the split windows in layout order.
  Asking for a pane that does not exist is a no-op.
- Directional "focus the adjacent pane" keys that move focus to the split
  window spatially neighboring the active one in a given direction (emacs
  `windmove`), choosing the closest pane whose perpendicular span overlaps the
  active pane.

These keys change *focus only*; they never send input to a child. Because their
whole purpose is to leave a pane, they must reach the TUI even when the focused
pane is forwarding keystrokes to a live child PTY — the same way the
window-management prefix already escapes PTY capture. A user must be able to
move out of a busy pane without that pane swallowing the key.

Directional focus movement is contextual: it only takes over the physical keys
when the view pane is focused in an actual multi-pane split (not zoomed). Where
that context does not hold, those same keys keep their prior meaning (e.g. list
reordering from the list pane, or normal child input in a single-pane view). A
directional move with no neighbor in that direction is a consumed no-op rather
than a fall-through.

Directional focus must also be reachable through the window-management escape
prefix (an explicit chord), not only through the bare modified-arrow keys. Many
terminals reserve some modified arrows for their own use and never deliver them
to the application — notably the vertical pair (modified Up/Down) is commonly
bound to line scrollback while the horizontal pair is forwarded — so a feature
that exists *only* as a bare modified-arrow binding silently loses one axis on
those terminals. The prefix-reached spelling is always delivered (it is the same
prefix that already escapes PTY capture), so it is the reliable path and the
bare modified-arrow form is an optimization for terminals that forward it. The
prefix form, being an explicit request, may report "no window that way" rather
than staying silent.

## Reason

Cycling is fine for two panes but slow with several; jumping straight to a pane
by number or direction is the natural way to navigate a grid. The keys are most
useful exactly when a pane is busy running an interactive child, which is also
when the child is capturing the keyboard — so if they did not escape PTY
capture they would be useless in their primary scenario, and the user would be
forced back to the escape-prefix chord they were trying to avoid.

Scoping the directional keys to a focused multi-pane split keeps them from
stealing those keystrokes from a child (or from list-reordering) when there is
no pane to move to, limiting the surface where a child program loses access to
those physical keys.

## Consequences

- Adding focus-movement keys means a small, fixed set of non-prefixed keys now
  shadow the focused child for those bindings. This is an accepted, bounded
  expansion of "which keys are global" (cf. 0055's non-goal): it is limited to
  focus movement, and the directional ones only apply inside a focused split.
- A full-screen child program in a focused split does not receive those exact
  keys while a neighbor exists; this mirrors the existing tradeoff where
  scrollback paging shadows the child's paging keys.
- The list-vs-view focus contract from [[0055-view-pane-input-requires-view-focus]]
  still holds: these keys decide *which* pane is focused; once focused, that
  surface owns its own keys.
- Pane numbering and cycle order are one shared ordering (list first, then split
  windows in layout order), so "next pane" and "pane N" agree.

## Non-Goals

- Does not change how splits are created, resized, or deleted, nor how a pane
  chooses its session ([[0039-tui-split-panes-keep-unique-sessions]]).
- Does not make any input-bearing key global; only pure focus-movement keys
  escape PTY capture.
- Does not mandate specific physical keys; it constrains their behavior, not
  their spelling.

## Examples

- Two shells side by side, typing in the left one: a single "focus right" key
  moves to the right shell without the left shell consuming the key.
- Four panes in a grid: "focus pane 3" jumps straight to the third split window;
  "focus pane 5" with only four panes does nothing.
- A focused split, pressing the "focus up" key with no pane above: nothing
  happens (the key is consumed), instead of reordering the list.
- Focus on the session list: the same directional keys reorder the selected
  session, because the focused-split context does not apply.
- Two panes stacked vertically in a terminal that scrolls on modified Up/Down:
  the bare modified-arrow never arrives, but the escape-prefix-plus-arrow chord
  still moves focus between the top and bottom panes.
