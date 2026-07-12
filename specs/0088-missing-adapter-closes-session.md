# 0088-missing-adapter-closes-session

Status: accepted
Date: 2026-07-12
Area: persistence
Scope: Session operations that discover an adapter has disappeared.

## Decision

When an operation finds that a session has no live adapter, the daemon marks a non-terminal session `Done`, persists that state, and broadcasts it to clients before returning the operation error.

## Reason

A non-terminal session without an adapter cannot accept input. Keeping it visually live leaves the user with no recovery path even though restarting the session can recreate its adapter.

## Consequences

Clients can consistently treat a missing adapter as a closed session and offer their normal restart interaction. A successful concurrent restart must not be overwritten by this terminal transition.

## Non-Goals

This does not turn adapter request failures into terminal transitions; only confirmed absence of an adapter has this meaning.
