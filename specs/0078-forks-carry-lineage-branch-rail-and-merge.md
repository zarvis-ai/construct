# 0078-forks-carry-lineage-branch-rail-and-merge

Status: accepted
Date: 2026-07-10
Area: ux
Scope: A single harness-picker fork action whose same-harness default requires
no initial prompt and whose result is always lineage-tracked and mergeable.

## Decision

There is one "fork" concept, not two. Forking a session always creates a
normal top-level sibling session and always records durable lineage
(`forked_from`) with the parent identity, transcript position, and creation
time — there is no separate gate that only tracks lineage for some forks and
not others.

Forking has one entry point. Its harness picker is pre-filled with the source
session's harness, so Enter accepts a same-harness fork while completion or an
edit selects another harness. Harness selection submits the fork immediately:
there is no second initial-prompt question, including for the same-harness
default. The new session is selected and focus moves to its live input.

Merge records a durable result-or-discard outcome on the fork and then
archives it. Taking a result injects a compact transcript rendering and the
fork's stable session ID into the parent through its ordinary input path, so
it is a real parent transcript/PTY message, not a side channel. The reference
keeps the full fork history discoverable without copying that history into the
parent's context.

## Reason

Tangent work needs full context without becoming a parented helper or
silently changing the original session. Splitting "fork" into two
differently-behaved primitives (one instant and untracked, one deliberate and
tracked) forced the user to predict, before acting, whether they'd want the
branch rail and merge menu later — and picking wrong meant losing lineage
retroactively. Tracking lineage unconditionally removes that up-front
decision: every fork, however it was created, is available for the branch
rail, fork log, and merge later. Harness choice remains explicit because
same-harness native context fidelity differs from portable cross-harness
transcript seeding, but it no longer requires separate keybindings or flows.

## Consequences

- Lineage is session data, so every client can render a branch rail or fork
  log without a separate UI-owned store.
- Forks remain visible user sessions, grouped and indented beneath their parent
  in clients that choose to render lineage. A fork's lineage marker and its
  independent pinned state occupy separate list affordances, so pinning a fork
  never hides or replaces its fork identity. Its status glyph aligns with the
  start of its parent session's name.
- Same-harness adapters may use native fork state for full context fidelity;
  cross-harness forks retain the portable transcript-seed behavior (spec
  0031). Both are lineage-tracked identically.
- The merge menu (result/discard) and its auto-archive-after-either behavior
  apply to any fork, regardless of which entry point created it.
- Every session title actions menu exposes **Fork conversation**, including on
  a fork. It also exposes **Merge result** as an inactive affordance on a
  parent and an active action only on a fork, making the branch workflow
  discoverable without suggesting that a parent can merge into itself.

## Non-Goals

Forks are not subagents, session widgets, or a new persistence system.
