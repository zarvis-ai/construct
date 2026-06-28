# 0042-program-run-progress-affordance

Status: accepted
Date: 2026-06-28
Area: ux
Scope: Visual feedback shown on a session program while a program Run is executing in the owning session.

## Decision

Pressing Run on a program must give immediate, continuous visual feedback that the run is in flight, even though the underlying agent may take a long time before it writes any result back to the program. The feedback is an animated "shimmer" over the executed Markdown, governed by a start / narrow / stop lifecycle:

- **Start (optimistic).** The shimmer begins the instant Run is pressed, on the client, before the execute call returns. A full-program run shimmers the whole document; a selection run shimmers only the selected region. The affordance must not wait on the round trip, because the latency it exists to mask is the agent's, not the request's.

- **Narrow (best-effort).** The shimmer is tracked per *block* — a contiguous run of non-blank Markdown lines. A block stays shimmering only while its content is unchanged from when the run started. Any change to a block's content removes it from the shimmer, whether the change comes from the agent writing progress back or from the user editing. As the agent resolves parts of the document, those parts settle out of the animation; untouched parts keep shimmering.

- **Re-running preserves prior narrowing.** Running again while a run is still in flight must not re-shimmer the whole document and discard the progress the agent already showed. A re-Run re-shimmers only the blocks the user changed since the last synced version plus the blocks that were still pending; blocks the agent had already settled stay calm. A first run, or a run scoped to an explicit selection, shimmers its whole executed region.

- **Stop (authoritative).** A run clears when, in priority order: (1) its pending set empties — every executed block has settled; (2) the owning session reaches a **terminal** state (done or errored) after being seen running — the agent is gone and cannot settle the rest; or (3) an **inactivity backstop** expires — a time cap, refreshed by the run's own activity (program declarations/edits on the run), that fires only after the run has gone silent, so a missed signal can never strand the animation on screen. The owning session merely returning to **awaiting-input is not, by itself, a stop signal for a run the agent is actively managing**: a self-scheduling agent — a background loop or monitor, or work delegated to a subagent — routinely goes idle while the run's work is still in flight, so "owning session idle" does not mean "run done." The lone exception is a run that no declaration has ever narrowed — the untouched optimistic shimmer on a non-declaring harness — which still clears when the owning session goes idle after being seen running, because nothing is managing it and its shimmer is only the turn-duration affordance.

- **Run Button Spinner.** The pulsing Run glyph in the title bar is a secondary indicator that stops pulsing early on the first program-relevant output signal (tool call, reasoning, or other assistant-visible content) to signal that the agent has started active work, even while the program shimmer continues.

Editing during a run is never blocked: the program is co-editable, and a run does not lock it. Because editing a block changes its content, editing inherently takes that block out of the shimmer — touching a block transfers it from "agent is working here" to "the user owns this now." This falls out of block-content tracking; no separate edit gesture is required.

Session activity alone must not drive the program affordance. A session can be busy because the user typed an ordinary prompt, not because a program run is active. The shimmer therefore starts only on a program Run, and a robust implementation distinguishes the program-originating turn from any other turn rather than treating generic session busyness as "program is running."

## Reason

The program executes by submitting the Markdown to the owning session as one instruction turn; the program only repaints when the agent later writes back. Between Run and that write there is a long silence with no on-program feedback, which reads as "nothing happened." The shimmer fills that silence with truthful progress: it appears immediately, recedes as the work actually resolves, and ends when the turn ends.

Block granularity is the natural unit because the program is plain Markdown with no other structure, and because a block is simultaneously the unit a human edits, the unit the agent rewrites, and the unit the user perceives as "a thing being worked on." Using one unit for shimmer, for narrowing, and for edit-ownership keeps the model coherent and avoids fragile character-range bookkeeping that breaks under concurrent edits.

The instruction the agent receives is a point-in-time snapshot taken at Run. Editing the program afterward does not change what the current run is doing. The shimmer is therefore a progress indicator, not a lock or a live-steering surface, and must not be presented as either.

## Consequences

- The affordance is shared transient program state owned by the daemon, with an optimistic client-side start for the initiating TUI. The daemon publishes the active run's start time, expiry, and pending block signatures in program get/state payloads so other TUIs and restarted TUIs can render the same shimmer. It is not persisted into the Markdown and does not participate in program versioning or optimistic concurrency.
- The daemon starts shared run state only after the Run prompt has been delivered to the owning session. The initiating client still starts optimistically before the round trip returns, but daemon-owned shared state must not be clearable by prompt echo or other delivery artifacts.
- Narrowing is best-effort. A block the agent never rewrites keeps shimmering until the turn completes; that is acceptable and is bounded by the stop signal. Two blocks with identical text are indistinguishable and settle together.
- The authoritative stop signals are, in priority order: the pending set emptying; a terminal owning-session state (done or errored) after the run was seen running; and an inactivity backstop — a hard time cap refreshed by the run's program declarations/edits, which fires only after the run goes silent. A run the agent is actively managing is **not** cleared by the owning session returning to awaiting-input, because self-scheduling agents (loops, monitors, delegated subagents) go idle while the run's work is still in flight; only an unmanaged optimistic run — one no declaration ever narrowed, on a non-declaring harness — is still cleared on that idle transition. The daemon distinguishes the two by whether any in-run program declaration has narrowed the run. The first observed agent-visible output stops only the Run button's pulsing indicator.
- Raw PTY bytes are not a program-run stop signal. PTY-backed harnesses can emit prompt echo, screen redraws, bracketed-paste artifacts, or other delivery noise around Run submission, and those bytes are not distinguishable enough to clear program progress. Program edits still narrow or clear the run, and structured agent-visible events may clear it for harnesses that provide them.
- Clients do not independently clear shared run state from session output events. They render optimistic/local state until the daemon reports active or cleared program run state through program get/state payloads.
- Once every executed block has settled but the turn is still running, the body animation has nothing left to shimmer. Clients may keep a small secondary running indicator to cover that window, but must not block input or imply the program is locked.
- Any rich program client (web, desktop) should follow the same start / narrow / stop lifecycle so the affordance is consistent across surfaces. Clients render animation locally from the daemon-published run facts; they do not invent independent run state except for the initiating client's optimistic pre-response affordance.

## Non-Goals

The shimmer is not a progress bar, an ETA, or a per-task status. It does not report what the agent is doing, only that a region is still in the run and has not yet settled. It does not gate editing, does not change the submitted instruction, and does not by itself convey success or failure — turn completion and the program's own contents do that.

## Examples

- A user runs a whole todo program on an idle session. The entire document shimmers immediately. As the agent moves items into a Done section, each rewritten item stops shimmering. When the agent finishes its turn, the remaining shimmer clears.
- A user selects one section and runs it. Only that section shimmers; the rest of the document is static.
- While a run shimmers, the user starts editing one paragraph. That paragraph stops shimmering as soon as its text changes; the other blocks keep shimmering. The edit is preserved and reconciled by the normal program save/merge path; it does not alter what the current run is doing.
- The agent has settled two of three todo items, so only the third still shimmers. The user edits a different item and presses Run again. The two settled items stay calm; the edited item and the still-unsettled item shimmer. The whole document does not light up again.
