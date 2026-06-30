# 0065-tui-view-mode-is-per-split-window

Status: accepted
Date: 2026-06-30
Area: tui
Scope: In the TUI split layout, the transcript/terminal view-mode toggle applies to the focused split window only; each split pane carries its own view mode.

## Decision

The TUI's "toggle view" key (transcript/chat vs live terminal) is scoped to the
**focused split window**, not the whole layout. Pressing it flips only the
active pane; sibling panes keep whatever mode they were already showing.

- View mode is tracked per split window, keyed by window id — the same shape as
  the existing per-window PTY scrollback and pane-size state. The single
  "current view" value tracks what the focused pane is showing right now, but it
  is not the mode applied to every pane when rendering.
- Absent an explicit per-window choice, a pane falls back to its session's
  natural surface: terminal for a PTY-backed session, otherwise chat.
- A pane's toggled mode is remembered across focus changes: focusing away and
  back to a pane restores the mode it was last left in (its remembered choice,
  else the natural fallback). Re-selecting a *different session* into the
  focused pane resets that pane to the new session's natural mode.
- Chat/transcript content is only rendered for a pane whose session's transcript
  is actually hydrated. Because only the focused session's transcript is held in
  memory, a non-focused pane in chat mode shows the empty-state hint rather than
  another session's transcript.

## Reason

Splits exist so the user can watch several sessions at once. A global view
toggle breaks that: flipping one pane to a transcript flips every pane, yanking
the other live terminals out from under the user. Per-window mode lets each pane
show the surface that makes sense for the session in it — one pane reading a
transcript while its neighbors keep streaming their terminals. This mirrors the
web UI's per-session view memory ([[0062-webui-view-mode-is-per-session]]),
adapted to the TUI where the unit of "where am I looking" is the split window,
not a client-persisted per-session preference.

## Consequences

- Rendering a split pane must resolve the mode for *that* pane's window, not read
  the single focused-pane mode. Any new per-pane surface must follow suit.
- Every code path that sets the focused pane's mode (toggling, selecting a
  session, hydrating, removing the selected session) must keep the per-window
  store in sync, or a pane will render one mode while its remembered mode says
  another.
- Chat rendering must guard its transcript against the pane it is drawing: it may
  only show the transcript when the hydrated transcript belongs to that pane's
  session. Rendering more sessions' transcripts at once would require hydrating
  more than the focused session's transcript, which this decision does not do.

## Non-Goals

- Persisting per-window view mode across daemon/TUI restart. The layout tree and
  selection persist; the per-window view mode is recomputed from natural mode on
  load.
- Hydrating non-focused panes' transcripts so they can render chat content. Until
  that exists, chat mode is effectively a focused-pane surface.
- Changing how splits are created, resized, deleted, or how a pane picks its
  session.
