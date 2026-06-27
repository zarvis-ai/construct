# 0042-canvas-run-progress-affordance

Status: accepted
Date: 2026-06-27
Area: ux
Scope: Visual feedback shown on a session canvas while a canvas Run is executing in the owning session.

## Decision

Pressing Run on a canvas must give immediate, continuous visual feedback that the run is in flight, even though the underlying agent may take a long time before it writes any result back to the canvas. The feedback is an animated "shimmer" over the executed Markdown, governed by a start / narrow / stop lifecycle:

- **Start (optimistic).** The shimmer begins the instant Run is pressed, on the client, before the execute call returns. A full-canvas run shimmers the whole document; a selection run shimmers only the selected region. The affordance must not wait on the round trip, because the latency it exists to mask is the agent's, not the request's.

- **Narrow (best-effort).** The shimmer is tracked per *block* — a contiguous run of non-blank Markdown lines. A block stays shimmering only while its content is unchanged from when the run started. Any change to a block's content removes it from the shimmer, whether the change comes from the agent writing progress back or from the user editing. As the agent resolves parts of the document, those parts settle out of the animation; untouched parts keep shimmering.

- **Re-running preserves prior narrowing.** Running again while a run is still in flight must not re-shimmer the whole document and discard the progress the agent already showed. A re-Run re-shimmers only the blocks the user changed since the last synced version plus the blocks that were still pending; blocks the agent had already settled stay calm. A first run, or a run scoped to an explicit selection, shimmers its whole executed region.

- **Stop (authoritative).** The shimmer for a session clears when the canvas-originating turn completes — observed as the owning session returning to an idle state (awaiting input, done, or errored) after it was seen running. A hard time cap also clears it, so a missed completion signal can never strand the animation on screen.

Editing during a run is never blocked: the canvas is co-editable, and a run does not lock it. Because editing a block changes its content, editing inherently takes that block out of the shimmer — touching a block transfers it from "agent is working here" to "the user owns this now." This falls out of block-content tracking; no separate edit gesture is required.

Session activity alone must not drive the canvas affordance. A session can be busy because the user typed an ordinary prompt, not because a canvas run is active. The shimmer therefore starts only on a canvas Run, and a robust implementation distinguishes the canvas-originating turn from any other turn rather than treating generic session busyness as "canvas is running."

## Reason

The canvas executes by submitting the Markdown to the owning session as one instruction turn; the canvas only repaints when the agent later writes back. Between Run and that write there is a long silence with no on-canvas feedback, which reads as "nothing happened." The shimmer fills that silence with truthful progress: it appears immediately, recedes as the work actually resolves, and ends when the turn ends.

Block granularity is the natural unit because the canvas is plain Markdown with no other structure, and because a block is simultaneously the unit a human edits, the unit the agent rewrites, and the unit the user perceives as "a thing being worked on." Using one unit for shimmer, for narrowing, and for edit-ownership keeps the model coherent and avoids fragile character-range bookkeeping that breaks under concurrent edits.

The instruction the agent receives is a point-in-time snapshot taken at Run. Editing the canvas afterward does not change what the current run is doing. The shimmer is therefore a progress indicator, not a lock or a live-steering surface, and must not be presented as either.

## Consequences

- The affordance is client-side presentation state. It is derived from signals the client already receives (the canvas Run it issued, canvas-state updates, and session status transitions); it is not persisted into the Markdown and does not participate in canvas versioning or optimistic concurrency.
- Narrowing is best-effort. A block the agent never rewrites keeps shimmering until the turn completes; that is acceptable and is bounded by the stop signal. Two blocks with identical text are indistinguishable and settle together.
- Correlating the stop signal with generic session status is approximate when the session was already busy at Run time. The precise version is a daemon-side tag marking the canvas-originating turn so its completion is unambiguous; until that exists, the client correlates on the next idle transition, with a hard time cap as a backstop. A future tag must not regress the immediate optimistic start.
- Once every executed block has settled but the turn is still running, the body animation has nothing left to shimmer. Clients may keep a small secondary running indicator to cover that window, but must not block input or imply the canvas is locked.
- Any rich canvas client (web, desktop) should follow the same start / narrow / stop lifecycle so the affordance is consistent across surfaces. Promoting the run state into broadcast canvas state would let all clients share one definition rather than each re-deriving it.

## Non-Goals

The shimmer is not a progress bar, an ETA, or a per-task status. It does not report what the agent is doing, only that a region is still in the run and has not yet settled. It does not gate editing, does not change the submitted instruction, and does not by itself convey success or failure — turn completion and the canvas's own contents do that.

## Examples

- A user runs a whole todo canvas on an idle session. The entire document shimmers immediately. As the agent moves items into a Done section, each rewritten item stops shimmering. When the agent finishes its turn, the remaining shimmer clears.
- A user selects one section and runs it. Only that section shimmers; the rest of the document is static.
- While a run shimmers, the user starts editing one paragraph. That paragraph stops shimmering as soon as its text changes; the other blocks keep shimmering. The edit is preserved and reconciled by the normal canvas save/merge path; it does not alter what the current run is doing.
- The agent has settled two of three todo items, so only the third still shimmers. The user edits a different item and presses Run again. The two settled items stay calm; the edited item and the still-unsettled item shimmer. The whole document does not light up again.
