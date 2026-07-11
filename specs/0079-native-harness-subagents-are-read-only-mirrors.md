# 0079-native-harness-subagents-are-read-only-mirrors

Status: accepted
Date: 2026-07-09
Area: harness
Scope: How Construct represents subagents created by a wrapped harness's own delegation mechanism.

## Decision

When a wrapped harness creates its own subagent, Construct projects that child
as a read-only virtual session beneath the owning Construct session. The
harness remains the sole owner of the child's process and lifecycle.

A native child has a stable harness-native identifier, explicit native
provenance, a parent link, live lifecycle state, and a structured transcript.
Clients render it in the ordinary session tree and allow users to focus and
inspect it. Construct does not spawn, resume, interrupt, archive, delete, or
send input directly to the native child. Those operations must go through the
parent harness until an explicit harness-specific control contract exists.

Native parent links are recursive. A native subagent created by another native
subagent appears beneath that native parent rather than being flattened under
the owning Construct session.

When a harness can publish an authoritative retained-child snapshot, Construct
archives mirrors absent from that snapshot. Retained transcript files do not
restore an archived mirror; a later native activity event does.
Harness-native identifiers are canonicalized across transcript filenames and
lifecycle notifications before they become mirror identities.

A terminal native-child status archives the mirror for every harness while
preserving the terminal outcome. For Claude Code, a terminal status in the
parent transcript is also the native-view removal signal because Claude can
retain the child's transcript files after dropping it from the active-agent
view.

## Reason

Claude Code, Codex, Antigravity, and Grok persist stable child identities,
parentage, semantic transcripts, and lifecycle signals. Showing that work makes
delegation visible without duplicating a child process or claiming lifecycle
authority Construct does not have.

Treating a mirror as an ordinary managed session would make controls
misleading and could cause Construct to resume a second harness process for a
child that is already owned by its parent.

## Consequences

- Adapter events may describe a native child and carry semantic transcript
  events belonging to it.
- The daemon persists virtual child summaries and transcripts so ordinary
  list, detail, transcript, focus, and preview paths work unchanged.
- Virtual children are excluded from daemon startup auto-resume.
- Removed native children become archived mirrors rather than stale active
  rows, preserving their transcripts without implying they still exist.
- Native children are archived when they reach any terminal state, regardless
  of whether the harness also publishes an explicit removal signal.
- Clients visibly label native children and must support recursive child
  traversal.
- Harness transcript formats are discovery inputs, not a lifecycle ownership
  transfer. Missing or changed upstream metadata may temporarily reduce native
  child visibility without affecting the parent session.

## Non-Goals

- Providing a common interrupt, message, resume, archive, or delete API for
  native children.
- Converting Construct-created subagents into harness-native children.
- Guaranteeing that every third-party harness exposes enough metadata for
  discovery.

## Examples

- A Claude Code session launches an Agent task. A `(native) claude` child row
  appears under it, streams the child's semantic transcript, and is archived
  when Claude reports task completion or failure.
- A Codex rollout declares a parent thread. A `(native) codex` child row appears
  under the Construct session associated with that parent rollout.
- A native Codex child launches another child. The grandchild appears beneath
  the first native child and is independently focusable.
- Antigravity reports a created child conversation and later delivers its
  completion message. The mirror follows that conversation's transcript and
  transitions from running to done.
- Grok publishes child spawn and finish updates separately from chat history.
  Construct consumes both logs so the mirror has lifecycle state and semantic
  transcript events.
