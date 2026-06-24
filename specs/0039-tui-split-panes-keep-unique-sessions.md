# 0039-tui-split-panes-keep-unique-sessions

Status: accepted
Date: 2026-06-24
Area: tui
Scope: How the terminal TUI assigns sessions to split session panes.

## Decision

When a TUI split pane selects a session that is already visible in another split pane, the two panes swap selections. The active pane shows the requested session, and the other pane shows the active pane's previous selection.

If stale state has the requested session visible in multiple other panes, the swap target is deterministic: the first matching other pane in layout order. Explicit split creation may initially duplicate the current pane because no alternate session was requested yet.

## Reason

Split session view is most useful when panes preserve distinct context. Accidentally showing the same session in multiple panes makes focus and list selection harder to reason about, especially when the user expected to move a visible session from one pane to another.

Swapping preserves all visible context instead of discarding one pane's prior selection, and it makes the action reversible by selecting the same visible session again.

## Consequences

Selection paths that target a session from the list, switch-session picker, or other shared selection flows should maintain the same swap behavior. A pane that receives the swapped-in prior selection should reset view-local scrollback because it is now showing different content.

Future UI affordances that intentionally duplicate a session into another split should be explicit and separate from normal session selection.

## Non-Goals

This does not require split creation to choose a different session automatically. It also does not prevent pinned previews, browser clients, or other non-split surfaces from showing the same session.
