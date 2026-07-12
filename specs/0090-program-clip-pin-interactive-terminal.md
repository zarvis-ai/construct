# 0090-program-clip-pin-interactive-terminal

Status: accepted
Date: 2026-07-12
Area: tui
Scope: Single-clicking a Program `@{session:…}` clip pins its hover-preview card into a live, keyboard-focused terminal so the user can interact with that session without leaving the Program doc; double-clicking navigates to the full session view, as every click did before this existed.

## Decision

Clicking a Program `@{session:…}` smart-clip chip has two distinct gestures, distinguished by timing:

- **Single click** toggles the clip's hover-preview card **pinned**. A pinned card stops disappearing when the pointer moves away, gains a focused border and a visible cursor, and captures keyboard input: keystrokes forward as raw PTY bytes to the pinned session, not to the Program's own markdown editor. Clicking the same pinned clip again unpins it. Clicking a different clip while one is pinned switches the pin to it. `Esc` is deliberately NOT an unpin key: sessions need it (interrupting a harness mid-turn, dismissing its menus), so it forwards to the pinned session like any other keystroke — unpinning is strictly a mouse gesture (click the clip again, or click outside the card). This is how a user answers an in-flight verb session's questions (spec 0089's `interview`, most saliently) without leaving the Program doc view.
- **Double click** (a second click on the *same* clip within a short window) navigates to the clip's session as a full view — unchanged from the click behavior that existed before pinning did.

A pinned card renders anchored to the clip's on-screen position (not the pointer), and continues to do so as long as the clip is visible; scrolling it off-screen hides the card without clearing the pin, so scrolling back re-shows it.

### Size ownership

Whether the pinned card may render the session *at the card's own size* is a question of who else is showing that session:

- **Owned mode (the common case).** When the pinned session is visible nowhere else on screen — no main-window pane, not the orchestrator panel, not a pin-strip tile — the pin **takes size ownership**: the session's PTY is resized to the card's content dims, so the harness itself reflows to fit and the card renders full-fidelity, crop-free, exactly the way the session would look in a dedicated terminal of that size. Ownership is released — the PTY resized back to the standard pane size — the moment the pin does not stand anymore: unpin, pin switch, dismissal, or the Program popup closing. Verb sessions (spec 0089) are unviewed by construction, so interviews get owned mode essentially always.
- **Crop mode (the guarded case).** When the session *is* visible elsewhere, the card must not fight that render site for the session's size (the shared-parser discipline of spec `0025`): it stays a fixed-size **crop** of the session's existing cached viewport, and the wheel pans the crop window as described below.

At most one render site ever owns a session's size. This supersedes the earlier absolute rule that a pinned card never renders at its own dimensions — that rule's reason (parser thrash and visible reflow when two sites disagree) only applies when a second site exists, which is exactly what the ownership condition checks.

### Card geometry: user-resizable and movable

The card's content size defaults to a standard-terminal-ish width (wider than the transient hover preview historically was) and is **user-adjustable by border drag**: grabbing the right or bottom border resizes the card (clamped between a small minimum and the Program modal), and grabbing the top border — the title bar — moves the card anywhere inside the modal. The chosen *size* is sticky across pin switches within the popup (a size preference); the chosen *position* resets when the pin changes (position is contextual to the clip the card was moved away from). While a size-owning pin is resized, its PTY follows the card's final dims on drag release — one reflow, not one per pointer step. Border drags are card-local gestures: like every other construct drag they own the mouse until release, and they are never forwarded into the pinned session.

Because a crop-mode card is a fixed-size **crop** of the session's viewport, content can extend beyond the window on both axes. The mouse wheel over the pinned card therefore **pans the crop** rather than scrolling the Program doc: vertical wheel steps the window back from (and forward toward) the live tail it is anchored to, clamping at the content's top; horizontal wheel events — or Shift- or Alt-modified vertical wheel — pan across the screen's width, clamping at its right edge. Horizontal panning follows the gesture as the terminal encodes it, which is the opposite of the raw event names: terminals normalize vertical wheel events to scroll intent ("up" always means "look back") but deliver horizontal deltas gesture-encoded. Horizontal panning also uses a coarser step than vertical — the hidden width is several times the hidden height, and a fine step is impractically slow on a discrete wheel. The keyboard pan below is direction-literal: the arrow points where the crop window moves. Because terminals are unreliable here (many never report Shift-modified wheels to the application at all, reserving Shift for native selection, and horizontal wheel reporting varies by terminal and input device), **Shift+arrows while pinned are the guaranteed keyboard pan** with the same orientation, and are the only keys a pinned card does not forward to the session — everything else, `Esc` included, reaches the session as raw bytes. The pan is card-local state: it is never forwarded to the pinned session as input, it never resizes the shared parser (the crop window moves; the parser doesn't), and it resets whenever the pin changes or clears so one pin's pan never bleeds into the next. Wheel events outside the card keep scrolling the Program doc and leave the pin and its pan untouched.

