# 0052-session-removal-cascades-to-subagents

Status: accepted
Date: 2026-06-28
Area: persistence
Scope: Archiving or deleting a session also archives or deletes the subagents it owns.

## Decision

Removing a session cascades onto its subagents. When a session is archived, the
child subagents parented to it are archived too; when a session is deleted, its
child subagents are deleted too. The cascade is recursive: a subagent that owns
its own subagents passes the same operation down the whole ownership tree.

The cascade follows the parent/child ownership link (a subagent's recorded
parent), not group membership or list position. Only true subagents are swept —
an independent top-level session is never removed as a side effect of removing
another session.

A failure to remove one subagent is logged and does not abort the rest of the
cascade or the parent's own removal.

## Reason

Subagents are delegated helpers that belong to their parent's task, not durable
standalone workstreams. Leaving them behind when their owner is gone produces
orphaned rows that reference a parent which no longer exists: they clutter the
list, can hold a worktree/adapter open, and leave dangling `@{session:<id>}`
references with no owner to collect them. Cascading matches the mental model that
retiring a task retires the helpers spawned for it — the same intuition behind
optional cascade for groups, made automatic for the tighter parent/subagent bond.

## Consequences

- Callers that want to retire a parent but keep a particular subagent must
  re-parent or promote that subagent to a top-level session first; there is no
  per-call opt-out flag on the parent's archive/delete.
- Archive's reversibility guarantee extends through the cascade: cascade-archived
  subagents keep their transcript and worktree and can be restarted, exactly like
  a directly-archived session. Cascade-delete is correspondingly irreversible for
  every swept subagent.
- The cascade reuses the daemon's single archive/delete entry points per node, so
  every client surface (TUI, CLI, MCP) inherits it without inventing its own
  sweep. Subagent-scoped removal (`construct_subagent_archive` /
  `construct_subagent_delete`) cascades onto that subagent's own children too.
- Removal is depth-first along ownership; a broken or missing child is skipped
  with a warning rather than stranding its siblings or the parent.

## Non-Goals

- This does not change the archive-vs-delete distinction itself (see
  [0047-archive-vs-delete-session-lifecycle](0047-archive-vs-delete-session-lifecycle.md));
  it only defines how each operation propagates to owned subagents.
- This does not cascade across group membership or any link other than the
  parent/subagent ownership relationship
  ([0014-subagents-are-parented-helpers](0014-subagents-are-parented-helpers.md)).

## Examples

- An orchestrator archives a finished parent session; its three review subagents
  are archived with it and disappear from the active list, transcripts intact and
  restartable.
- A user deletes a session that spawned a subagent which itself spawned a nested
  subagent; all three records are removed from disk.
- Deleting an unrelated top-level session leaves another session's subagents
  untouched, because they are not parented to the deleted session.
