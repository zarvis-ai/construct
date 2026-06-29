# 0058-program-view-position-survives-hide-show

Status: accepted
Date: 2026-06-28
Area: tui
Scope: The Program view restores its caret and scroll position when hidden and shown again.

## Decision

Hiding the Program view and showing it again for the same session must land on
the exact caret offset and scroll position the user left. Hide→show is
position-preserving, not a reset to the top of the document.

This holds for every hide path (the title-bar toggle, the close action, and
dismissing the modal) followed by reopening the same session's program.

## Reason

The Program view's caret and scroll are ephemeral UI state held only by the
active popup. When the program is hidden the popup is dropped, and reopening
re-fetches the document fresh from the daemon — which has no caret or scroll, so
a naive rebuild starts at offset 0. A user who scrolls or clicks deep into a
program, hides it to glance at the terminal, and reopens it expects to resume
where they were, not to be thrown back to the top.

## Consequences

- A hidden program's caret + scroll must be captured before the popup is
  dropped, and reapplied onto the freshly-loaded popup on reopen.
- This remembered position is kept separate from the map that drives split-window
  rendering of non-active programs. A fully-hidden program must not reappear in a
  split pane just because its position is remembered; conversely, remembering the
  position must not depend on the program still rendering anywhere.
- The remembered caret is a char offset into a document that may have changed on
  the daemon while hidden, so it is clamped to the current buffer on restore; an
  out-of-range scroll offset is clamped by the renderer. Restoration therefore
  never points past the end of the content.
- Switching the selected session between programs is a separate mechanism that
  already preserves full popup state (including unsaved edits); this decision is
  specifically about the hide→show cycle of a single session's program.

## Non-Goals

- Persisting caret/scroll across a daemon restart or client relaunch. Only the
  set of open programs is persisted; their in-view position is in-memory only.
- Preserving an explicit selection/highlight across hide→show — only caret,
  preferred column, and scroll are remembered.
