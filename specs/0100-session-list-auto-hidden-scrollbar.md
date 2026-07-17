# 0100-session-list-auto-hidden-scrollbar

Status: accepted
Date: 2026-07-17
Area: tui
Scope: An overflowing TUI session list exposes its scroll position without adding persistent visual chrome.

## Decision

When the session rows exceed their viewport, the TUI renders a vertical
scrollbar over the rows' rightmost column only while the session rows have
active keyboard focus or the pointer is over the session-list header or rows.
The scrollbar hides when neither condition is true. Hovering the lineage or
operator regions below the rows does not reveal the session-list scrollbar.

The scrollbar is a slim right-edge overlay: it does not reserve a column,
reflow row labels, or change the list's geometry when it appears. Its thumb
and track retain a full-cell mouse target for dragging and jumping, while the
existing mouse-wheel and keyboard scrolling behavior remains unchanged.

## Reason

A long session list already scrolls, but without a position indicator it is
hard to tell that more sessions exist or where the current viewport sits.
Keeping the indicator auto-hidden preserves the sidebar's low-chrome resting
state and matches the lineage view's scrollbar semantics.

## Consequences

- Future session-row layout changes must preserve the overlay behavior and
  must not subtract a content column for the scrollbar.
- Focus means active session-row focus, not dormant sidebar sub-focus while
  lineage or another pane owns input.
- Scrollbar drag state keeps the bar visible until mouse-up even if the pointer
  temporarily leaves the rows.

## Non-Goals

- No horizontal session-list scrolling.
- No persisted session-list scrollbar visibility state.
