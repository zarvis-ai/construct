# 0078-forks-carry-lineage-branch-rail-and-merge

Status: accepted
Date: 2026-07-09
Area: ux
Scope: A single, unified fork action — instant same-harness by default,
explicit cross-harness on request — whose result is always lineage-tracked
and mergeable back into its parent.

## Decision

There is one "fork" concept, not two. Forking a session always creates a
normal top-level sibling session and always records durable lineage
(`forked_from`) with the parent identity, transcript position, and creation
time — there is no separate gate that only tracks lineage for some forks and
not others.

Forking has two entry points, distinguished only by how the target harness is
chosen, never by whether lineage is tracked:

- **Primary (instant, same-harness).** No prompt of any kind. The session is
  forked immediately using its own harness, the new session is selected, and
  keyboard focus lands directly in its live input — continuing work reads the
  same as jumping into any other session.
- **Secondary (explicit, cross-harness).** A harness picker lets the user
  target a different harness than the source. Once a harness is chosen (or
  the default accepted), it lands the same way as the primary path — no
  forced prompt, focus moves straight to the new session.

Merge records a durable result-or-discard outcome on the fork and then
archives it. Taking a result injects a compact transcript rendering into the
parent through its ordinary input path, so it is a real parent
transcript/PTY message, not a side channel.

## Reason

Tangent work needs full context without becoming a parented helper or
silently changing the original session. Splitting "fork" into two
differently-behaved primitives (one instant and untracked, one deliberate and
tracked) forced the user to predict, before acting, whether they'd want the
branch rail and merge menu later — and picking wrong meant losing lineage
retroactively. Tracking lineage unconditionally removes that up-front
decision: every fork, however it was created, is available for the branch
rail, fork log, and merge later. The two-tier keybinding still exists because
same-harness vs. cross-harness is a real fork-time decision (native context
fidelity is only possible within one harness) — but it no longer decides
whether the daemon remembers where the session came from.

## Consequences

- Lineage is session data, so every client can render a branch rail or fork
  log without a separate UI-owned store.
- Forks remain visible top-level user sessions, merely grouped beneath their
  parent in clients that choose to render lineage.
- Same-harness adapters may use native fork state for full context fidelity;
  cross-harness forks retain the portable transcript-seed behavior (spec
  0031). Both are lineage-tracked identically.
- The merge menu (result/discard) and its auto-archive-after-either behavior
  apply to any fork, regardless of which entry point created it.

## Non-Goals

Forks are not subagents, session widgets, or a new persistence system.
