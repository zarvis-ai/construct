# 0013-webui-pty-sessions-can-switch-views

Status: accepted
Date: 2026-05-31
Area: webui
Scope: Web clients may choose how to view PTY-backed sessions that have semantic transcript events.

## Decision

PTY-backed sessions open in terminal view by default. The web UI may offer a switch from terminal view to chat transcript view only for PTY sessions whose harness emits enough semantic transcript events for chat to be meaningful. Interactive harnesses that maintain native transcript files may watch those transcripts and mirror normalized message/tool/cost events into agentd while keeping the PTY as the interactive surface. Chat transcript view renders semantic transcript events and must not append raw PTY byte-stream events as chat text. Non-PTY sessions remain chat-only.

## Reason

Terminal view preserves the live interactive surface for PTY harnesses, while chat transcript view is better for reading structured messages, tool events, and long history. Raw PTY output contains cursor movement, repaint frames, and status spinners that make append-only chat unreadable. Native transcript watchers let Codex, Claude, and Antigravity expose chat-mode history without scraping their terminal output.

## Consequences

The selected view is user-facing state and may be remembered by the web client. Returning from chat view to terminal view must refresh or otherwise reconcile the terminal surface so it does not miss PTY or structured events that arrived while chat view was active. Sessions that only produce raw terminal output should remain terminal-only in the web UI.

Returning from terminal view to a previously loaded chat view should reveal the cached transcript without refetching completed history. If chat history is still backfilling, the web client should preserve the reader's viewport, pause work that depends on visible layout while chat is hidden, and resume older-history loading when chat becomes visible again.

## Non-Goals

This does not require every client to expose the same control, and it does not make headless or non-PTY sessions terminal-capable.
