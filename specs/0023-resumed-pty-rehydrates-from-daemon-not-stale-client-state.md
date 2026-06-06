# 0023-resumed-pty-rehydrates-from-daemon-not-stale-client-state

Status: accepted
Date: 2026-06-06
Area: tui
Scope: Applies to TUI reconnect after a daemon restart, and the daemon-side force-redraw on session respawn, for PTY harnesses that repaint on resume (codex, claude, shell).

## Decision

When the TUI reconnects to the daemon (which happens on every daemon restart), it discards its in-memory terminal histories and re-hydrates them from the daemon's `pty.log`. It does not carry its pre-restart screen state forward.

The daemon, on respawning a non-silent-resume PTY session, continues to force a redraw (a one-column bump+restore SIGWINCH). That force-redraw fires when the resumed child's PTY output has *settled* — produced output then gone quiet — rather than after a fixed delay, with a hard cap as a fallback.

## Reason

A daemon restart respawns every session and `truncate`s each session's `pty.log` so the resumed child renders into a clean slate (a resumed codex/claude/shell repaints on resume *without* a full screen clear — measured: zero `ESC[2J`/`ESC[3J`/cursor-home in codex's resume output). The daemon-side truncate keeps the daemon's own log clean, but the TUI kept its own pre-restart `ItemHistory`. Feeding the resumed child's clean-slate output on top of that stale grid/scroll-region/cursor state leaves the pane half-rendered — in practice blank until the user manually resizes (a manual SIGWINCH forces a full repaint). The TUI was effectively the one party that never honored the truncate.

Re-hydrating from `pty.log` on reconnect makes the client consistent with the daemon: the resumed child's output lands on an empty history, exactly as it lands on the freshly-truncated log. The full conversation renders on the first frame, with no manual resize.

The force-redraw remains as defense in depth (and for single-session restarts, where the client does not reconnect). It is settle-timed rather than fixed-delay because a resumed child's draw time varies; a fixed delay fired before the child had drawn and wasted the SIGWINCH.

## Consequences

Reconnect is treated as a fresh start for terminal rendering: histories are cleared and the selected/pinned PTY sessions re-hydrate from the daemon's authoritative log. A brief re-hydration flash on reconnect is acceptable.

Silent-resume harnesses (zarvis) keep their `pty.log` across respawn, so re-hydration simply reloads the full log — no special-casing needed. They are still skipped by the force-redraw.

## Non-Goals

This does not make the daemon understand harness-specific "ready" signals; output settling is a coarse, harness-agnostic proxy for "done drawing." It does not attempt perfect reconciliation of a hydration snapshot against concurrently-arriving live PTY bytes — the settle-timed force-redraw covers the narrow window where a snapshot is taken before the child draws.

## Examples

A codex session with a screenful of conversation is open when the daemon restarts. The daemon respawns codex, truncates the log, and codex repaints (no clear). The TUI reconnects, drops its stale history, re-hydrates from the now-clean log, and renders the full conversation on the first frame — no manual resize.
