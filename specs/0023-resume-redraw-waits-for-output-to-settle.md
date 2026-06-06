# 0023-resume-redraw-waits-for-output-to-settle

Status: accepted
Date: 2026-06-06
Area: daemon
Scope: Applies to the post-resume force-redraw for PTY harnesses that do not support silent resume (codex, claude, shell).

## Decision

After resuming a PTY session whose harness cannot silently resume, the daemon forces a redraw by bumping the PTY width by one column and restoring it (two SIGWINCHes). That bump fires when the resumed child's PTY output has *settled* — it has produced output and then gone quiet for a short window — not after a fixed delay. A hard cap fires the bump regardless if the child never settles or never draws.

## Reason

These harnesses only repaint past content when their PTY receives a SIGWINCH. The child is respawned at the cached size, so a same-size `pty_resize` from a reconnecting TUI is a kernel no-op (`ioctl(TIOCSWINSZ)` signals only on an actual size change) and never triggers the repaint. The daemon must force the SIGWINCH itself.

A fixed delay is fragile: it has to be long enough for the slowest resume yet short enough not to leave the pane visibly blank. Codex resuming a large conversation takes longer to load and draw than the fixed 250 ms allowed, so the bump landed while only the banner/footer was on screen. The forced SIGWINCH repainted that partial frame, and because no further SIGWINCH followed, the conversation stayed blank until the user manually resized their terminal.

Settling on output is a direct signal that the child has finished its resume draw, so the SIGWINCH lands on a complete frame regardless of how long the resume took.

## Consequences

The daemon tracks each session's last PTY-output timestamp (it already does, for the "looks busy" signal). The resume-redraw task polls that timestamp and fires once output has been quiet for the settle window, or after the hard cap. The cap guarantees a redraw even for a child that streams continuously or emits nothing.

Harnesses that advertise `supports_silent_resume` (zarvis) are still skipped entirely — they deliberately paint nothing on resume and a forced SIGWINCH would double-paint.

## Non-Goals

This does not make the daemon understand harness-specific "ready" signals. Output settling is a coarse, harness-agnostic proxy for "done drawing," which is sufficient.

## Examples

A codex session with a screenful of conversation is open when the daemon restarts. Codex resumes, loads the rollout, and draws its input box; loading and drawing take ~1.5 s. The redraw task sees output stop, waits out the settle window, then bumps the width — codex repaints the full conversation. The user never sees a blank pane and never has to resize.
