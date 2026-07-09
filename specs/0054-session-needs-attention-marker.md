# 0054-session-needs-attention-marker

Status: accepted
Date: 2026-06-28
Area: ux
Scope: How the daemon detects that a session has stopped working and surfaces a sticky "needs you" marker the operator clears by looking.

## Decision

A session carries a persisted boolean "needs attention" marker, separate from
its run state. It is the operator's fleet-inbox signal: "this session is waiting
on you."

- **It is not a new run state.** The existing run states (`Running`,
  `AwaitingInput`, `Done`, `Errored`, …) are unchanged. "Needs you" is the
  derived predicate `state != Running`, captured as a sticky marker so it
  persists after the transition that produced it.
- **The daemon owns detection.** The marker is raised when a session leaves
  `Running` for a non-running state (`AwaitingInput`, `Done`, `Errored`) and
  the session is not the one the operator is currently focused on. It is
  cleared when the operator switches to (focuses) the session, and also when
  the session returns to `Running` (it is no longer waiting).
- **The stop must follow activity the operator hasn't seen.** A session going
  non-running only flags if genuine session activity (output, messages, tool
  calls, a terminal event) arrived *while the session was not focused* since the
  operator last looked. Activity in the focused session does not count — in
  particular the operator's own keystrokes echoing at a prompt. Otherwise:
  focus an idle session, type and clear it without submitting, switch away, and
  the echo-then-idle would falsely flag it. This "unseen activity since seen"
  signal is in-memory and reset whenever the operator views the session.
- **Every harness must reach an accurate non-running state.** Harnesses that
  emit structured lifecycle events already do. Interactive PTY harnesses, which
  otherwise sit in `Running` forever, get daemon-side detection via a hybrid:
  - **Line-oriented shells** — foreground process-group comparison: when the
    terminal's foreground process group is the shell's own group, the shell is
    at its prompt (awaiting input); when a launched command holds the
    foreground, it is running. This is exact and immediate.
  - **Full-screen TUI harnesses** (interactive coding assistants whose child
    holds the terminal's foreground group for its whole lifetime, so the
    process-group signal can't distinguish busy from idle) — an output
    quiescence timeout: no terminal output for a short fixed window means the
    session is awaiting input. Output resuming returns it to running.
- **Idle housekeeping is not activity.** Full-screen TUI harnesses repaint
  status-line housekeeping while sitting idle — e.g. a periodic auto-update
  check that paints a message and erases it a moment later. Byte-wise that is
  indistinguishable from real output; what distinguishes it is that it does
  not persist. For quiescence-detected harnesses, output counts as genuine
  activity — both for the unseen-activity signal and for undoing a
  quiescence-driven idle — only once a burst of output has kept arriving for
  a short window comparable to the quiescence window itself, where a burst is
  broken by any silence long enough to have triggered quiescence. Shorter
  blips leave the session's state and markers completely untouched; without
  this rule every idle, unfocused session re-raises its marker on each
  housekeeping repaint, showing the operator a dot with nothing new to see.
- **The marker is persisted** and survives daemon and client restarts. On
  restart, sessions that were waiting still show the marker; a reconnecting
  viewer re-asserts focus so the session it is looking at clears.

## Reason

Operators run many sessions in parallel and need to know which ones to attend
to. `AwaitingInput` alone is ambiguous — it conflates "finished, idle" with
"blocked, needs a decision" — and an inferred "unread output" flag is passive
(it says nobody looked, not that anybody must). A sticky marker driven off the
stop transition is high-precision: idle-but-seen sessions stay quiet, freshly
stopped ones light up, and the operator clears them simply by looking.

Detection must live in the daemon because it is the single source of truth all
clients read, and because the two interactive cases have genuinely different
best signals — process-group state is exact for shells but useless for a
full-screen TUI that never yields the foreground; quiescence is the only signal
left for the latter. The foreground-group probe needs the PTY master handle,
which lives in the adapter process, so that half is detected adapter-side and
reported as a normal state transition; the quiescence half is detected in the
daemon, which already tracks last-output time.

## Consequences

- Future harnesses that run a full-screen interactive child should rely on the
  quiescence path (or emit their own `AwaitingInput`); line-oriented shells
  should opt into foreground-group detection. A harness that holds the terminal
  foreground but is genuinely idle without emitting output will be flagged after
  the quiescence window — accepted; the marker targets backgrounded sessions and
  is suppressed for the focused one.
- The quiescence window is a fixed timeout, not a guess refined per harness. A
  long-thinking session that emits nothing for the whole window is briefly
  marked awaiting; output resuming clears it. Keep the window long enough to
  avoid flapping.
- The burst filter trades a small delay and a rare miss for precision: a
  quiescence-detected session reads as running only once its output has
  persisted past the blip window, and a genuine turn whose entire output fits
  inside the window never flags. Accepted — these harnesses repaint
  continuously (spinners, streaming) during real turns, so sub-window turns
  are vanishingly rare, while idle housekeeping blips arrive forever.
- A housekeeping message that paints and stays (e.g. "update available") does
  not flag either — a single repaint is not a sustained burst. Accepted: the
  marker signals a stop after work, not passive notices.
- The "focused session" used to suppress the marker is global to the daemon
  (last switch wins). With multiple simultaneous viewers this is approximate;
  single-operator use is exact. Don't build per-viewer marker state on top of
  this without revisiting the model.
- The marker is orthogonal to run state and to pinning/archival. It must not be
  repurposed as a state variant, and clients must treat it as advisory display,
  not control flow.

## Non-Goals

- No reason/category metadata on the marker (no "needs credential" vs "needs a
  decision"). It is a single boolean.
- No per-viewer or per-client marker state.
- No automatic clearing from anything other than focus or a return to running
  (e.g. it does not time out on its own).

## Examples

- A backgrounded coding session finishes its turn and goes idle → after it stops
  producing output it is marked; the operator sees the dot, switches to it, and
  the dot clears.
- A shell session's long build finishes and it returns to the prompt → marked
  immediately (foreground group is the shell again).
- The operator is actively viewing a session when it stops → no dot (the focused
  session is suppressed).
- The daemon restarts while three sessions were waiting → all three still show
  the dot; the session the operator reopens clears as the viewer re-asserts
  focus.
- An idle coding-assistant session paints "Checking for updates" in its status
  bar every 30 minutes and erases it half a second later → no state change, no
  dot. The same session streaming a real answer for several seconds and then
  stopping → dot.
