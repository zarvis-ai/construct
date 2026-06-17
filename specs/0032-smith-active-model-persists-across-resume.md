# 0032-smith-active-model-persists-across-resume

Status: accepted
Date: 2026-06-17
Area: harness
Scope: A smith model chosen mid-session via `/model` survives session restart and daemon restart, and the recorded session model tracks the switch.

## Decision

When a smith session switches model with the `/model` slash command, the
session's recorded model updates to match. Restarting the session (e.g. ctrl-d
then restart) or restarting the daemon brings the session back on the model it
was last running, not the model it was created with. The model shown for the
session also reflects the switch rather than the creation-time value.

The adapter reports the switch to the daemon as a dedicated state-change event
carrying a canonical model spec. The daemon is the source of truth for the
session's active model: it records the new spec on the session and, on the next
resume, re-injects it into the adapter's start parameters. The adapter resolves
its model from those start parameters, so it comes back on the recorded model.

The event is durable per-session state, not conversation: it is never written
to the transcript, mirroring how an in-adapter approval-mode change is reported
and persisted.

## Reason

A session's start parameters carry its model but are written once at create and
never updated. Every resume path — daemon restart and explicit session restart —
re-spawns the adapter from those frozen parameters. A `/model` switch only
changed in-adapter state, so it was silently lost on the next restart and the
session reverted to its creation-time model — surprising, since other session
state (history, approval mode) survives restart.

Routing the change through the daemon — rather than having the adapter persist a
private sidecar — keeps a single source of truth, lets the recorded/displayed
model track the switch, and reuses the existing adapter→daemon mechanism for
reporting an internal state change.

## Consequences

- The reported model spec must be re-resolvable by the adapter's model
  resolver. A named-endpoint profile is reported in its `@name:model` form so
  the profile's endpoint and key are recovered on resume; a bare
  `provider:model` would lose them.
- The reported spec pins the model that was actually running. If a profile's
  default model later changes in config, a resumed session that had pinned it
  keeps the model it was using rather than silently moving to the new default.
- On resume the daemon prefers the recorded session model over the frozen
  creation-time start parameter. A session that never switched model is
  unaffected (its recorded model still equals its creation-time model).
- The model-change report is durable state, never a transcript row, and is not
  re-broadcast as a transcript event — clients learn the new model from the
  updated session record. UIs should read the live model from the session
  record, not by replaying events.
- Persistence is best-effort at the daemon: if recording the new model fails,
  the running session continues on the new model and only a subsequent resume
  could fall back to the creation-time model.

## Non-Goals

- Persisting per-turn model overrides or any model history — only the current
  active model is retained.
- Changing how the creation-time model is chosen for a brand-new session.
- A model-change report from a harness that has no notion of switching models.
