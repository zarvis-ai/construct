# 0046-session-create-initial-prompt-submits

Status: accepted
Date: 2026-06-27
Area: harness
Scope: Creating a session or subagent with a non-empty initial prompt must start its first turn, not leave the prompt buffered and the session idle.

## Decision

When a session (including a subagent) is created with a non-empty `prompt`, the
daemon delivers that prompt to the harness via the `session.start` payload, and
every adapter starts its first turn from it. The user must not have to send a
second, manual `send_input` to get a freshly created, prompted session to run.

The seed prompt travels through exactly one channel — the `session.start`
params. The daemon does **not** write the seed prompt into the child PTY. Each
adapter submits it natively:

- Headless adapters (claude/codex/smith/grok headless) push the prompt onto
  their run queue and run it on the first loop iteration, emitting `Running`.
- Interactive PTY harnesses receive the prompt as a native launch argument
  (e.g. claude/codex positional prompt) or as a queued submit; they do not rely
  on the daemon injecting a submit keystroke.

A created session with an empty/absent prompt correctly idles in
`AwaitingInput` until the first `send_input`.

## Reason

A created-with-prompt session that only buffered the text — sitting in
`AwaitingInput` until nudged — would make subagent orchestration unreliable: a
parent would create a child to do work and the child would do nothing until the
parent noticed and re-sent the task. The whole point of `create(prompt)` is "go
do this now."

This is distinct from the canvas `Run` submit path (see
`canvas_pty_submit_bytes` and `0042-canvas-run-progress-affordance`). That path
*does* write bytes into a live PTY-backed session's line editor, so it must
terminate the prompt with CR (`\r`) — an LF would land in the editor
unsubmitted. The create path has no such terminator concern because it never
writes to the PTY; the prompt is structured data in `session.start`. Future
changes should not "fix" the create path by adding a CR/LF terminator — there
is no keystroke to send.

## Consequences

- The daemon's `session.start` construction must keep forwarding
  `params.prompt` verbatim. Dropping or blanking it regresses every harness at
  once into the idle-until-nudged failure.
- Each adapter must keep seeding its first turn from `params.prompt` on a fresh
  (non-resume) start. On resume the seed prompt is intentionally skipped — it is
  already in the harness's own restored conversation.
- New harnesses/adapters inherit this contract: consume `params.prompt` at
  `session.start` and start the turn; do not wait for an external submit.

## Non-Goals

- Resume behavior: a resumed session does not re-run its original seed prompt.
- Empty-prompt sessions: idling in `AwaitingInput` with no prompt is correct,
  not a bug.

## Examples

- A parent calls `subagent_create(harness, prompt)`. The child transcript opens
  with the user prompt, transitions to `Running`, and produces assistant output
  without any follow-up `send_input`.
- A parent creates a child with no prompt. The child opens in `AwaitingInput`;
  the parent's later `send_input` (enqueue) starts the first turn.
