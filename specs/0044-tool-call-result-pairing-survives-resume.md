# 0044-tool-call-result-pairing-survives-resume

Status: accepted
Date: 2026-06-27
Area: persistence
Scope: Persisted smith conversation history must present every tool call with a matching tool result before it is replayed to a provider.

## Decision

Every assistant tool call in a smith session's persisted history must have a
matching tool result by the time the history is sent to a model provider. When
a session resumes from its persisted conversation, the harness repairs the
loaded history so that:

- every tool call with no matching result is given a synthetic error result
  (placed immediately after the issuing call), and
- every result whose issuing call is absent is dropped.

The repaired history is written back to disk so the fix is durable across
future resumes. Providers may additionally repair defensively before a request
as belt-and-suspenders, but the load-time repair is the authoritative fix.

Back-fill rather than delete: when a parallel tool batch is partially recorded,
the legs that completed keep their real results; only the missing legs get a
placeholder.

## Reason

Conversation history is persisted append-only, one message per line, and the
assistant's tool-call record is written *before* the tools run and their
results are written. Any interruption in that window — a daemon restart, a
turn-timeout SIGKILL, or two adapter processes briefly sharing one history file
— can leave a tool call on disk with no result.

Provider APIs reject this. The OpenAI / codex Responses backend returns
`400 "No tool output found for function call ..."` and Anthropic rejects an
orphan `tool_use` the same way. Because the full history is replayed on every
turn, a single orphaned record makes *every* subsequent request fail
identically: the session is wedged permanently with no path to recovery, since
nothing else removes the bad record.

## Consequences

- Resume must sanitize tool-call/result pairing before the first provider
  request, and must persist the sanitized history so the repair is not redone
  on every resume.
- A synthetic result is a normal, expected artifact in recovered histories. It
  is marked as an error and worded so the model treats it as "result missing,"
  not as real tool output. Code that reasons over history must tolerate it.
- This repair masks the *symptom* of concurrent writers / torn writes; it does
  not make concurrent writes to one history file safe. A single-writer
  guarantee per session remains a separate, desirable invariant.
- Compaction's existing rule — never cut between a tool call and its result —
  still holds; this decision covers orphans that already exist in the loaded
  history rather than orphans introduced by trimming.

## Non-Goals

- Guaranteeing a single writer per session history file.
- Preserving the lost tool output. The original result is gone; the synthetic
  result only restores a well-formed request shape.

## Examples

- History ends `...assistant[call X]` with no result for X (the turn was killed
  mid-tool). On resume, a synthetic error result for X is inserted after the
  call; the next request is accepted.
- An assistant turn issues parallel calls A and B; only A's result was
  persisted. On resume, A keeps its real result and B gets a synthetic error
  result.
- A result references a call that no longer appears (its call record was lost).
  The stray result is dropped.
