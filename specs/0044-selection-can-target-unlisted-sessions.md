# 0044-selection-can-target-unlisted-sessions

Status: accepted
Date: 2026-06-27
Area: tui
Scope: Validity of the main-view session selection and what a canvas session clip may focus.

## Decision

A session selection that drives the main view (and a main-window pane) is valid as long as that session still exists, independent of whether it currently has a navigable row in the session list. Clicking a canvas session clip focuses its target session persistently — including sessions the list cannot show, such as a subagent (which lists only as a child of its parent, and not at all when the parent is the hidden orchestrator).

The session list is a navigation projection, not the set of focusable sessions. When the current selection has no visible row, the list simply highlights nothing; the main view still renders the selected session.

## Reason

The canvas refers to sessions by id. For an orchestrator's subagent, the canvas clip is the only affordance that can point the main view at it — there is no list row to click. Tying selection-validity to list membership (or to a "user session" kind) made such a click unstable: the click set the selection, but the next session-list refresh re-validated against the visible list, found no row, and reverted — which also popped the previously stashed canvas back open, so the switch appeared to "bounce back" to the canvas.

## Consequences

Any re-validation or pruning of a session selection must test that the session still exists, not that it appears in the rendered list or is of a particular kind. A selection is only reset when its session is actually gone.

Focusing a session through a non-list affordance (canvas clip, and any similar jump-to-session gesture) must also point the active window pane at it, because the main view renders from the pane's selection, not from the bare selection field. Updating one without the other leaves the view showing the previous session.

List navigation and reorder actions are unaffected and continue to operate on the visible user list (see 0007). This decision only widens what may be *focused*, not what list commands traverse.

## Non-Goals

This does not give unlisted sessions a row, a cursor position, or list-navigation reachability. It does not change which sessions the list renders.

## Examples

A fleet/orchestrator canvas shows clips for the orchestrator's subagents. Clicking such a clip reveals that subagent in the main view and keeps it there across session-list refreshes; the list shows no highlighted row because the subagent has none. The selection only changes again when the user navigates elsewhere or the subagent ends.
