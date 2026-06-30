# 0064-chat-view-preserves-message-newlines

Status: accepted
Date: 2026-06-30
Area: tui
Scope: The structured-event chat view must render multi-line model output with its line breaks intact.

## Decision

When the TUI renders a session's conversation from structured events (the
"chat" / transcript-inspection view used for non-PTY and headless sessions, as
opposed to the terminal-emulator view), assistant, user, and reasoning prose
must keep its newlines. Every `\n` in a message body starts a new visual line.
Streaming deltas that are folded onto an in-progress message block must apply
the same split, so a newline arriving mid-stream still breaks the line.

## Reason

The chat view builds the rich-text widget's line list directly from event text.
The widget's word-wrapper treats a bare `\n` as ordinary whitespace and only
breaks lines on width — it does **not** split a styled run on embedded
newlines. So a multi-line model message placed into a single line collapses
onto one wrapped row: paragraphs, lists, and code blocks all run together into
"jam-packed" text. The terminal-emulator view does not have this problem
because its synthesized byte stream normalizes `\n` to CRLF before the emulator
advances a row; the chat view needs the equivalent step at the line-list level.

## Consequences

- Any code that converts message/reasoning event text into rich-text lines must
  split on `\n` rather than emitting one line per event. A leading prefix (a
  timestamp or role label) stays on the first segment's line; later segments
  begin their own lines.
- Width-based wrapping stays the widget's job; this rule only restores the hard
  newlines the widget would otherwise swallow.
- Single-line event bodies (tool calls, results, status, cost) are unaffected;
  they are already flattened to one line before display.

## Non-Goals

- This does not change which view a session opens in, nor the terminal-emulator
  rendering path (that path already preserves newlines via CRLF normalization).
- No markdown layout is implied — only literal newline preservation.
