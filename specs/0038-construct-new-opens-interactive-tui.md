# 0038-construct-new-opens-interactive-tui

Status: accepted
Date: 2026-06-24
Area: cli
Scope: The user-facing `construct new` command that creates top-level sessions.

## Decision

`construct new` is an interactive entry point by default. When no explicit mode
is provided, it creates the session in interactive mode, starts the daemon if
needed, and opens the TUI focused on the new session.

Scripts and integrations that need create-and-exit behavior must request a
non-attaching flow explicitly. `--no-tui` creates an interactive session, prints
its id, and exits. `--mode headless` creates a headless session, prints its id,
and exits.

## Reason

Creating a new session is usually the first step in operating it. Opening the
TUI makes the command behave like the user's next expected action, avoids a
separate attach step, and keeps the default experience aligned with how sessions
are created inside the TUI.

## Consequences

Future CLI changes should keep the default `construct new` flow attached to the
interactive UI. One-shot creation behavior should remain explicit so it remains
safe for scripts to choose it deliberately.

Removing or hiding individual one-shot commands does not remove the underlying
client or protocol operation when other clients, tools, or the TUI still need
that operation.

## Non-Goals

This does not require every IPC client to default to interactive mode. Non-TUI
protocol clients may keep their own explicit mode and attach behavior.
