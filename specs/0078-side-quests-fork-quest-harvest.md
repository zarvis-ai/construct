# 0078-side-quests-fork-quest-harvest

Status: accepted
Date: 2026-07-09
Area: ux
Scope: User-driven tangent work that is returned to or discarded from an interactive session.

## Decision

A side quest is a normal top-level sibling session forked from an interactive
parent. It records durable lineage (`forked_from`) with the parent identity,
transcript position, and creation time. Harvest records a durable result or
discard outcome on the quest and then archives it.

## Reason

Tangent work needs full context without becoming a parented helper or silently
changing the original session. Lineage is session data so every client can
render a branch rail or quest log without a separate UI-owned store.

## Consequences

- Taking a result injects a compact transcript rendering into the parent through
  its ordinary input path, so it is a real parent transcript/PTY message.
- Side quests remain visible top-level user sessions, merely grouped beneath
  their parent in clients that choose to render lineage.
- Same-harness adapters may use native fork state; cross-harness forks retain
  the portable transcript-seed behavior.

## Non-Goals

Side quests are not subagents, session widgets, or a new persistence system.
