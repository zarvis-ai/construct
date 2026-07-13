# 0091-opencode-native-session-tracking

Status: accepted
Date: 2026-07-13
Area: harness
Scope: Native OpenCode conversation identity inside Construct sessions.

## Decision

Each interactive OpenCode harness session records its active native OpenCode
session id through OpenCode's event plugin API. The integration is injected for
that process only and does not edit the user's project or global OpenCode
configuration. A Construct respawn resumes the recorded native session and does
not resubmit the original prompt.

The recorded id follows OpenCode session creation and updates, including a new
conversation created from inside the TUI. Construct must not infer ownership by
selecting the newest session from OpenCode's shared database because multiple
OpenCode processes may use the same working directory concurrently.

## Reason

OpenCode exposes stable session ids and a native resume flag, but its persistent
database is shared across processes. Time- or directory-based discovery can bind
one Construct session to a sibling's conversation. Process-local OpenCode events
identify the conversation without that race and preserve the native context
across Construct daemon restarts.

## Consequences

- The adapter may merge a local plugin reference into inline OpenCode config for
  the child process, while preserving any inline config supplied by the user.
- Failure to install or configure the capture plugin must not prevent OpenCode
  from launching; it degrades to a fresh conversation on a later respawn.
- The OpenCode TUI continues to own approvals, tools, rendering, and session
  switching. Construct supplies the PTY, lifecycle, and persisted native id.
- The original creation prompt is delivered with OpenCode's native prompt flag
  exactly once.

## Non-Goals

This does not translate OpenCode's internal tool calls or approvals into
Construct-native structured events, and it does not define a headless OpenCode
adapter mode.
