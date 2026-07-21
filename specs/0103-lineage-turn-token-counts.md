# 0103-lineage-turn-token-counts

Status: accepted
Date: 2026-07-21
Area: ux
Scope: Lineage turn-info windows report token consumption instead of message counts, with hover detail for the input/output/cached breakdown.

## Decision

Sessions accumulate a lifetime token tally — input, output, and cached
input tokens — from the per-turn cost/usage reports harnesses already
emit. The lineage view's turn-info line prefers that tally over the
message count: a window whose token data is available reads as
"`<tokens> tok · <compute time>`" (tokens = input + output for the
window), falling back to "`N msgs`" only when no token data exists for
the window (harness never reports usage, or legacy records). Hovering a
turn-info line reveals a tooltip with the window's detail: message
count, input tokens, output tokens, and cached input tokens.

The tally rides the same checkpoint mechanism as message counts: the
parent's token tally is stamped onto a fork record at fork-out and onto
the merge record at merge-back, so each lineage window's tokens are
plain subtraction between checkpoints — no transcript fetch at render
time.

Token semantics: "input" is the total prompt-side token count including
cache reads; "cached" is the subset of input served from the provider's
prompt cache. A harness that reports only an unsplit total stores it as
input with zero output; clients present such a window's detail as a
single total rather than fabricating a split.

Like message counts, the tally self-heals: the daemon recounts it from
the persisted transcript's cost events at load, so sessions recorded
before the tally existed (or that lagged a crash) regain their totals
without migration.

## Reason

"N msgs" says how chatty a window was, not how much model work it
consumed. Token counts are the unit users reason about for cost,
context pressure, and cache effectiveness — and harnesses already
report them per turn, so the daemon can aggregate instead of every
client re-deriving them from transcripts. The fallback keeps the view
useful for harnesses (shells, harnesses without usage reporting) and
historical records where tokens are simply unknown.

## Consequences

- Adapters should surface per-turn token usage in their cost events
  whenever their harness exposes it, in every mode the adapter drives
  (interactive and headless alike). One turn's usage must be reported
  exactly once — an adapter reading a stream that repeats the same
  usage across records must dedupe before emitting.
- Fork-out and merge-back must stamp the parent's tally at the same
  instant they stamp the message count; a boundary that stamps one but
  not the other skews every later window's delta.
- Legacy records (zero-tally stamps) must keep rendering via the
  message-count fallback rather than showing zero tokens.
- The transcript recount at load must stay in sync with the live
  accumulation path — both count the same events the same way.

## Non-Goals

- Estimating token counts for harnesses that never report them (e.g.
  by character heuristics). An estimate next to real numbers is worse
  than the message-count fallback.
- Cost-in-dollars display in the lineage view; the tally is about
  volume, not price.

## Examples

- A fork branches after the parent consumed 120k tokens, and merges
  back when the parent is at 120k and the fork at 30k: the parent's
  pre-fork window shows "120.0k tok", the fork's lane shows "30.0k
  tok", each with its own compute time.
- Hovering a window's turn-info line shows e.g.
  "5 msgs · in 118.2k · out 1.8k · cached 96.4k".
- A codex window (unsplit total) hovers as "3 msgs · 2.3k tok total".
- A shell session's windows keep showing "N msgs" with wall-clock
  spans, exactly as before.
