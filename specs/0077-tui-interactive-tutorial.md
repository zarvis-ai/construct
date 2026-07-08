# 0077-tui-interactive-tutorial

Status: accepted
Date: 2026-07-08
Area: ux
Scope: an in-TUI guided tour that teaches the core keybindings and the program board to non-emacs users.

## Decision

construct ships an interactive tutorial ("tour"): a small floating coach-mark
card that tells the user what to do next and advances only when the user
actually does it. The tour observes the TUI's real chord/action dispatch and
the daemon's real event stream — it never renders a fake screen, never
blocks input, and never steals a keystroke or click from the live UI. Every
control the tour mentions is a real, currently-usable affordance: pressing
the suggested key does the real thing, and (with one exception) clicking the
key label in the card dispatches the exact same action a keypress would.

The exception is the tour's first step, which teaches the emacs-style
two-key chord notation itself (e.g. "C-x C-f means hold Ctrl, tap X, then
still holding Ctrl, tap F") and the universal `C-g` escape hatch. That step
requires real keystrokes — it is teaching fingers, not testing recognition —
so its key labels are click-only nudges rather than click-advance. Every
other step is fully clickable: a mouse-only user can complete the whole tour
without ever pressing a chord.

The tour covers, in order: how to read chord notation and back out with
`C-g`; creating a practice session; sending it a message; moving focus and
selection around the list/view; opening the program board, applying the
built-in "Tasks" template, and running it so the board's own rule delegates
the task to a live subagent; splitting the screen to watch the subagent
work; the two panic-button keys (help, quit); and cleaning up the practice
session. Each step's card shows live feedback — a keystroke echo, a
wrong-key correction, or a mini-checklist for steps with more than one
sub-condition — sourced from the same dispatch and event machinery the rest
of the TUI uses, not a scripted narration.

The tour never starts on its own. It has exactly two entry points: a line in
the empty-state welcome card, and a command-palette command. Starting the
tour while one is already active is a no-op. Starting it when sessions
already exist is allowed — the tour creates an additional practice session
rather than requiring an empty fleet.

When no agent harness is configured, the tour degrades gracefully instead of
stalling on a step it cannot complete: session creation and the first
message fall back to a shell session, the program-board step becomes
editing-only (open the board, apply the template, type a task line — no
run), and the split-screen step becomes a plain split-and-navigate exercise
with no subagent to watch. The card says so explicitly and points at
harness setup.

Completing the tour writes a persistent "done" marker; the welcome card's
tour line is only highlighted as an invitation while that marker is absent,
never auto-starting the tour on its account. The current step number is
persisted separately so a tour interrupted by quitting the TUI resumes at
the same step on the next launch instead of restarting from the beginning;
that persisted step is cleared as soon as the tour ends, however it ends.
Ending the tour early (before the last step) never writes the done marker —
only finishing the last step does — so an early exit still gets re-invited
next time.

The tour's chord labels are sourced from the TUI's real keymap for the
user's active profile (emacs or vim), never hardcoded to one profile's
notation. Where vim has its own idiomatic binding for an action the tour
references, the card shows that binding rather than the emacs one; where an
action has no distinct vim form, the shared binding is shown as-is.

## Reason

New users unfamiliar with emacs-style chord notation have no way to learn
what "C-x C-f" means or how the fleet-of-agents model (sessions, the
program board, subagent delegation) fits together, short of reading docs
outside the tool. A tour that only ever *describes* actions risks drifting
from reality as keybindings change, and a tour that *simulates* a fake
screen teaches muscle memory for a UI that doesn't actually respond that
way. Anchoring every step to the TUI's real dispatch path means the tour
can never show stale advice — if a chord changes, the tour's label changes
with it — and completion always means the user actually did the real
thing, not that they clicked past a slide.

## Consequences

- Any future change to a keybinding this tour references must keep (or
  deliberately update) the tour's label for it — the tour reads the live
  keymap, so a silent keybinding change silently changes what the tour
  teaches.
- The tour's advancement logic depends on the same action-dispatch and
  event-notification machinery every other TUI feature uses. It must not
  gain a private, parallel input path — that would reintroduce the
  simulation problem this design exists to avoid.
- Degraded mode (no agent harness) must keep working as harness detection
  or the onboarding/configure flow evolves — the tour is often the first
  thing a completely fresh install shows, so it cannot assume a harness is
  configured.
- The persisted resume-step and done-marker are per-machine local state, not
  synced anywhere; reinstalling or wiping local state resets the invitation.

## Non-Goals

- The tour is not a substitute for `?` help or written docs — it teaches a
  fixed, opinionated path through the core workflow once, not every
  feature.
- The tour does not attempt to teach every keybinding or every harness's
  behavior; it demonstrates one full example (create → message → move
  around → delegate via the program board → split-screen watch → clean up).
- The tour does not gate any other feature. Sessions, the program board, and
  delegation all work identically whether or not the tour has ever been
  run.

## Examples

- A user who has never touched emacs opens construct for the first time,
  sees the welcome card's highlighted tour line, and presses the suggested
  key. The card explains chord notation with a live micro-exercise before
  ever asking the user to create anything.
- A user quits mid-tour (say, partway through the program-board step). On
  the next launch, the tour resumes at that same step rather than restarting
  from the chord-notation lesson.
- A user on a machine with no agent harness installed runs the tour anyway;
  the program-board step becomes an editing-only exercise with a note
  pointing at harness setup, and the tour still reaches its final "tour
  complete" card.
- A mouse-only user runs the whole tour (after the first, keystroke-only
  step) purely by clicking the key labels the card shows, each of which
  performs the same action a keypress would.
