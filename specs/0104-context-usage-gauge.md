# 0104-context-usage-gauge

Status: accepted
Date: 2026-07-21
Area: ux
Scope: Sessions carry a live context-usage gauge (tokens in the model's context window), shown in the modeline next to the model name.

## Decision

Adapters report the session's current context usage whenever their
harness exposes it: the prompt-side token count of the most recent
model call (fresh input plus everything served from or written to the
prompt cache — i.e. what actually filled the window), and, when the
harness states it, the model's context-window size. The daemon keeps
the latest report on the session summary — a gauge, not a counter: new
reports replace old ones, a context reset clears it, and the last
report is recovered from the transcript at load.

The TUI modeline renders the gauge immediately after the model name:
"`(used/window %)`" when the window is known — e.g. `(12.4k/258k 5%)`
— and "`(used)`" when it isn't. The window is never guessed from model
names or hardcoded tables outside the harness's own report: showing
`(used)` honestly beats a percentage against an assumed denominator.

## Reason

Context pressure is the operational number users watch while driving
long sessions — it decides when to compact, fork, or start fresh — and
most harnesses already report it precisely. Message counts and token
totals (spec 0103) measure consumption over time; this gauge answers
the different question "how full is this conversation right now?".

## Consequences

- Adapters must report usage from the harness's own numbers, per
  model call (or per harness snapshot), and must not fabricate a
  window size the harness didn't state.
- The gauge must reset when the conversation's context resets
  (harness-native /clear and equivalents), not persist a stale value
  into the fresh conversation.
- Repeated identical snapshots must not spam the transcript; adapters
  report on change.
- Clients rendering the gauge must handle all three states: unknown
  (no report yet — render nothing), used-only, and used+window.

## Non-Goals

- Estimating context usage for harnesses that report nothing (e.g. a
  bare shell). No report → no gauge.
- Quota/rate-limit display — that is subscription state (spec 0086),
  not conversation state.

## Examples

- A codex session that just consumed 12,400 prompt tokens against its
  258k window shows `(12.4k/258k 5%)` after the model name.
- A kimi session (no window reported) shows `(74.2k)`.
- A brand-new session or a bare shell shows no gauge at all.
- After a harness-native `/clear`, the gauge disappears until the
  first call of the fresh conversation reports again.
