# 0037-tui-pty-keystroke-latency

Status: accepted
Date: 2026-07-10
Area: tui
Scope: Applies to the terminal TUI when forwarding user keystrokes into a PTY-backed session.

## Decision

Forwarded PTY keystrokes must keep the event loop on the shortest path back to input and PTY-output polling. When a keystroke is passed through to a child PTY and the client skips the stale immediate redraw, history hydration, pane resize debounce, and similar background maintenance must not run before the loop polls again for the child PTY echo.

The daemon must enqueue live PTY output for connected clients before performing
synchronous transcript-marker writes, PTY-log writes, or summary bookkeeping
for that output. Sequence allocation still happens before both paths, and the
durable writes still complete in the event handler before it returns.

## Reason

The visible result of a forwarded terminal keystroke comes from the child PTY's echo or redraw. Any unrelated work performed after queuing the input but before polling for PTY output adds typing-to-screen latency, which users perceive as delayed characters or a cursor that briefly stalls and catches up.

Interactive terminal repaint streams can produce many small PTY chunks per
keystroke. Making every live chunk wait behind filesystem work turns storage
latency into typing latency and allows repaint fragments to accumulate behind
the user's input.

## Consequences

Maintenance that warms history or debounces resize may be deferred by an event-loop iteration during active typing. That is acceptable because the selected terminal's input responsiveness is the higher-priority interaction. Maintenance still runs on normal redraw iterations and must remain bounded so it does not starve live input.

A connected client may briefly observe a sequence before that sequence becomes
available through durable replay. This small live-ahead window is accepted;
the handler must still persist events in sequence order and log failures.

## Non-Goals

This does not require bypassing all bookkeeping on every key event. Local TUI commands, scrollback movement, and keys that redraw client-owned UI may still update the interface immediately when their visible effect is owned by the TUI rather than the child PTY.
