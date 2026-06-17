# 0031-sessions-fork-into-new-siblings

Status: accepted
Date: 2026-06-17
Area: architecture
Scope: Changing the harness backing an existing session, and carrying context across harnesses.

## Decision

A session's harness is fixed for the life of that session. To "switch the
harness" of running work, agentd **forks**: it creates a new sibling session
backed by the chosen harness and leaves the original untouched. The fork
inherits the source's working directory and group, runs as an independent
top-level session (not a child/subagent), and — unless seeding is disabled or
the target harness takes commands rather than conversation (`shell`) — its
initial prompt is seeded with a rendered summary of the source transcript.

Context transfer across harnesses is **best-effort and limited to the
harness-agnostic transcript**. The fork does not adopt the source harness's
private resume state.

## Reason

A session's identity includes harness ownership (spec 0001), and the
adapter-process lifecycle binds one reconnectable adapter to a session for its
lifetime. Mutating the harness in place would break that identity and the
reconnect contract, and there is no faithful way to hand a harness another
harness's private conversation state (smith's own message log, or the external
CLIs that claude/codex/antigravity wrap, each manage their own resume).

The seed is rendered from the **full** transcript by default, in chronological
order, because the user's objective is normally stated at the very beginning —
dropping the opening to keep only recent activity would discard the goal the
fork exists to continue.

Forking sidesteps all of that: it reuses the ordinary session-creation path,
keeps both the before and after available to the user, and is honest about what
can and cannot cross the boundary.

## Consequences

- Forking is composed from existing primitives (read source, read transcript,
  create) and needs no new daemon mutation or adapter-lifecycle path. It can
  live in the shared client so every surface (CLI, MCP, future UI) forks
  identically.
- A model spec is harness-specific, so the source's model is only inherited
  when the harness is unchanged; otherwise the target harness picks its default
  unless the caller overrides it.
- The seed is a plain-text rendering of the full transcript by default. It is
  background context, not a re-execution of past tool calls, and is omitted for
  harnesses that consume commands rather than conversation. A caller may set a
  byte ceiling; when hit, the opening (objective) and the most-recent activity
  are preserved and the middle is elided, so the goal is never dropped.
- Because the harness-private state does not transfer, a fork is a fresh agent
  run primed with context — not a perfect continuation. Surfaces should present
  it that way.

## Non-Goals

This does not add in-place harness mutation, does not make the fork a subagent
of the source, and does not attempt to translate one harness's native resume
state into another's.

## Examples

From a stuck `claude` session, fork into `smith` to continue with a different
model, seeded with the recent conversation; the original `claude` session stays
exactly as it was. Forking a `shell` session just opens a new shell in the same
directory and group, with no conversational seed.
