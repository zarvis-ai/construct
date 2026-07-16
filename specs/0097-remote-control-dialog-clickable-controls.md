# 0097-remote-control-dialog-clickable-controls

Status: accepted
Date: 2026-07-15
Area: tui
Scope: The controls inside the remote-control dialog are clickable, dispatching through the same handlers as their keyboard equivalents.

## Decision

Every actionable control the remote-control dialog draws is clickable with the
mouse, in addition to its keyboard shortcut:

- the tunnel-type buttons in the chooser (a click moves the selection onto that
  provider, mirroring an arrow key — it does not start it);
- the `[ back ]` and `[ stop ]` buttons in the tunnel-ready view;
- the footer key hints `Enter`, `Esc`, `o`, and `←` present in each view.

A click on a control produces exactly the same effect as pressing the key it
stands for. The click path routes through the dialog's keyboard handler (or the
same selection/focus state that handler mutates) rather than a second,
click-only decision path. Clickable key hints are styled distinctly so their
affordance is visible.

Clicks that land inside the dialog but on no control are still swallowed (they
do not fall through to the panes beneath the modal), and a click outside the
dialog dismisses it, as for any modal.

## Reason

The dialog is reached from a persistent, clickable status affordance (see
[0096](0096-remote-control-status-affordance.md)); arriving there by mouse and
then being forced onto the keyboard to do anything is a discontinuity. Routing
clicks through the keyboard handler keeps a single source of truth for what each
control does, so the two input paths cannot drift apart.

## Consequences

Control geometry must be registered per frame in absolute screen coordinates,
after the dialog's QR/text composition has placed the body, and cleared when the
dialog closes so no stale zone dispatches against a later frame. When the body is
too wide for the terminal and the modal soft-wraps, click zones may be
suppressed; the keyboard must remain sufficient to drive every control in that
regime.

## Non-Goals

This does not make the dialog's addresses, credentials, or QR code clickable,
and it does not add mouse-only actions that have no keyboard equivalent.
