# 0048-program-shimmer-agent-contract

Status: accepted
Date: 2026-06-28
Area: harness
Scope: What an agent executing a program Run must do with per-block shimmer so the animation truthfully tracks unfinished work.

## Decision

Shimmer on a program block has exactly one meaning to the agent: **the block's work is still pending in this run — queued, in progress, or not yet done; outcome unknown.** No shimmer means the block is **settled — done, skipped, or no work needed.**

"Pending" is a property of the **work**, not of **how the work runs.** Shimmer must be agnostic to the execution mechanism: the agent doing the block itself, handing it to a subagent, and any future execution path are all the same to the shimmer — what matters is only whether the block is still pending or has settled. A block counts as pending from the moment the agent decides to act on it, including before any delegated work has started. The contract must never be phrased in terms of a specific mechanism (e.g. "a subagent is running"), because that both excludes the other ways a block can run and breaks the cases that mechanism does not cover.

From that single meaning, the agent's obligations follow:

- **Planning pass first.** The agent's first program action on a Run — before doing or delegating any task — is one edit that touches every executed block, marking each pending block with `shimmer: true` and letting every settled block clear. This is the only way no-work and skipped blocks stop shimmering promptly instead of riding the optimistic full-program shimmer until the turn ends.
- **A pending block keeps `shimmer: true` on every edit until it settles.** Editing a block clears its shimmer unless the edit carries `shimmer: true`. So an edit that records intermediate state on a still-pending block (e.g. a progress note, a hand-off marker) must re-assert `shimmer: true`, or it will wrongly settle a block whose work is still in flight.
- **A settled block never carries `shimmer: true`.** Once work on a block is done, skipped, or unnecessary, the final edit omits shimmer so it clears.

The agent must not invert these: never strand a settled block shimmering, and never drop shimmer from a block whose work is still in flight.

## Reason

The shimmer's job is to fill the silence between Run and the agent's first writeback with truthful progress (see `0042-program-run-progress-affordance`). That only works if the agent's notion of shimmer matches the user's: "this region is still being worked on." Earlier instruction phrasings narrowed shimmer to "a subagent is actively running," which broke in three ways: agents cleared shimmer on blocks they were about to delegate (the subagent had not started, so "actively running" read as false); agents left no-work blocks shimmering because nothing told them to clear settled blocks up front; and the framing silently assumed delegation was the only way a block runs, when the main agent runs blocks itself and other execution mechanisms may exist in the future. Defining shimmer as pending-vs-settled — independent of how the block runs and of whether anything has spun up yet — removes all three failure modes and gives the agent one question to answer per block: is this settled, or still pending?

## Consequences

- The agent-facing instructions and the program-edit tool description must describe shimmer as pending-vs-settled, never tied to a particular execution mechanism. They must not reintroduce an "only when a subagent is actively running" rule, both because the planning pass necessarily marks blocks before any delegated work starts and because delegation is only one of the ways a block can run.
- The planning-pass-first requirement is surfaced in the program execution prompt itself (the literal turn prompt), not only in the run-context instructions, because salience there is what makes agents actually do it before starting work.
- Narrowing remains best-effort and system-owned: a pending block the agent never re-touches still clears via the stop signals in `0042` — the inactivity backstop once the run goes silent, or, for an unmanaged run that no declaration ever narrowed, the owning session returning to idle. Because a run the agent has narrowed is no longer cleared by the owning session merely going idle (`0042`), a self-scheduling agent that delegates a block and returns to awaiting-input keeps that block shimmering until it settles it or the backstop fires. The agent contract here only governs which blocks the agent actively keeps shimmering or clears, not the lifecycle backstops.
- This is an agent behavior contract, not a system mechanism. The daemon still treats any edit without `shimmer: true` as a clear and re-adds shimmer only for edits that set the flag; this spec constrains how the agent uses that mechanism.

## Non-Goals

This does not change the start/narrow/stop lifecycle, the optimistic client-side start, or the stop signals defined in `0042-program-run-progress-affordance`. It does not make shimmer a per-task status, a lock, or a progress bar. It does not require — or allow — shimmer to distinguish how a block runs; every execution path is simply "pending" until the block settles.

## Examples

The execution mechanisms below (run-it-yourself, delegate-to-a-subagent) are illustrative, not exhaustive; the contract treats any way of running a block identically.

- A program has three blocks. The agent's first action is a planning-pass edit: block A (the agent will run it itself) and block B (delegated, not started yet) both get `shimmer: true`; block C (already done, no work) omits shimmer and clears immediately. A and B keep shimmering; C is calm within seconds of Run.
- The agent hands block B off and edits it to note the hand-off. That edit carries `shimmer: true`, so B keeps shimmering while the work is in flight. When the result lands and the agent writes it back, that final edit omits shimmer and B settles.
- The agent finishes block A itself and writes the result with a plain edit (no shimmer). A settles. Block B is still pending and keeps shimmering until its result arrives or the turn ends.
