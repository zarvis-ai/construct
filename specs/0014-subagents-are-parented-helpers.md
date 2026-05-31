# 0014-subagents-are-parented-helpers

Status: accepted
Date: 2026-05-31
Area: convention
Scope: How agents should interpret requests for subagents versus new sessions.

## Decision

When a user asks for a "subagent", the default behavior is to create a child
agent parented to the current session. The child may be backed by any harness,
appears nested under the parent in clients, and remains managed through
subagent operations.

When a user asks for a "new session", "top-level session", "visible session", or
an independent fleet session, the default behavior is to create a normal
fleet-level session instead.

## Reason

Users use "subagent" to mean delegated helper work that belongs to the current
task. Creating an unrelated top-level session breaks that mental model: the
review/helper is harder to collect, does not appear nested under the parent, and
is not managed through subagent tools.

Top-level sessions are still useful for durable workstreams, manual panes, and
independent agents. The distinction should be controlled by the user's wording.

## Consequences

Agent prompts and tool descriptions should steer models toward child subagents
when the word "subagent" appears, including for split-review or parallelized
research tasks.

Tools that create normal sessions should describe themselves as top-level
session creation, and should explicitly point to the subagent creation tool when
the user asked for a subagent.

## Examples

- "Create three subagents to review this PR" creates three parented child agents.
- "Use Codex and Claude subagents to review different angles" creates parented
  subagents backed by the Codex and Claude harnesses.
- "Open a new Codex session" creates a top-level Codex session.
- "Start a shell session for logs" creates a top-level shell session unless the
  user says it should be a subagent.
