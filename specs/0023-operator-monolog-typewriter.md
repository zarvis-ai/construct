# 0023-operator-monolog-typewriter

Status: accepted
Date: 2026-06-06
Area: tui
Scope: How the operator (orchestrator) surfaces its spoken messages to the user.

## Decision

The operator surfaces in two ways, and it chooses by *which event it emits*:

- **Findings the user should act on or keep → an Operator widget** (`UiPanel`), the existing mechanism rendered in the matrix area. Persistent.
- **Monolog / "what I just did" narration → a typewriter line over the matrix rain.** When the orchestrator finishes a turn with substantive text, the matrix-rain body briefly becomes a monochrome terminal: the line types out, holds, fades, and the rain resumes. Ephemeral; nothing to open.

The TUI consolidates the orchestrator's streaming assistant `Message` deltas across a turn into one finalized string at turn end (`AgentStatus active=false`), filters the internal `noted`/empty no-op token, and plays it once as the monolog. No new protocol event — `Message` (stream) vs `UiPanel` (widget) already distinguishes the two.

## Reason

The operator's text replies landed only in the daemon-owned orchestrator panel, which is collapsed by default (`orchestrator_panel_h: None`) — so a genuinely useful line ("'run using zarvis' is waiting at the folder trust prompt — press Enter") was invisible unless the user opened the panel. The matrix area is the operator's always-visible visual home, so surfacing its monolog there (without stealing focus or a panel) closes the gap. A typewriter over the rain is ambient: visible but not modal, and self-dismissing.

## Consequences

A short operator reply now reaches the user with the panel closed; the panel remains the full-detail/scrollback view. Only substantive replies show (`noted`/empty are dropped). One monolog at a time — a newer utterance replaces an older one. The monolog rides the existing matrix animation tick, so it needs no extra redraw machinery; it renders only while the matrix rain is visible. Widgets still render on top of a monolog, so an open widget isn't hidden. The orchestrator system prompt was updated to tell the operator its short text shows as a fading monolog and to use widgets for anything persistent/actionable.

## Non-Goals

Not a transient toast, OS notification, or auto-opening panel (considered, rejected as either missable or focus-stealing); not a new protocol/`SessionEvent` variant; does not change the orchestrator panel, the operator title/status, or the widget rendering path; does not surface non-orchestrator sessions' messages this way.
