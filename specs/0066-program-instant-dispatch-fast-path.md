# 0066-program-instant-dispatch-fast-path

Status: accepted
Date: 2026-07-02
Area: protocol
Scope: Selection-Run on a program list item that names exactly one `@{harness:<name>}` clip.

## Decision

When a program Run targets a **selection** and every selected block is a **list item whose text names exactly one `@{harness:<name>}` smart clip**, the daemon executes the dispatch mechanically instead of routing it through the owning session's agent. For each such item, the daemon:

1. Creates a subagent session (`SessionKind::Subagent`, parented to the owning session, same `cwd` as the owning session, backed by the named harness) with the item's text — list marker and clip syntax stripped — as its initial prompt.
2. Applies one anchored program edit that appends an `@{session:<new-id>}` clip to that item (`keep_pending: true`, so the block's shimmer survives the text change) and declares the item pending with tooltip "Dispatched".

No prompt is delivered to the owning session for a fast-pathed run: the owning agent is not consulted, does not run a turn, and does not decide anything. The response and the `active_run` shimmer projection reflect the started run (built via the same `start_program_run` seeding used by every other program run) so clients render shimmer exactly as they would for an agent-mediated run.

**Mixed selections are all-or-nothing.** If any selected block is not a list item, contains zero smart clips, contains more than one smart clip, or its one clip is not `@{harness:<name>}` (or names an empty harness), the *entire* selection falls through unchanged to the normal execute path — the whole selection is delivered as a prompt to the owning session, same as before this decision. No selection is partially fast-pathed and partially delivered as a prompt in the same Run.

A full-document Run (no selection) always uses the normal path, regardless of content. This decision only changes selection-Run behavior.

## Reason

`@{harness:<name>}` naming a harness with no other content is an unambiguous, purely mechanical instruction: spawn a worker and link it in. Routing that through the owning session's agent costs a full LLM round trip to re-derive a decision the human already made by writing the clip and pressing Run. Executing it in the daemon collapses that latency to the anchored-edit/session-create cost — sub-second — while leaving every other program semantic (full-document Run, multi-step orchestration, agent judgment about *what* to delegate) exactly as `0041`/`0042`/`0048` already define it.

All-or-nothing mixed-selection handling is the simplest behavior that stays correct: partially fast-pathing a selection would require deciding whether the *rest* of the selection still gets a prompt in the same Run, whether the fast-pathed items' clips are visible to that prompt's snapshot, and how a single `active_run` shimmer projection represents two different execution mechanisms started by one gesture. None of that judgment is mechanical, so it does not belong in the fast path; falling through preserves the one well-defined normal-path behavior for anything that isn't the exact single-clip-item shape.

## Consequences

- The fast path is purely additive to the existing `program_execute` contract: no new RPC method, no new `ProgramExecuteParams` field. Detection is entirely a function of the selection's Markdown.
- `keep_pending` plus an explicit tooltip-carrying shimmer declaration is the same mechanism `0048`/`0053` already require of an agent moving or annotating a still-in-flight block; the daemon uses it identically here, so shimmer, block-ref reconciliation, and the run stop/backstop lifecycle (`0042`) apply unchanged.
- The daemon does not watch the dispatched subagent(s) for completion and does not settle their items itself. Settling — or further orchestration — remains agent or human work, done the normal way (editing the program, running it again). The dispatched item keeps shimmering (tooltip "Dispatched") until something explicitly settles it or the run's inactivity backstop fires.
- Subagent creation happens before the anchored edit lands. If a later item's subagent creation fails, earlier items in the same dispatch already have live subagents with no program clip pointing at them yet; the caller sees the error and may re-run. The daemon does not roll back already-created subagents — the same non-transactional tradeoff anchored program edits already accept for any multi-edit call.
- Anchored-edit addressing is still text-based: two selected items with byte-identical text (including their clip) can collide on `old_string` uniqueness exactly as any other anchored edit would. This is an existing limitation of anchored edits, not new to this fast path.

## Non-Goals

- Not a scheduler or orchestrator. The daemon does not sequence dispatched items, does not decide what to delegate, and does not interpret program prose beyond recognizing the single-clip-item shape.
- Does not change full-document Run behavior, the shimmer lifecycle (`0042`), block addressing (`0053`), or the shimmer tooltip contract (`0057`).
- Does not add a new smart-clip type or change `@{harness:<name>}` clip syntax; it only recognizes an existing clip shape as a trigger for mechanical execution.
- Does not settle a dispatched item when its subagent finishes; that remains external (agent or human) behavior.

## Examples

- A program has `- Fix the flaky login test @{harness:codex}`. Selecting that line and pressing Run creates a `codex` subagent with prompt "Fix the flaky login test", rewrites the line to `- Fix the flaky login test @{harness:codex} @{session:sabc123}`, and the line shimmers with tooltip "Dispatched" — all without the owning session's agent ever running.
- A selection spans two items: `- Fix the flaky test @{harness:codex}` and `- Fix the flaky test @{harness:codex}` (identical text). The anchored edit for the second item may fail with an ambiguous-anchor error after the first item's subagent was already created, per Consequences above.
- A selection spans `- Fix the flaky test @{harness:codex}` and `- Investigate the timeout, see @{session:s1}`. The second item's clip is not a harness clip, so the *whole* selection is delivered as a prompt to the owning session, unchanged from prior behavior — no subagent is created for either item.
- A selection is the heading `## Todo` plus one item beneath it. The heading is not a list item, so the whole selection falls through to the normal path.
