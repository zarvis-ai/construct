# 0040-mouse-events-forward-to-tracking-child

Status: accepted
Date: 2026-06-25
Area: tui
Scope: When the cursor is over a pane whose PTY child has requested mouse tracking, the client forwards mouse events into that child instead of acting on them itself.

## Decision

The client is the terminal for every child PTY, so it owns the physical mouse. When a mouse event occurs over the content area of a pane whose child currently has a mouse-tracking mode enabled (DEC private mode `9`/`1000`/`1002`/`1003`), the client translates the event into the report that child expects and writes it down the PTY — instead of consuming the event for its own scrollback, focus, or text selection.

The report must honor both the child's tracking **mode** (which events are reportable: presses only, presses+releases, button-motion, any-motion; the wheel is always reported when any mode is on) and its **encoding** (legacy single-byte, UTF-8 `1005`, or SGR `1006`). Coordinates are reported relative to the child's screen — the pane's content rectangle with borders excluded, 1-based — not the outer terminal.

The client tracks the child's mouse mode and encoding from the same byte stream it renders, exactly as it tracks bracketed-paste mode (see [0034](0034-forwarded-pastes-honor-child-bracketed-paste-mode.md)). A child that never enables a mouse mode keeps the client's own mouse handling (wheel scrollback, click-to-focus, drag-to-select). Forwarding is also suppressed while a client-owned drag gesture (pane/list/scrollbar resize, an in-progress text selection) is mid-flight, so those gestures finish under the client's control.

Forwarding is likewise suppressed while a client-owned overlay that paints over pane content is open (e.g. the session-title actions menu, the canvas popup). Such an overlay is a transient modal surface the user is interacting with, so its rows take mouse priority over the child underneath — otherwise a mouse-grabbing child swallows every overlay click and its actions silently do nothing. The overlay's own trigger lives on the pane border (excluded from the content rectangle), so opening it is never forwarded either.

A button-press event is the one gesture the client acts on **in addition to** forwarding it: pressing a mouse button over a pane's content area moves the client's keyboard focus to that pane before the report is written down the PTY. Focus is a client-side concern the child cannot observe, so the report the child receives is identical either way — but without it a click inside a mouse-grabbing child reaches the child while the client's focus (and therefore the keyboard) stays on whatever pane it was on, which no user expects. Wheel and motion events are forwarded without changing focus.

## Reason

A child that turns on mouse tracking — Claude Code's fullscreen mode is the motivating case — is explicitly asking to handle scroll, clicks, and selection itself. The client sat between the user's terminal and the child and silently ate every mouse event: the wheel only moved the client's own scrollback and never reached the child, so scrolling did nothing inside a fullscreen child, and clicks could not reach its menus or tool-block toggles. Forwarding restores parity with running the harness directly in a terminal, where the wheel and clicks reach whatever app has grabbed the mouse.

## Consequences

- The client must keep a live view of each child's mouse mode and encoding, derived from the rendered byte stream rather than guessed. Sessions with no live parser yet, and synth/chat sessions that never run a real terminal child, report no mouse mode and keep the client's own handling.
- While a child holds the mouse, the client's own in-pane gestures over that pane are deferred to the child: the wheel scrolls the child rather than the client's scrollback, and drag-to-select no longer originates in that pane. Native terminal selection then requires the user's terminal modifier (Fn / Option / Shift, per the terminal), the same as any full-screen mouse-grabbing app. Click-to-focus is the exception — a button press still focuses the pane (see the Decision) and then forwards, so the pane the user clicked is the one receiving keystrokes.
- Events the active mode does not report (e.g. plain motion under press/release mode) are not forwarded; the client keeps its own handling of those rather than swallowing them.
- Coordinates are mapped to the pane's content area, so divider and frame clicks (on borders, outside the content rectangle) continue to drive the client's resize/focus logic and are never forwarded.
- A client-owned overlay open over a pane (session-title menu, canvas popup) takes mouse priority over the child beneath it. While such an overlay is open the client handles the mouse itself, so the overlay's actions stay clickable even when the pane's harness has grabbed the mouse.

## Non-Goals

- This does not change how keyboard input or pastes are encoded and forwarded.
- The client does not synthesize or alter the mouse *report* (no scroll acceleration, no extra synthetic reports); the bytes it writes down the PTY are a faithful pass-through. The only client-side action layered on top is moving keyboard focus on a button press — which the child cannot observe and which leaves the report untouched.
- Panes whose child does not enable mouse tracking are unaffected — the client's wheel-scrollback and selection behavior there is unchanged.
