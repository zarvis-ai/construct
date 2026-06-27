# 0045-pane-title-cluster-and-widget-reveal

Status: accepted
Date: 2026-06-27
Area: tui
Scope: Title-bar frame elements take the host pane's border color, and sticky widgets reveal over whatever surface is currently on top.

## Decision

The shared title-bar right cluster (widget indicators, harness label, and the dash that stitches the widget square into the top border) must render in the **border color of the pane it sits on**, not a fixed one. The session view's cluster uses the session view's focus-aware border color; the canvas's cluster uses the canvas border color. The two title bars share one builder, and that builder is given the pane's border style rather than re-deriving a single hardcoded style.

A session's sticky widgets (hover preview or pinned) must be revealed on top of **whatever surface currently occupies the pane** — the plain session view and the canvas alike. An overlay that paints over the session view (such as the canvas) is responsible for re-rendering the visible/pinned widgets on top of itself; it cannot rely on the body the session view drew underneath, because the overlay clears it.

## Reason

The widget indicator and harness label are meant to read as part of the title bar's frame, so they must match the frame they are attached to. When the cluster was hardcoded to the session view's (green) border color, the canvas — whose border is a distinct accent color — showed a stray mismatched dash beside the widget square.

Widget reveal is armed by the title-bar squares regardless of which surface is shown, so a user hovering or pinning a widget while an overlay is open expects to see the widget. If the overlay clears the underlying reveal and does not redraw it, the affordance silently does nothing.

## Consequences

- A single builder produces both title bars' right clusters; callers pass the pane's border style. New surfaces that reuse the cluster must pass their own border style.
- Any full-pane overlay that hides the session view and offers the widget-square affordance must also re-render the visible/pinned widgets itself.
- The reveal renderer is keyed by an explicit session id (not "the selected session") so it can serve both the session view and an overlay bound to a specific session.

## Non-Goals

- Does not change which widgets count as visible (hover grace, pinning, temporary reveal) — only where they are drawn.
- Does not require inactive/secondary overlays (e.g. non-focused split canvases) to drive the single shared hover/scroll/popover layout state; only the active surface does.
