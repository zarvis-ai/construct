# 0040-mouse-events-forward-to-tracking-child

Status: accepted
Date: 2026-06-25
Area: tui
Scope: When the cursor is over a pane whose PTY child has requested mouse tracking, the client forwards mouse events into that child instead of acting on them itself.

## Decision

The client is the terminal for every child PTY, so it owns the physical mouse. When a mouse event occurs over the content area of a pane whose child currently has a mouse-tracking mode enabled (DEC private mode `9`/`1000`/`1002`/`1003`), the client translates the event into the report that child expects and writes it down the PTY — instead of consuming the event for its own scrollback, focus, or text selection.

The report must honor both the child's tracking **mode** (which events are reportable: presses only, presses+releases, button-motion, any-motion; the wheel is always reported when any mode is on) and its **encoding** (legacy single-byte, UTF-8 `1005`, or SGR `1006`). Coordinates are reported relative to the child's screen — the pane's content rectangle with borders excluded, 1-based — not the outer terminal.

The client tracks the child's mouse mode and encoding from the same byte stream it renders, exactly as it tracks bracketed-paste mode (see [0034](0034-forwarded-pastes-honor-child-bracketed-paste-mode.md)). A child that never enables a mouse mode keeps the client's own mouse handling (wheel scrollback, click-to-focus, drag-to-select). Forwarding is also suppressed while a client-owned drag gesture (pane/list/scrollbar resize, an in-progress text selection) is mid-flight, so those gestures finish under the client's control.

## Reason

A child that turns on mouse tracking — Claude Code's fullscreen mode is the motivating case — is explicitly asking to handle scroll, clicks, and selection itself. The client sat between the user's terminal and the child and silently ate every mouse event: the wheel only moved the client's own scrollback and never reached the child, so scrolling did nothing inside a fullscreen child, and clicks could not reach its menus or tool-block toggles. Forwarding restores parity with running the harness directly in a terminal, where the wheel and clicks reach whatever app has grabbed the mouse.

## Consequences

- The client must keep a live view of each child's mouse mode and encoding, derived from the rendered byte stream rather than guessed. Sessions with no live parser yet, and synth/chat sessions that never run a real terminal child, report no mouse mode and keep the client's own handling.
- While a child holds the mouse, the client's own in-pane gestures over that pane are deferred to the child: the wheel scrolls the child rather than the client's scrollback, click-to-focus and drag-to-select no longer originate in that pane. Native terminal selection then requires the user's terminal modifier (Fn / Option / Shift, per the terminal), the same as any full-screen mouse-grabbing app.
- Events the active mode does not report (e.g. plain motion under press/release mode) are not forwarded; the client keeps its own handling of those rather than swallowing them.
- Coordinates are mapped to the pane's content area, so divider and frame clicks (on borders, outside the content rectangle) continue to drive the client's resize/focus logic and are never forwarded.

## Non-Goals

- This does not change how keyboard input or pastes are encoded and forwarded.
- The client does not synthesize or alter mouse semantics (no scroll acceleration, no click-to-focus *and then* forward); it is a faithful pass-through for panes whose child owns the mouse.
- Panes whose child does not enable mouse tracking are unaffected — the client's wheel-scrollback and selection behavior there is unchanged.
