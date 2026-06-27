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

- **Stop (authoritative).** The shimmer for a session clears on the first canvas-relevant
  output signal from that session: tool call, reasoning, or other assistant-visible
  content. A hard time cap also clears it, so a missed output signal can never strand
  the animation on screen.

Editing during a run is never blocked: the canvas is co-editable, and a run does not lock it. Because editing a block changes its content, editing inherently takes that block out of the shimmer — touching a block transfers it from "agent is working here" to "the user owns this now." This falls out of block-content tracking; no separate edit gesture is required.

Session activity alone must not drive the canvas affordance. A session can be busy because the user typed an ordinary prompt, not because a canvas run is active. The shimmer therefore starts only on a canvas Run, and a robust implementation distinguishes the canvas-originating turn from any other turn rather than treating generic session busyness as "canvas is running."

## Reason

The canvas executes by submitting the Markdown to the owning session as one instruction turn; the canvas only repaints when the agent later writes back. Between Run and that write there is a long silence with no on-canvas feedback, which reads as "nothing happened." The shimmer fills that silence with truthful progress: it appears immediately, recedes as the work actually resolves, and ends when the turn ends.

Block granularity is the natural unit because the canvas is plain Markdown with no other structure, and because a block is simultaneously the unit a human edits, the unit the agent rewrites, and the unit the user perceives as "a thing being worked on." Using one unit for shimmer, for narrowing, and for edit-ownership keeps the model coherent and avoids fragile character-range bookkeeping that breaks under concurrent edits.

The instruction the agent receives is a point-in-time snapshot taken at Run. Editing the canvas afterward does not change what the current run is doing. The shimmer is therefore a progress indicator, not a lock or a live-steering surface, and must not be presented as either.

## Consequences

- The affordance is shared transient canvas state owned by the daemon, with an optimistic client-side start for the initiating TUI. The daemon publishes the active run's start time, expiry, and pending block signatures in canvas get/state payloads so other TUIs and restarted TUIs can render the same shimmer. It is not persisted into the Markdown and does not participate in canvas versioning or optimistic concurrency.
- The daemon starts shared run state only after the Run prompt has been delivered to the owning session. The initiating client still starts optimistically before the round trip returns, but daemon-owned shared state must not be clearable by prompt echo or other delivery artifacts.
- Narrowing is best-effort. A block the agent never rewrites keeps shimmering until the first observed output; that is acceptable and is bounded by the stop signal. Two blocks with identical text are indistinguishable and settle together.
- Session status transitions are intentionally ignored as stop signals; they do not uniquely identify canvas-originating activity and can arrive in the absence of output. Until the daemon has an explicit canvas-turn id, it clears shared run state on first observed agent-visible output. A hard time cap remains as a backstop for silent runs.
- Clients do not independently clear shared run state from session output events. They render optimistic/local state until the daemon reports active or cleared canvas run state through canvas get/state payloads.
- Once every executed block has settled but the turn is still running, the body animation has nothing left to shimmer. Clients may keep a small secondary running indicator to cover that window, but must not block input or imply the canvas is locked.
- Any rich canvas client (web, desktop) should follow the same start / narrow / stop lifecycle so the affordance is consistent across surfaces. Clients render animation locally from the daemon-published run facts; they do not invent independent run state except for the initiating client's optimistic pre-response affordance.

## Non-Goals

The shimmer is not a progress bar, an ETA, or a per-task status. It does not report what the agent is doing, only that a region is still in the run and has not yet settled. It does not gate editing, does not change the submitted instruction, and does not by itself convey success or failure — turn completion and the canvas's own contents do that.

## Examples

- A user runs a whole todo canvas on an idle session. The entire document shimmers immediately. As the agent moves items into a Done section, each rewritten item stops shimmering. When the agent finishes its turn, the remaining shimmer clears.
- A user selects one section and runs it. Only that section shimmers; the rest of the document is static.
- While a run shimmers, the user starts editing one paragraph. That paragraph stops shimmering as soon as its text changes; the other blocks keep shimmering. The edit is preserved and reconciled by the normal canvas save/merge path; it does not alter what the current run is doing.
- The agent has settled two of three todo items, so only the third still shimmers. The user edits a different item and presses Run again. The two settled items stay calm; the edited item and the still-unsettled item shimmer. The whole document does not light up again.
