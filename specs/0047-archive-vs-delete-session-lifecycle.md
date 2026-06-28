# 0047-archive-vs-delete-session-lifecycle

Status: accepted
Date: 2026-06-27
Area: harness
Scope: Agents and CLI users must be able to soft-archive a session/subagent, not only hard-delete it.

## Decision

Removing a session from active view has two distinct, non-interchangeable operations, and both are reachable from every client surface (TUI, CLI, MCP):

- **Archive** is soft and reversible. It terminates the session's adapter and hides the session from the default list, but keeps the transcript and worktree on disk and leaves the record intact. An archived session can be brought back (restarted/resumed). The `archived` boolean in the session summary records this state.
- **Delete** is hard and irreversible. It kills the adapter and removes the transcript, worktree, and metadata from disk. The record is gone.

Archive and delete share the same single-operation shape (a session/subagent id in, success out) and are exposed as siblings everywhere:

- MCP: `construct_archive_session` (general) and `construct_subagent_archive` (ownership-scoped to the calling session) sit next to `construct_delete_session` / `construct_subagent_delete`.
- CLI: `construct archive <session_id>` sits next to `construct delete <session_id>`.

All of these route to the daemon's existing single archive entry point that flips `archived = true`; clients must not invent a parallel archive mechanism.

## Reason

Delete was previously the only way an agent could clear finished work, which forced an irreversible choice: lose the transcript/worktree or leave clutter. Orchestration patterns (a canvas moving a TODO to Done) routinely want to retire a subagent while preserving its history for later inspection or restart. The TUI already offered archive; agents and CLI users had no equivalent, so the soft/reversible option was unreachable programmatically.

## Consequences

- New session-removal surfaces must offer archive wherever they offer delete, and must keep archive's reversibility guarantee (transcript + worktree survive).
- The daemon's archive path returns immediately and performs adapter shutdown in the background; callers should treat archive as fire-and-forget state flip, not a blocking teardown.
- Descriptions/help text for delete should keep steering callers toward archive when the intent is "tidy up, not destroy," so the destructive option is chosen deliberately.
- Restore/un-archive (resuming an archived session) is a separate operation; today it exists in the daemon and is reachable by restarting the session. Exposing it as a first-class MCP/CLI verb is a natural follow-up but is not required for archive to be useful.

## Non-Goals

- This does not change what `delete` does or make delete recoverable.
- This does not define new persistence for archived state beyond the existing `archived` flag.

## Examples

- An orchestrator finishes a subagent's task and merges its PR, then calls the subagent-archive tool. The subagent disappears from the active list but its transcript and worktree remain; the orchestrator (or a human) can restart it later.
- A user runs `construct archive <id>` to retire a session from the list without losing its work, in contrast to `construct delete <id>` which wipes it.
