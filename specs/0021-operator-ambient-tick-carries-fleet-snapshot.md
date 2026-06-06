# 0021-operator-ambient-tick-carries-fleet-snapshot

Status: accepted
Date: 2026-06-06
Area: harness
Scope: What the Operator's ambient loop tick observation contains.

## Decision

The ambient operator loop tick (see [0020](0020-operator-runs-ambiently.md)) carries a **live fleet snapshot** instead of a content-free prompt. At each tick the Operator adapter pulls the current session list from the daemon and folds in what changed since the previous tick, then injects that as the `OBSERVATION:` text:

- counts of active sessions by state (running / awaiting_input / errored),
- the sessions that **changed state** since the last tick (into errored / awaiting_input / done),
- sessions that **need attention** now (errored, or awaiting_input for a while),
- and a short instruction to inspect a stuck/errored/surprising session or reply `noted`.

If the daemon can't be reached, the tick falls back to the previous bare prompt.

## Reason

As shipped in 0020 the tick was content-free ("inspect only if useful … reply `noted`"). With no fleet data and a strong silence bias in the system prompt, the Operator had nothing concrete to react to and defaulted to `noted` almost every tick — observed live: when asked, the Operator reported *"only the repeated ambient operator loop tick observations … no errors, approvals, done events, or fleet alerts surfaced."* Data-carrying fleet-event observations only fire on sparse state transitions; between them the ambient loop produced no signal. Pushing a concrete snapshot (plus a per-tick delta) gives the model something real to notice — the "stale work / blockers / workflow issues" the ambient loop was meant to surface.

## Consequences

The tick now performs a cheap daemon round-trip (`session.list`) every interval; failures degrade to the old bare prompt. The Operator keeps prior-tick state in memory to compute deltas; this resets on adapter restart (the first tick after restart emits a baseline with no delta). Snapshot lists are capped to stay low-noise. Normal tool policy, approvals, and the existing rate limiting are unchanged.

## Non-Goals

Does not change the loop interval, the orchestrator-only gate, or the fleet-event observation pipeline. Does not persist snapshot state across restarts. Does not grant the ambient loop extra authority.

## Update (idle detection + selective previews)

The snapshot also flags **idle running** sessions — `Running` with no PTY byte
for `IDLE_RUNNING_MINS` (10m), detected from `last_pty_at_ms`. This closes a
blind spot: interactive `claude`/`codex`/`shell` sessions never emit
`AwaitingInput` (only their headless paths do) and the daemon doesn't infer
idle from quiescence, so the Operator otherwise can't tell a finished/waiting
session from a busy one. Idle running is the most common "needs attention" case
for those harnesses.

For the few most notable sessions (errored, then idle, then long-awaiting,
capped at `PREVIEW_SESSIONS`), the tick attaches a short ANSI-free preview — the
last few non-empty messages from the session's transcript tail — so the
Operator has concrete content to judge. Previews are deliberately selective to
keep the observation's input cost bounded on a large fleet.
