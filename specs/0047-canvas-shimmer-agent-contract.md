# 0047-canvas-shimmer-agent-contract

Status: accepted
Date: 2026-06-27
Area: harness
Scope: What an agent executing a canvas Run must do with per-block shimmer so the animation truthfully tracks unfinished work.

## Decision

Shimmer on a canvas block has exactly one meaning to the agent: **the block's work is still pending in this run — queued, in progress, or not yet done; outcome unknown.** No shimmer means the block is **settled — done, skipped, or no work needed.** "Pending" is defined by the state of the work, not by who does it: a block counts as pending whether the agent will do it inline or hand it to a subagent, and it counts as pending from the moment the agent decides to act on it — including before a delegated subagent has started.

From that single meaning, the agent's obligations follow:

- **Planning pass first.** The agent's first canvas action on a Run — before doing any task and before creating any subagent — is one edit that touches every executed block, marking each pending block with `shimmer: true` and letting every settled block clear. This is the only way no-work and skipped blocks stop shimmering promptly instead of riding the optimistic full-canvas shimmer until the turn ends.
- **A pending block keeps `shimmer: true` on every edit until it settles.** Editing a block clears its shimmer unless the edit carries `shimmer: true`. So an edit that records intermediate state on a still-pending block (e.g. "delegated", partial progress) must re-assert `shimmer: true`, or it will wrongly settle a block whose work is still in flight.
- **A settled block never carries `shimmer: true`.** Once work on a block is done, skipped, or unnecessary, the final edit omits shimmer so it clears.

The agent must not invert these: never strand a settled block shimmering, and never drop shimmer from a block whose work is still in flight.

## Reason

The shimmer's job is to fill the silence between Run and the agent's first writeback with truthful progress (see `0042-canvas-run-progress-affordance`). That only works if the agent's notion of shimmer matches the user's: "this region is still being worked on." Earlier instruction phrasings narrowed shimmer to "a subagent is actively running," which broke in two predictable ways: agents cleared shimmer on blocks they were about to delegate (the subagent had not started, so "actively running" read as false), and agents left no-work blocks shimmering because nothing told them to clear settled blocks up front. Defining shimmer as pending-vs-settled — independent of inline-vs-delegated and independent of whether a subagent has spun up yet — removes both failure modes and gives the agent one question to answer per block: is this settled, or still pending?

## Consequences

- The agent-facing instructions and the canvas-edit tool description must describe shimmer as pending-vs-settled, not as subagent-activity. They must not reintroduce an "only when a subagent is actively running" rule, because the planning pass necessarily marks blocks before their subagents start.
- The planning-pass-first requirement is surfaced in the canvas execution prompt itself (the literal turn prompt), not only in the run-context instructions, because salience there is what makes agents actually do it before starting work.
- Narrowing remains best-effort and system-owned: a pending block the agent never re-touches still clears at turn end via the stop signal in `0042`. The agent contract here only governs which blocks the agent actively keeps shimmering or clears, not the lifecycle backstops.
- This is an agent behavior contract, not a system mechanism. The daemon still treats any edit without `shimmer: true` as a clear and re-adds shimmer only for edits that set the flag; this spec constrains how the agent uses that mechanism.

## Non-Goals

This does not change the start/narrow/stop lifecycle, the optimistic client-side start, or the stop signals defined in `0042-canvas-run-progress-affordance`. It does not make shimmer a per-task status, a lock, or a progress bar. It does not require shimmer to distinguish inline work from delegated work — both are simply "pending."

## Examples

- A canvas has three blocks. The agent's first action is a planning-pass edit: block A (will run inline) and block B (delegated to a subagent that has not started yet) get `shimmer: true`; block C (already done, no work) omits shimmer and clears immediately. A and B keep shimmering; C is calm within seconds of Run.
- The agent delegates block B and edits it to note the delegation. That edit carries `shimmer: true`, so B keeps shimmering while the subagent works. When the subagent's result lands and the agent writes it back, that final edit omits shimmer and B settles.
- The agent finishes block A's inline work and writes the result with a plain edit (no shimmer). A settles. Block B is still pending and keeps shimmering until its result arrives or the turn ends.
