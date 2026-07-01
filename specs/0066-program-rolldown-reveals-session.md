# 0066-program-rolldown-reveals-session

Status: accepted
Date: 2026-07-01
Area: tui
Scope: The active Program view rests partway down its pane instead of covering it entirely, and keyboard/mouse focus can hand off to the revealed session underneath.

## Decision

The active Program view is top-anchored and, at rest, covers only part of its
pane — by default enough that roughly a third of the pane stays visible
underneath as the session's own terminal/chat view. The user can drag the
Program's bottom border to change how much of the pane it covers; the program
never grows to fully re-cover the pane by dragging (a small sliver of the
session view always stays reachable), and it never shrinks below a usable
minimum (border + title + one content row).

Clicking inside the revealed session strip does not roll the Program up or
close it. It hands keyboard focus to the session underneath — the user can
type into that session's terminal/chat exactly as if the Program weren't
open — while the Program stays visible at its current height. Clicking back
into the Program's own body reclaims keyboard focus for the Program editor.
While the session holds focus this way, the Program's frame dims (the same
visual language used for an unfocused split pane), signaling that keystrokes
are no longer going to it.

Background Program popups shown in non-active split windows (a passive
preview of another pane's program, not the one currently being edited) are
unaffected: they keep covering their pane fully, as before. Only the active
Program rolls down and is drag-resizable.

The Program's hide/show toggle (title-glyph click, `C-x Space`) is unrelated
and unchanged: it fully removes the Program from view. Rolling down is the
Program's normal, visible resting state; hiding is a separate, explicit
dismissal.

## Reason

The Program view previously covered its entire pane whenever open, so working
in the underlying session (reading its output, typing into its terminal)
required fully closing the Program first. That's disruptive for the common
case of directing a session's work from its Program while still wanting to
glance at or interact with the session directly. Rolling down by default keeps
the session reachable without any extra step, and the drag handle lets a user
who wants more of one or the other adjust the split to taste, the same way
list width, pin-strip height, and the matrix-rain panel are already
user-adjustable via border drags.

## Consequences

- Mouse hit-testing for "is this click on the Program" must use the Program's
  actual rendered rect, not the full pane rect — a click below the Program's
  bottom border is a click on the session, not on the Program, even though
  both live in the same pane.
- A boolean "session terminal has focus while Program is open" state is
  needed independent of the existing List/View pane focus, since both "typing
  into the Program" and "typing into the session revealed beneath it" are
  `View`-pane-focused from the outer split's perspective.
- The user's preferred Program cover height persists across launches, keyed
  like the other adjustable panel sizes (pin strip, matrix rain, orchestrator
  panel) — a stale value from a since-resized terminal is re-clamped against
  the pane's current height at render time rather than trusted verbatim.
- Any future Program chrome (new title-bar controls, new body affordances)
  that assumes the Program always fills its pane must instead reason about
  the smaller, user-adjustable rect.

## Non-Goals

- This does not change the Program's hide/show (fully dismissed) behavior or
  its animation timing.
- This does not make background/inactive split-window Program previews
  drag-resizable or roll them down; they still cover their pane fully.
- This does not add a fraction/percentage-based resize model — the cover
  height is stored as an absolute row count, matching the existing pin-strip /
  matrix-rain / orchestrator-panel convention, re-clamped to the current pane
  size at render time.

## Examples

- Opening a Program on a 30-row pane rests with the Program covering the top
  ~20 rows, leaving the bottom ~10 rows showing the session's own terminal.
- Dragging the Program's bottom border down by 5 rows grows it to ~25 rows
  covered / ~5 revealed; dragging it up shrinks it back down, but never below
  a small usable minimum and never so far down that the session view
  disappears entirely.
- Clicking in the revealed strip and typing sends keystrokes to the session,
  not the Program's Markdown buffer; the Program's border dims while this
  holds. Clicking back into the Program's body restores its normal border
  color and keyboard focus.
