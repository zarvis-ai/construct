# 0088-program-clip-pin-interactive-terminal

Status: accepted
Date: 2026-07-12
Area: tui
Scope: Single-clicking a Program `@{session:…}` clip pins its hover-preview card into a live, keyboard-focused terminal so the user can interact with that session without leaving the Program doc; double-clicking navigates to the full session view, as every click did before this existed.

## Decision

Clicking a Program `@{session:…}` smart-clip chip has two distinct gestures, distinguished by timing:

- **Single click** toggles the clip's hover-preview card **pinned**. A pinned card stops disappearing when the pointer moves away, gains a focused border and a visible cursor, and captures keyboard input: keystrokes forward as raw PTY bytes to the pinned session, not to the Program's own markdown editor. Clicking the same pinned clip again unpins it. Clicking a different clip while one is pinned switches the pin to it. `Esc` unpins without forwarding the keystroke. This is how a user answers an in-flight verb session's questions (spec 0087's `interview`, most saliently) without leaving the Program doc view.
- **Double click** (a second click on the *same* clip within a short window) navigates to the clip's session as a full view — unchanged from the click behavior that existed before pinning did.

A pinned card renders anchored to the clip's on-screen position (not the pointer), and continues to do so as long as the clip is visible; scrolling it off-screen hides the card without clearing the pin, so scrolling back re-shows it. It does not own its own terminal size: like the plain hover card, it crops the session's existing cached viewport rather than replaying at its own dimensions, so it never fights the main/split view over the shared parser's size (see Consequences).

## Reason

A verb session (spec 0087) may need the user's input mid-run — the `interview` verb is built around exactly this. Before this, the only way to answer was to fully navigate to that session, losing your place in the Program doc. A raw double-click-only affordance existed for navigation but nothing let the user interact in place.

The click gesture was chosen over a separate keybinding or button because it reuses the one interaction surface a clip chip already has (a click), and reserving single-click for the *lower-friction, more reversible* action (pin, i.e. glance and possibly answer) while promoting the existing navigate behavior to double-click (a deliberate, higher-commitment action to leave the doc) matches how the two actions actually differ in cost. Crossterm has no native double-click event; this timing is owned by the client.

A floating, pin-and-focus overlay was tried once before for a different surface (session-list hover, superseded lineage-preview design) and was abandoned partly over mouse-passthrough conflicts with a PTY child that itself wants mouse tracking. This spec avoids that failure mode by scope: a pinned card forwards **keyboard** input only; it never attempts to forward mouse events into the pinned session, so a pinned card's own clicks/hover never need to arbitrate with a child TUI's mouse grab. This is why the affordance is well suited to short, keyboard-only Q&A (an `interview` verb turn) and not to operating a mouse-driven full-screen app through the card.

Cropping the session's existing cached viewport rather than replaying at the card's own size is the same reason the plain hover card and pin-strip tiles already do this (spec `0025`): the underlying terminal parser is shared across every render site for a session, and replaying it at a second, differently-sized dimension on every frame is measured to be dramatically more expensive than a same-size no-op, and would visibly reflow the session anywhere else it's also shown.

## Consequences

- A Program click handler must distinguish single- from double-click by tracking `(clip session id, click time)` and comparing against a fixed window; a second click on a *different* clip is never a double-click regardless of timing.
- Keyboard routing must check for a pinned clip before the Program markdown editor's own key handling, and must forward to the pinned clip's session id specifically — not whatever session happens to be selected in the sidebar, which is typically a different session (the Program-owning one).
- A pinned card must never grow its own terminal size or trigger a resize of the shared session parser; it stays a crop of whatever size the session is already cached at, matching the existing hover-card/pin-tile discipline.
- A pinned card forwards keyboard bytes only. Mouse events over a pinned card are not forwarded to the pinned session; this scope limit is deliberate (see Reason), not an oversight to fix later without reconsidering the tradeoff.
- This supersedes spec `0060`'s Non-Goal "does not make the preview persistent, pinned, or interactive" for the **clip-chip** hover card specifically. That Non-Goal still holds for the **shimmer-text** hover card (the dispatching session's own preview) — this spec does not touch that affordance.

## Non-Goals

- Forwarding mouse events (clicks, drags, scroll) into a pinned session — keyboard only, see Reason.
- Web UI parity — this is TUI-only for now; the web Program view's clip hover is unaffected.
- Giving the pinned card its own, larger, or resizable terminal size independent of the session's existing cached viewport.
- Changing the shimmer-text hover card (spec `0060`) in any way; it remains a transient, unpinnable preview of the dispatching session.

## Examples

A user runs the `interview` verb on a vague section. The verb session's clip appears next to the selection, shimmering. The user single-clicks the clip: its hover card pins open with a focused border, showing the verb's first question. The user types an answer and presses Enter — the keystrokes went to the verb session's PTY, not the Program buffer. The verb asks a follow-up; the user answers again, still without leaving the Program doc. When done, the user presses Esc to unpin, or double-clicks the clip to jump to the full session view instead.

A user single-clicks a clip to glance at a worker's pinned output, then clicks a different clip elsewhere in the doc — the pin follows to the new clip; the first one's card closes.