Dismissal follows floating-overlay convention: a left click that lands **neither on the pinned card nor on a session clip dismisses the pin**, and then proceeds with the effect that click always had (placing the caret, activating a control, focusing another pane) — whether it lands in the Program body, on Program chrome, or outside the Program modal entirely. A click *on* the pinned card is consumed by the card: it never forwards into the pinned session (keyboard-only scope, see Reason), never leaks through to the Program text beneath the card, and does not dismiss the pin; it does reclaim keyboard focus for the card, so a user who clicked away into another pane can click the card to resume typing into the pinned session. Clicks on clips stay the pin's own toggle/switch gestures described above.

## Reason

A verb session (spec 0089) may need the user's input mid-run — the `interview` verb is built around exactly this. Before this, the only way to answer was to fully navigate to that session, losing your place in the Program doc. A raw double-click-only affordance existed for navigation but nothing let the user interact in place.

The click gesture was chosen over a separate keybinding or button because it reuses the one interaction surface a clip chip already has (a click), and reserving single-click for the *lower-friction, more reversible* action (pin, i.e. glance and possibly answer) while promoting the existing navigate behavior to double-click (a deliberate, higher-commitment action to leave the doc) matches how the two actions actually differ in cost. Crossterm has no native double-click event; this timing is owned by the client.

A floating, pin-and-focus overlay was tried once before for a different surface (session-list hover, superseded lineage-preview design) and was abandoned partly over mouse-passthrough conflicts with a PTY child that itself wants mouse tracking. This spec avoids that failure mode by scope: a pinned card forwards **keyboard** input only; it never attempts to forward mouse events into the pinned session, so a pinned card's own clicks/hover never need to arbitrate with a child TUI's mouse grab. This is why the affordance is well suited to short, keyboard-only Q&A (an `interview` verb turn) and not to operating a mouse-driven full-screen app through the card.

Cropping the session's existing cached viewport (crop mode) rather than replaying at the card's own size is the same reason the plain hover card and pin-strip tiles already do this (spec `0025`): the underlying terminal parser is shared across every render site for a session, and replaying it at a second, differently-sized dimension on every frame is measured to be dramatically more expensive than a same-size no-op, and would visibly reflow the session anywhere else it's also shown. Owned mode is not an exception to that reasoning but its complement: when no second render site exists, resizing the PTY itself (and rendering at exactly that size) has no one to fight and no thrash to cause, and a crop of a large viewport is strictly worse for actually *reading* a session — the interesting region (a prompt, an input box) is frequently outside the window, which is what motivated ownership in the first place.

## Consequences

- A Program click handler must distinguish single- from double-click by tracking `(clip session id, click time)` and comparing against a fixed window; a second click on a *different* clip is never a double-click regardless of timing.
- Keyboard routing must check for a pinned clip before the Program markdown editor's own key handling, and must forward to the pinned clip's session id specifically — not whatever session happens to be selected in the sidebar, which is typically a different session (the Program-owning one).
- A **crop-mode** pinned card must never trigger a resize of the shared session parser; it stays a crop of whatever size the session is already cached at, matching the existing hover-card/pin-tile discipline. An **owned-mode** card resizes the PTY (not merely the parser) and renders at exactly the owned dims — the card and PTY must always agree, or the parser would resize every frame.
- Ownership must be released on every pin-release path — unpin, switch, dismissal, popup close/replace — by resizing the session back to the standard pane size; a session must never be stranded card-sized after its pin is gone.
- A pinned card forwards keyboard bytes only. Mouse events over a pinned card are not forwarded to the pinned session; this scope limit is deliberate (see Reason), not an oversight to fix later without reconsidering the tradeoff. Border drags (resize/move) are likewise card-local.
- Click-outside dismissal requires the client to remember the card's painted bounds from the last frame; when the pinned clip is scrolled off-screen (no card painted), any non-clip click still dismisses the pin.
- This supersedes spec `0060`'s Non-Goal "does not make the preview persistent, pinned, or interactive" for the **clip-chip** hover card specifically. That Non-Goal still holds for the **shimmer-text** hover card (the dispatching session's own preview) — this spec does not touch that affordance.

## Non-Goals

- Forwarding mouse events (clicks, drags, scroll) into a pinned session — keyboard only, see Reason. Wheel over the card pans the card's own crop window, and border drags adjust the card itself; neither is delivered to the session.
- Web UI parity — this is TUI-only for now; the web Program view's clip hover is unaffected.
- Taking size ownership of a session that is visible anywhere else on screen — crop mode is the only sanctioned behavior there.
- Changing the shimmer-text hover card (spec `0060`) in any way; it remains a transient, unpinnable preview of the dispatching session.

## Examples

A user runs the `interview` verb on a vague section. The verb session's clip appears next to the selection, shimmering. The user single-clicks the clip: its hover card pins open with a focused border, showing the verb's first question. The user types an answer and presses Enter — the keystrokes went to the verb session's PTY, not the Program buffer. The verb asks a follow-up; the user answers again, still without leaving the Program doc. When done, the user clicks anywhere else in the doc to unpin (the click also lands there, e.g. placing the caret), clicks the clip again, or double-clicks the clip to jump to the full session view instead. Pressing Esc mid-interview interrupts the verb session itself — it forwards like every other key.

A user single-clicks a clip to glance at a worker's pinned output, then clicks a different clip elsewhere in the doc — the pin follows to the new clip; the first one's card closes.
