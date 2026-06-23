# 0035-interactive-upgrade-checks

Status: accepted
Date: 2026-06-23
Area: cli
Scope: Upgrade checks at `construct` process startup.

## Decision

Interactive user-facing `construct` client commands check for newer releases
before running and ask for confirmation before upgrading. If the user accepts,
the command upgrades the installed binary, restarts any running daemon, and then
continues the original command under the upgraded binary.

Daemon lifecycle internals, adapter child processes, ACP stdio mode, hidden MCP
mode, the upgrade command itself, and non-interactive invocations do not prompt.
`CONSTRUCT_NO_UPDATE_CHECK=1` disables both the startup prompt and cached TUI
update notices.

## Reason

Users should learn about available releases when they naturally run the binary,
and accepting the upgrade should make the new code live without requiring a
separate daemon restart. At the same time, daemon exec paths and stdio
integrations must remain non-blocking and script-friendly.

## Consequences

Future startup checks must preserve a non-interactive skip path and must not
insert prompts into daemon, adapter, ACP, or hidden MCP execution. Upgrade
installation and daemon restart should continue to use the same implementation
as the explicit upgrade command so checksum, atomic replace, and restart
behavior do not drift.

This accepts a small startup network probe for interactive client commands.
