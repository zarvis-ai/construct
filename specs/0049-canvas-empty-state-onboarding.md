# 0049-canvas-empty-state-onboarding

Status: accepted
Date: 2026-06-27
Area: ux
Scope: What an empty canvas shows in place of a bare placeholder string.

## Decision

When a canvas has no content, its body renders an onboarding placeholder instead of a single line of grey instructional text. The placeholder has three parts, top to bottom:

1. A one-line description of what the canvas is.
2. Up to two clickable template buttons, drawn as bordered boxes side by side. Clicking a button fills the canvas with that template's Markdown as a starting point the user then edits.
3. A divider followed by a short tip line covering core canvas affordances (session embeds, select-and-Run, clip blocks).

The placeholder appears exactly when the canvas body is empty, and disappears as soon as any content exists — including the moment a template button is clicked or the first character is typed. The "blank" template is never offered as a button, since it is the empty state itself.

Clicking a template button is an ordinary buffer edit: it records an undo state, stamps the document's template id, and persists on the normal save path (canvas close / Run), exactly like typed input. It does not commit immediately or bypass the editor.

## Reason

The canvas is a primary surface, but a bare "type here" prompt does not tell a new user what the canvas is for or give them a fast way to start. Surfacing templates as one-click buttons turns the empty state into discoverable onboarding while preserving the plain editing model: the buttons are shortcuts for content the user could have typed, not a separate creation flow.

## Consequences

- Only the active canvas publishes the button hitboxes, so a click never targets an inactive split.
- The placeholder must keep every line within the canvas width so nothing wraps; wrapping would desync the button hit rows from what is painted. When the canvas is too narrow or short for buttons, or no templates exist, it degrades to the description-and-tip prose with no buttons.
- The button hit geometry is computed in absolute screen cells, which is safe only because an empty canvas never scrolls (offset is always zero). Any future scrolling of the empty state must recompute hits against the scroll offset.
- Template names shown on buttons are truncated to a bounded width so long names cannot blow out the layout.
- The set of templates offered tracks the daemon's template list (built-in plus user templates), fetched at client start and refreshed on reconnect.

## Non-Goals

This spec does not define a full template gallery, template management/editing UI, hover/focus styling for the buttons, or showing more than two templates at once.

## Examples

- Opening a fresh canvas shows the description, two bordered template buttons (e.g. Tasks and Investigation), a divider, and a tip.
- Clicking the Tasks button replaces the empty body with the Tasks template's Markdown, places the cursor at the end, and the placeholder vanishes; `C-/` undoes back to the empty state.
- Deleting all canvas content brings the placeholder back.
- On a very narrow canvas, the same canvas shows only the description and tip, with no buttons.
